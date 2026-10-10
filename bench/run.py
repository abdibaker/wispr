#!/usr/bin/env python3
"""Reproducible STT and cleanup benchmark against the app's 9Router endpoint.

  python3 bench/run.py synth                      # (re)generate synthetic clips with espeak-ng
  python3 bench/run.py stt [--trials 3]           # STT models x vocabulary hints
  python3 bench/run.py cleanup [--trials 2]       # cleanup configurations vs raw baseline
  python3 bench/run.py latency LOG                # p50/p95 per stage from voice-prompt.log
  python3 bench/run.py e2e-report OUT...          # raw/cleaned accuracy and latency from e2e.sh

Reads endpoint from ~/.config/voice-prompt/settings.json and the key from the keyring
(or $VP_KEY). Writes JSON results under bench/results/. Stdlib only.
"""
import argparse, ctypes, http.client, json, os, pathlib, re, ssl, statistics, subprocess
import sys, tempfile, time, urllib.parse, uuid, wave

HERE = pathlib.Path(__file__).resolve().parent
ROOT = HERE.parent
CORPUS = json.loads((HERE / "corpus.json").read_text())
RESULTS = HERE / "results"

STT_MODELS = ["groq/whisper-large-v3-turbo", "groq/whisper-large-v3"]
# Prose clips whose transcripts must not gain vocabulary terms.
CONTAMINATION = ["Codex", "T3 Code", "Claude Code", "9Router", "TanStack", "Tailscale", "Coolify", "shadcn"]
# (model, reasoning_effort). "" omits the field, as the app does for "none".
CLEANUP_CONFIGS = [
    ("gpt-6-luna", "low"),  # current default
    ("gpt-6-luna", ""),
    ("gpt-6-luna", "medium"),
    ("groq/llama-3.3-70b-versatile", ""),
    ("groq/openai/gpt-oss-120b", "low"),
    ("groq/qwen/qwen3-32b", ""),
    ("glm-5.3-flash", ""),
    ("gemini-3.8-flash", ""),
    ("claude-haiku-5-5", ""),  # thinking off
    ("claude-haiku-5-5", "low"),
    ("claude-sonnet-5-5", ""),
]


def settings():
    return json.loads((pathlib.Path.home() / ".config/voice-prompt/settings.json").read_text())


def credential():
    key = os.environ.get("VP_KEY") or subprocess.run(
        ["secret-tool", "lookup", "service", "voice-prompt", "username", "9router-api-key"],
        capture_output=True, text=True, timeout=10).stdout.strip()
    if not key:
        sys.exit("no API key (keyring or $VP_KEY)")
    return key


class Client:
    """One keep-alive HTTPS connection, like the app's pooled reqwest client."""

    def __init__(self):
        self.url = urllib.parse.urlsplit(settings()["endpoint"])
        self.key = credential()
        self.conn = None
        self.pace = 0.0
        self.last = 0.0

    def reset(self):
        if self.conn:
            self.conn.close()
        self.conn = None

    def post(self, path, body, content_type, timeout=60):
        """Paced and retried on 429: Groq allows 20 requests/minute per Whisper model.
        `wall_ms` covers every attempt and wait, so retried rows are not reported as fast."""
        started = None
        for attempt in range(4):
            if self.pace:
                time.sleep(max(0, self.last + self.pace - time.monotonic()))
            started = started or time.perf_counter()  # pacing before the first attempt is ours, not the user's
            self.last = time.monotonic()
            out, parsed = self._post(path, body, content_type, timeout)
            if out.get("status") != 429:
                break
            print(f"  429, waiting 30 s (attempt {attempt + 1})", file=sys.stderr, flush=True)
            time.sleep(30)
        out["attempts"] = attempt + 1
        out["wall_ms"] = ms(started)
        return out, parsed

    def _post(self, path, body, content_type, timeout):
        cold = self.conn is None
        if cold:
            self.conn = http.client.HTTPSConnection(
                self.url.hostname, self.url.port, timeout=timeout, context=ssl.create_default_context())
        started = time.perf_counter()
        try:
            self.conn.request("POST", self.url.path.rstrip("/") + path, body,
                              {"Authorization": "Bearer " + self.key, "Content-Type": content_type})
            response = self.conn.getresponse()
            data = response.read()
        except (OSError, http.client.HTTPException) as error:
            self.reset()
            return {"error": type(error).__name__, "ms": ms(started), "cold": cold}, {}
        out = {"status": response.status, "ms": ms(started), "cold": cold}
        self.body = data
        try:
            parsed = json.loads(data)
        except ValueError:
            parsed = {}
        if response.status != 200:
            error = str(parsed.get("error", data[:200]))[:200].replace(self.key, "[redacted]")
            out["error"] = re.sub(r"org_[0-9a-z]+", "org_[redacted]", error)
        return out, parsed


def ms(since):
    return round((time.perf_counter() - since) * 1000)


# ---------- synthetic audio ----------

def espeak(text, path):
    lib = ctypes.CDLL("libespeak-ng.so.1")
    rate = lib.espeak_Initialize(2, 0, None, 0)  # AUDIO_OUTPUT_SYNCHRONOUS
    chunks = []
    callback_type = ctypes.CFUNCTYPE(ctypes.c_int, ctypes.POINTER(ctypes.c_short), ctypes.c_int, ctypes.c_void_p)
    callback = callback_type(lambda wav, n, _: chunks.append(ctypes.string_at(wav, n * 2)) or 0 if wav and n > 0 else 0)
    lib.espeak_SetSynthCallback(callback)
    lib.espeak_SetVoiceByName(b"en-us")
    lib.espeak_SetParameter(1, 160, 0)  # words per minute
    data = text.encode()
    lib.espeak_Synth(data, len(data) + 1, 0, 0, 0, 0, None, None)
    lib.espeak_Synchronize()
    with tempfile.NamedTemporaryFile(suffix=".wav") as raw:
        with wave.open(raw.name, "wb") as out:
            out.setnchannels(1), out.setsampwidth(2), out.setframerate(rate)
            out.writeframes(b"".join(chunks))
        # The app records 16 kHz mono; pad 300 ms of silence like a real push-to-talk clip.
        subprocess.run(["ffmpeg", "-v", "error", "-y", "-i", raw.name, "-af", "adelay=300,apad=pad_dur=0.3",
                        "-ar", "16000", "-ac", "1", str(path)], check=True)


def neural(text, voice, path):
    """Neural TTS (edge-tts through 9Router): closer to human speech than espeak, still synthetic."""
    client = Client()
    out, _ = client.post("/audio/speech", json.dumps({"model": f"edge-tts/{voice}", "input": text}).encode(),
                         "application/json")
    if out.get("status") != 200:
        sys.exit(f"TTS failed: {out}")
    with tempfile.NamedTemporaryFile(suffix=".mp3") as mp3:
        mp3.write(client.body)
        mp3.flush()
        subprocess.run(["ffmpeg", "-v", "error", "-y", "-i", mp3.name, "-af", "adelay=300,apad=pad_dur=0.3",
                        "-ar", "16000", "-ac", "1", str(path)], check=True)

def synth(_args):
    for case in CORPUS["stt"]:
        path = audio_path(case)
        if "speak" in case:
            espeak(case["speak"], path)
        elif "voice" in case:
            if path.exists():
                continue
            neural(case["say"], case["voice"], path)
        else:
            continue
        print("wrote", path.relative_to(ROOT))


def audio_path(case):
    return HERE / (case.get("audio") or f"audio/{case['id']}.wav")


# ---------- scoring ----------

def words(text):
    text = text.lower().replace("'", "")
    return re.findall(r"[a-z0-9]+", text)


def wer(reference, hypothesis):
    r, h = words(reference), words(hypothesis)
    row = list(range(len(h) + 1))
    for i in range(1, len(r) + 1):
        prev, row[0] = row[0], i
        for j in range(1, len(h) + 1):
            prev, row[j] = row[j], min(row[j] + 1, row[j - 1] + 1, prev + (r[i - 1] != h[j - 1]))
    return row[len(h)] / max(len(r), 1)


def contains(text, term, case=True):
    """Whole-token match: `15` does not match `150`, `whisper-large-v3` not `...-v3-turbo`."""
    flags = 0 if case else re.IGNORECASE
    # A number may take a hyphenated unit ("250-millisecond"); an identifier may not grow ("-turbo").
    after = r"(?!\w|[./]\w)" if term.isdigit() else r"(?![\w-]|[./]\w)"
    return re.search(r"(?<![\w./-])" + re.escape(term) + after, text, flags) is not None


def term_hits(terms, text):
    """Exact (case-sensitive) spelling of each technical term."""
    return [t for t in terms if contains(text, t)]


def pct(values, p):
    values = sorted(values)
    if not values:
        return None
    return values[min(len(values) - 1, max(0, round(p / 100 * len(values) + 0.5) - 1))]


def summary(values):
    return {"n": len(values), "p50": pct(values, 50), "p95": pct(values, 95),
            "mean": round(statistics.mean(values)) if values else None}


# ---------- STT ----------

def multipart(audio, fields):
    boundary = uuid.uuid4().hex
    parts = [f'--{boundary}\r\nContent-Disposition: form-data; name="{k}"\r\n\r\n{v}\r\n'.encode()
             for k, v in fields.items()]
    parts.append(f'--{boundary}\r\nContent-Disposition: form-data; name="file"; filename="audio.wav"\r\n'
                 f'Content-Type: audio/wav\r\n\r\n'.encode() + audio + b"\r\n")
    parts.append(f"--{boundary}--\r\n".encode())
    return b"".join(parts), "multipart/form-data; boundary=" + boundary


def hint(vocabulary):
    """Mirror of settings::vocabulary_hint (400-character budget, first terms win)."""
    out, seen = "", set()
    for term in (t.strip() for t in vocabulary):
        if not term or term.lower() in seen:
            continue
        seen.add(term.lower())
        candidate = term if not out else out + ", " + term
        if len(candidate) > 400:
            break
        out = candidate
    return out


def stt(args):
    client = Client()
    client.pace = args.pace
    hints = {"none": "", "default": hint(CORPUS["vocabulary"]["default"]),
             "extended": hint(CORPUS["vocabulary"]["extended"])}
    # "contextual": the app sends the extended hint only into technical windows (editor,
    # terminal, file in the title); prose clips stand for dictation elsewhere and get none.
    records = []
    for trial in range(args.trials):
        for case in CORPUS["stt"]:
            audio = audio_path(case).read_bytes()
            for model in args.models:
                for hint_name in args.hints:
                    fields = {"model": model, "response_format": "json", "language": "en"}
                    text_hint = (hints["extended"] if case["id"].startswith("tech") else "") \
                        if hint_name == "contextual" else hints[hint_name]
                    if text_hint:
                        fields["prompt"] = text_hint
                    if args.cold:
                        client.reset()
                    receipt, parsed = client.post("/audio/transcriptions", *multipart(audio, fields))
                    text = (parsed.get("text") or "").strip()
                    record = {"trial": trial, "case": case["id"], "synthetic": case["synthetic"], "model": model,
                              "hint": hint_name, **receipt, "text": text}
                    if "error" not in receipt:
                        record["wer"] = round(wer(case["reference"], text), 3)
                        if not case["id"].startswith("tech"):
                            record["contaminated"] = [t for t in CONTAMINATION if contains(text, t, case=False)]
                        record["terms"] = f"{len(term_hits(case['terms'], text))}/{len(case['terms'])}"
                    records.append(record)
                    print(json.dumps(record), flush=True)
    write("stt", records)
    report_stt(records)


def report_stt(records):
    ok = [r for r in records if "error" not in r]
    print(f"\n{'model':32} {'hint':10} {'n':>3} {'p50':>6} {'p95':>6} {'WER real':>9} {'WER synth':>9} {'terms':>7} {'errors':>6} {'contam':>6}")
    for model in dict.fromkeys(r["model"] for r in records):
        for hint_name in sorted({r["hint"] for r in records}):
            group = [r for r in ok if r["model"] == model and r["hint"] == hint_name]
            errors = sum(1 for r in records if r["model"] == model and r["hint"] == hint_name and "error" in r)
            if not group:
                continue
            real = [r["wer"] for r in group if not r["synthetic"]]
            synth_ = [r["wer"] for r in group if r["synthetic"]]
            hit = sum(int(r["terms"].split("/")[0]) for r in group)
            total = sum(int(r["terms"].split("/")[1]) for r in group)
            lat = summary([r["ms"] for r in group])
            contaminated = sum(1 for r in group if r.get("contaminated"))
            print(f"{model:32} {hint_name:10} {lat['n']:>3} {lat['p50']:>6} {lat['p95']:>6} "
                  f"{statistics.mean(real) if real else 0:>9.3f} {statistics.mean(synth_) if synth_ else 0:>9.3f} "
                  f"{hit:>3}/{total:<3} {errors:>6} {contaminated:>6}")


# ---------- cleanup ----------

def system_prompt(vocabulary, extra=""):
    """Mirror of OpenAiCompatible::clean's system message."""
    source = (ROOT / "src-tauri/src/providers.rs").read_text()
    prompt = source.split('pub const CLEANUP_SYSTEM_PROMPT: &str = "', 1)[1].split('";', 1)[0]
    prompt = prompt.replace("\\\n", "").replace('\\"', '"')
    if vocabulary:
        prompt += "\nVocabulary: " + ", ".join(vocabulary)
    if extra.strip():
        prompt += "\nAdditional user preferences: " + extra.strip()
    return prompt


def squash(text):
    return "".join(c for c in text.lower() if c.isalnum())


def distance(a, b):
    row = list(range(len(b) + 1))
    for i, x in enumerate(a, 1):
        prev, row[0] = row[0], i
        for j, y in enumerate(b, 1):
            prev, row[j] = row[j], min(row[j] + 1, row[j - 1] + 1, prev + (x != y))
    return row[len(b)]


def relevant(vocabulary, transcript, limit=24):
    """Mirror of settings::relevant_terms."""
    words = [w for w in (squash(t) for t in transcript.split()) if w and w not in ("dot", "slash", "dash", "underscore")]
    out, seen = [], set()
    for term in (t.strip() for t in vocabulary):
        key = squash(term)
        if not key or term.lower() in seen:
            continue
        seen.add(term.lower())
        for start in range(len(words)):
            run, hit = "", False
            for joined, word in enumerate(words[start:start + 4]):
                run += word
                n = len(key)
                allowed = n // 4 if n >= 8 else 1 if n >= 5 and joined else 0
                if abs(len(run) - n) <= allowed and distance(run, key) <= allowed:
                    hit = True
                    break
            if hit:
                out.append(term)
                break
    return out[:limit]


def judge(case, output):
    """Deterministic checks; returns a list of failures (empty = preserved)."""
    if output is None:
        return ["no output"]
    lower = output.lower()
    failures = []
    for term in case.get("exact", []):
        if not contains(output, term):
            failures.append(f"missing exact {term!r}")
    for options in case.get("all", []):
        if not any(contains(output, o, case=False) for o in options):
            failures.append(f"missing {options[0]!r}")
    for banned in case.get("none", []):
        if re.search(r"(?<![a-z0-9])" + re.escape(banned.strip().lower()) + r"(?![a-z0-9])", lower):
            failures.append(f"kept {banned.strip()!r}")
    ratio = len(output) / max(len(case["raw"]), 1)
    if ratio > case.get("max_ratio", 1.6):
        failures.append(f"grew x{ratio:.1f} (answered or expanded?)")
    if ratio < case.get("min_ratio", 0.3):
        failures.append(f"shrank x{ratio:.1f} (summarized?)")
    return failures


def cleanup(args):
    client = Client()
    client.pace = args.pace
    # --app-instructions appends the stored cleanup_instructions, as the app does.
    extra = settings().get("cleanup_instructions", "") if args.app_instructions else ""
    vocabulary = CORPUS["vocabulary"]["extended"]
    prompt_for = (lambda case: system_prompt(CORPUS["vocabulary"]["default"], extra)) if args.vocabulary == "global" \
        else (lambda case: system_prompt(relevant(vocabulary, case["raw"]), extra))
    records = []
    for case in CORPUS["cleanup"]:  # raw/no-cleanup baseline: 0 ms, judged on the raw text
        records.append({"config": "raw (no cleanup)", "trial": 0, "case": case["id"], "ms": 0,
                        "output": case["raw"], "failures": judge(case, case["raw"])})
    configs = [c for c in CLEANUP_CONFIGS if not args.only or c[0] in args.only]
    for trial in range(args.trials):
        for model, effort in configs:
            name = f"{model} effort={effort or 'omitted'} vocab={args.vocabulary}"
            for case in CORPUS["cleanup"]:
                body = {"model": model, "stream": False, "messages": [
                    {"role": "system", "content": prompt_for(case)},
                    {"role": "user", "content": f"<transcript>\n{case['raw']}\n</transcript>"}]}
                if effort:
                    body["reasoning_effort"] = effort
                receipt, parsed = client.post("/chat/completions", json.dumps(body).encode(), "application/json")
                choice = (parsed.get("choices") or [{}])[0]
                output = (choice.get("message") or {}).get("content")
                finish = choice.get("finish_reason")
                failures = judge(case, output) if "error" not in receipt else [receipt["error"]]
                if finish == "length":
                    failures.append("finish_reason=length")
                record = {"config": name, "trial": trial, "case": case["id"], **receipt, "finish": finish,
                          "output": output, "failures": failures,
                          "usage": parsed.get("usage")}
                records.append(record)
                print(json.dumps({k: v for k, v in record.items() if k != "usage"}), flush=True)
    write("cleanup", records)
    report_cleanup(records)


def report_cleanup(records):
    print(f"\n{'config':48} {'n':>3} {'p50':>6} {'p95':>6} {'pass':>7} {'errors':>6}  failures")
    for name in dict.fromkeys(r["config"] for r in records):
        group = [r for r in records if r["config"] == name]
        errors = sum(1 for r in group if "error" in r)
        # Wall time across retries; failed attempts still cost the user that time.
        lat = summary([r.get("wall_ms", r["ms"]) for r in group])
        passed = sum(1 for r in group if not r["failures"])
        failed = sorted({f"{r['case']}: {'; '.join(r['failures'])}" for r in group if r["failures"]})
        print(f"{name:48} {lat['n']:>3} {lat['p50'] or 0:>6} {lat['p95'] or 0:>6} {passed:>3}/{len(group):<3} {errors:>6}  "
              + " | ".join(failed)[:400])


# ---------- app log ----------

def latency(args):
    rows, labels = [], []
    for line in pathlib.Path(args.log).read_text().splitlines():
        if " latency total=" in line:
            fields = line.split(" latency ", 1)[1]
            # first_audio=na means no chunk arrived: absent, not zero.
            rows.append({k: int(v) for k, v in re.findall(r"(\w+)=(\d+)\b", fields)})
            labels.append(dict(re.findall(r"(outcome|delivery|destination|stt_mode|fallback_reason)=(\S+)", fields)))
    if not rows:
        sys.exit("no new-format latency lines")
    for key in ("outcome", "delivery", "destination", "stt_mode", "fallback_reason"):
        counts = {}
        for label in labels:
            counts[label.get(key, "unlogged")] = counts.get(label.get(key, "unlogged"), 0) + 1
        print(f"{key}: " + ", ".join(f"{k}={v}" for k, v in sorted(counts.items())))
    # Only successful cleanups describe cleanup latency; fallbacks are counted above.
    cleaned = [r for r, label in zip(rows, labels) if label.get("outcome", "cleaned") == "cleaned"]
    if len(cleaned) != len(rows):
        print(f"stage timings below use the {len(cleaned)} cleaned dictations of {len(rows)}")
        rows = cleaned
    cold = [r for r in rows if r.get("idle", 0) > 90_000]
    warm = [r for r in rows if r.get("idle", 0) <= 90_000]
    for label, group in (("all", rows), ("cold (idle > 90 s)", cold), ("warm", warm)):
        if not group:
            continue
        print(f"\n{label}: {len(group)} dictations")
        for key in group[0]:
            values = [r[key] for r in group if key in r]
            s = summary(values)
            print(f"  {key:15} p50 {s['p50']:>6}  p95 {s['p95']:>6}  mean {s['mean']:>6}")


def alignment(reference, hypothesis):
    """Word-level substitutions, deletions and insertions (Levenshtein backtrace)."""
    r, h = words(reference), words(hypothesis)
    d = [[0] * (len(h) + 1) for _ in range(len(r) + 1)]
    for i in range(len(r) + 1):
        d[i][0] = i
    for j in range(len(h) + 1):
        d[0][j] = j
    for i in range(1, len(r) + 1):
        for j in range(1, len(h) + 1):
            d[i][j] = min(d[i - 1][j] + 1, d[i][j - 1] + 1, d[i - 1][j - 1] + (r[i - 1] != h[j - 1]))
    i, j, subs, dels, ins = len(r), len(h), [], [], []
    while i or j:
        if i and j and d[i][j] == d[i - 1][j - 1] + (r[i - 1] != h[j - 1]):
            if r[i - 1] != h[j - 1]:
                subs.append((r[i - 1], h[j - 1]))
            i, j = i - 1, j - 1
        elif i and d[i][j] == d[i - 1][j] + 1:
            dels.append(r[i - 1])
            i -= 1
        else:
            ins.append(h[j - 1])
            j -= 1
    return subs, dels, ins

def e2e_report(args):
    """Raw and cleaned accuracy plus stage latency for each saved `e2e.sh` run.

    Raw transcripts are scored before cleanup, so Haiku cannot hide STT errors. Cancelled
    presses shift the order, so each transcript is paired with its closest reference."""
    for output in args.outputs:
        cases = [c for c in CORPUS["stt"] if audio_path(c).exists()]
        text = pathlib.Path(output).read_text()
        print(f"\n=== {output}")
        raws = [json.loads(l[4:]) for l in text.splitlines() if l.startswith("raw ")]
        # Only the clips this run played; a truncated transcript must not match a shorter clip.
        played = next((l.split()[1:] for l in text.splitlines() if l.startswith("clips ")), None)
        if played:
            cases = [c for c in CORPUS["stt"] if str(audio_path(c).relative_to(HERE)) in played]
        groups = {}
        errors = {}
        by_path = {str(audio_path(c).relative_to(HERE)): c for c in cases}
        # With no cancelled presses the history is in play order; otherwise pair by closest text.
        positional = played and len(raws) % len(played) == 0 and "outcome=cancelled" not in text
        for index, entry in enumerate(raws):
            if positional:
                case = by_path[played[index % len(played)]]
            else:
                # Normalized by the longer side: plain WER favours long references for bad transcripts.
                case = min(cases, key=lambda c: wer(c["reference"], entry["raw"]) * len(words(c["reference"]))
                           / max(len(words(c["reference"])), len(words(entry["raw"])), 1))
            kind = "real" if not case["synthetic"] else ("neural" if "voice" in case else "espeak")
            g = groups.setdefault((entry["stt_model"], kind), {"wer": [], "hit": 0, "terms": 0, "clean_hit": 0,
                                                              "subs": 0, "dels": 0, "ins": 0, "numbers": [0, 0]})
            g["wer"].append(wer(case["reference"], entry["raw"]))
            g["hit"] += len(term_hits(case["terms"], entry["raw"]))
            g["clean_hit"] += len(term_hits(case["terms"], entry.get("cleaned") or entry["raw"]))
            g["terms"] += len(case["terms"])
            subs, dels, ins = alignment(case["reference"], entry["raw"])
            g["subs"] += len(subs); g["dels"] += len(dels); g["ins"] += len(ins)
            for term in case["terms"]:
                if term.isdigit():
                    g["numbers"][1] += 1
                    g["numbers"][0] += contains(entry["raw"], term)
                elif not contains(entry["raw"], term):
                    errors.setdefault(entry["stt_model"], {}).setdefault(term, set()).add(
                        " ".join(h for _, h in subs[:3]) or "(omitted)")
        print(f"{'model':34} {'kind':7} {'n':>3} {'WER':>6} {'raw terms':>10} {'clean terms':>12} {'numbers':>8} {'sub/del/ins':>12}")
        for (model, kind), g in sorted(groups.items()):
            print(f"{model:34} {kind:7} {len(g['wer']):>3} {statistics.mean(g['wer']):>6.3f} "
                  f"{g['hit']:>4}/{g['terms']:<5} {g['clean_hit']:>6}/{g['terms']:<5} "
                  f"{g['numbers'][0]:>3}/{g['numbers'][1]:<4} {g['subs']:>4}/{g['dels']}/{g['ins']}")
        for model, missed in errors.items():
            print(f"  missed by {model}: " + "; ".join(f"{t}" for t in sorted(missed)))
        rows = [dict(re.findall(r"(\w+)=(\d+)\b", l.split(" latency ", 1)[1])) for l in text.splitlines() if " latency total=" in l]
        modes = re.findall(r"stt_mode=(\S+) fallback_reason=(\S+)", text)
        counts = {}
        for mode in modes:
            counts[mode] = counts.get(mode, 0) + 1
        print("modes: " + ", ".join(f"{m}/{r}={n}" for (m, r), n in sorted(counts.items())))
        def span(name, a, b=None):
            values = [int(r[a]) - (int(r[b]) if b else 0) for r in rows if a in r and (b is None or b in r)]
            if values:
                s = summary(values)
                print(f"  {name:28} p50 {s['p50']:>6}  p95 {s['p95']:>6}  n {s['n']}")
        span("press -> provider ready", "ready")
        span("press -> first partial", "first_partial")
        span("first audio sent -> partial", "first_partial", "first_sent")
        span("release -> final STT", "release_to_stt")
        span("release -> cleaned text", "release_to_cleaned")
        span("release -> delivery", "total")
        span("cleanup", "cleanup_total")
        for line in text.splitlines():
            if line.startswith("resources"):
                print("  " + line)

def write(kind, records):
    RESULTS.mkdir(exist_ok=True)
    path = RESULTS / f"{kind}-{time.strftime('%Y%m%d-%H%M%S')}.json"
    path.write_text(json.dumps(records, indent=1))
    print("results:", path.relative_to(ROOT))


def main():
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("synth").set_defaults(run=synth)
    p = sub.add_parser("stt")
    p.add_argument("--trials", type=int, default=3)
    p.add_argument("--hints", nargs="+", default=["none", "default", "extended", "contextual"])
    p.add_argument("--models", nargs="+", default=STT_MODELS)
    p.add_argument("--cold", action="store_true", help="new connection per request")
    p.add_argument("--pace", type=float, default=3.5, help="seconds between requests (rate limit)")
    p.set_defaults(run=stt)
    p = sub.add_parser("cleanup")
    p.add_argument("--trials", type=int, default=2)
    p.add_argument("--only", nargs="*", help="model names to include")
    p.add_argument("--pace", type=float, default=0.0, help="seconds between requests (rate limit)")
    p.add_argument("--app-instructions", action="store_true", help="append the stored cleanup_instructions")
    p.add_argument("--vocabulary", choices=["global", "relevant"], default="global",
                   help="global: the default list for every case (old app); relevant: only mentioned terms (app now)")
    p.set_defaults(run=cleanup)
    p = sub.add_parser("e2e-report")
    p.add_argument("outputs", nargs="+", help="saved e2e.sh outputs")
    p.set_defaults(run=e2e_report)
    p = sub.add_parser("latency")
    p.add_argument("log", nargs="?", default=str(pathlib.Path.home() / ".local/share/voice-prompt/voice-prompt.log"))
    p.set_defaults(run=latency)
    args = parser.parse_args()
    args.run(args)


if __name__ == "__main__":
    main()
