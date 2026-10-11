#!/usr/bin/env python3
"""Reproducible STT and cleanup benchmarks. See bench/README.md.

  python3 bench/run.py cleanup [--suite smoke]    # stored transcripts -> cleanup -> evaluator
  python3 bench/run.py stt-live [--provider muse] # fixed audio -> live STT, no cleanup
  python3 bench/run.py record                     # record the author's core STT clips
  python3 bench/run.py synth                      # (re)generate synthetic clips
  python3 bench/run.py stt [--trials 3]           # batch Whisper models x vocabulary hints
  python3 bench/run.py latency LOG                # p50/p95 per stage from voice-prompt.log
  python3 bench/run.py e2e-report OUT...          # raw/cleaned accuracy and latency from e2e.sh

Reads endpoint from ~/.config/voice-prompt/settings.json and the key from the keyring
(or $VP_KEY). Writes JSON results under bench/results/. Stdlib only.
"""
import argparse, concurrent.futures, ctypes, functools, hashlib, http.client, json, os, pathlib, re, signal, ssl, statistics
import subprocess, sys, tempfile, threading, time, urllib.parse, uuid, wave
from evaluate import evaluate, has_span

HERE = pathlib.Path(__file__).resolve().parent
ROOT = HERE.parent
CORPUS = json.loads((HERE / "corpus.json").read_text())
RESULTS = HERE / "results"

STT_MODELS = ["groq/whisper-large-v3-turbo", "groq/whisper-large-v3"]
# Prose clips whose transcripts must not gain vocabulary terms.
CONTAMINATION = ["Codex", "T3 Code", "Claude Code", "9Router", "TanStack", "Tailscale", "Coolify", "shadcn"]
GROQ_URL = "https://api.groq.com/openai/v1"
# 9Router otherwise prepends its own style prompt to the system message.
ROUTER_HEADERS = {"x-9router-token-saver": "off"}
# 9Router adds a fixed buffer to reported prompt tokens; subtracted so paths compare.
ROUTER_PROMPT_PAD = 2000

def settings():
    return json.loads((pathlib.Path.home() / ".config/voice-prompt/settings.json").read_text())


def credential(account="9router-api-key", env="VP_KEY"):
    key = os.environ.get(env) or subprocess.run(
        ["secret-tool", "lookup", "service", "voice-prompt", "username", account],
        capture_output=True, text=True, timeout=10).stdout.strip()
    if not key:
        sys.exit(f"no {account} (keyring or ${env})")
    return key


class Client:
    """One keep-alive HTTPS connection, like the app's pooled reqwest client."""

    def __init__(self, url=None, key=None, headers=None):
        self.url = urllib.parse.urlsplit(url or settings()["endpoint"])
        self.key = key or credential()
        self.headers = headers or {}
        self.conn = None
        self.pace = 0.0
        self.last = 0.0

    def reset(self):
        if self.conn:
            self.conn.close()
        self.conn = None

    def post(self, path, body, content_type, timeout=60, tries=4):
        """Paced and retried on 429: Groq allows 20 requests/minute per Whisper model.
        `wall_ms` covers every attempt and wait, so retried rows are not reported as fast."""
        started = None
        for attempt in range(tries):
            if self.pace:
                time.sleep(max(0, self.last + self.pace - time.monotonic()))
            started = started or time.perf_counter()  # pacing before the first attempt is ours, not the user's
            self.last = time.monotonic()
            out, parsed = self._post(path, body, content_type, timeout)
            if out.get("status") != 429 or attempt + 1 == tries:
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
                              {"Authorization": "Bearer " + self.key, "Content-Type": content_type, **self.headers})
            response = self.conn.getresponse()
            first_byte = ms(started)
            data = response.read()
        except (OSError, http.client.HTTPException) as error:
            self.reset()
            return {"error": type(error).__name__, "ms": ms(started), "cold": cold}, {}
        out = {"status": response.status, "ms": ms(started), "ttfb_ms": first_byte, "cold": cold}
        if response.status == 429:
            out["retry_after"] = response.getheader("retry-after")
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
    if not any(c.isalnum() for c in term):  # punctuation such as "?"
        return term in text
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
        for case in [c for c in CORPUS["stt"] if audio_path(c).exists()]:
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


def stt_cases(args):
    cases = [c for c in CORPUS["stt"] if (args.kind is None or c["kind"] in args.kind)
             and (not args.core or c.get("core")) and (not args.case or c["id"] in args.case)]
    missing = [c["id"] for c in cases if not audio_path(c).exists()]
    if missing:
        print(f"skipping {len(missing)} unrecorded clips (run.py record): {' '.join(missing)}", file=sys.stderr)
    cases = [c for c in cases if audio_path(c).exists()]
    cut = [c["id"] for c in cases if c["kind"] == "author" and cut_off(audio_path(c))]
    if cut:
        print(f"ends mid-speech, re-record with `run.py record --again --case {' '.join(cut)}`", file=sys.stderr)
    return cases


COMMANDS = {"git status", "pnpm check", "pnpm install", "cargo test"}

def term_kind(term):
    """Scoring category of a reference term; a term may be only one."""
    if term in COMMANDS:
        return "command"
    if "/" in term or re.fullmatch(r"[\w-]+\.(rs|toml|lock|json|ts|tsx)", term):
        return "path"
    if term in ("9Router", "T3 Code"):
        return "term"
    if re.search(r"[A-Za-z]{2}.*\d|\d.*[A-Za-z]{2,}", term) and not re.fullmatch(r"\d+(\.\d+)?\s*\w{1,3}", term) \
            or term in ("GPT-OSS", "Scribe v2 Realtime", "Whisper large-v3", "Nova-3"):
        return "model"
    if any(ch.isdigit() for ch in term) or term in ("three retries",):
        return "number"
    return "term"

def command_shape(term, text):
    """How a command or path came out: exact, case (only letter case differs), spoken
    (same characters, other separators: "src tauri slash", "Cargotoml"), partial (some
    parts missing or replaced), missing (no part survives)."""
    if contains(text, term):
        return "exact"
    if contains(text, term, case=False):
        return "case"
    parts = [p for p in re.split(r"[^a-z0-9]+", term.lower()) if p]
    heard = squash(text.replace("slash", "").replace(" dot ", ""))
    if squash(term) in heard:
        return "spoken"
    got = [p for p in parts if p in words(text) or p in heard]
    return "partial" if got else "missing"

def stt_live(args):
    """Fixed audio -> live STT through the app's own streaming code (examples/stt.rs), at real
    time, no cleanup. Latency: `first_partial` from the first audio sent, `release_to_final`
    from the last chunk to the provider's confirmed final transcript."""
    cases = stt_cases(args)
    if not cases:
        sys.exit("no recorded clips match")
    binary = ROOT / "src-tauri/target/debug/examples/stt"
    if not binary.exists():
        sys.exit("build it first: cd src-tauri && cargo build --example stt")
    jobs = []
    for trial in range(args.trials):
        for case in cases:
            # The app sends keyterms only into technical windows; the core corpus is mixed.
            terms = CORPUS["vocabulary"]["extended"] if args.keyterms and case["terms"] else []
            jobs.append({"id": f"{case['id']}#{trial}", "audio": str(audio_path(case)), "keyterms": terms})
    env = dict(os.environ, STT_LANGUAGE=args.language or settings().get("language", "en"),
               SCRIBE_CHUNK_MS=str(args.chunk_ms))
    out = subprocess.run([str(binary), args.provider, str(args.concurrency)], input=json.dumps(jobs),
                         capture_output=True, text=True, timeout=900, env=env)
    by_id = {c["id"]: c for c in cases}
    label = args.label or "-".join([args.provider] + (["keyterms"] if args.keyterms else []))
    records = []
    for line in out.stdout.splitlines():
        r = json.loads(line)
        case = by_id[r["id"].split("#")[0]]
        r.update(case=case["id"], kind=case["kind"], provider=label, language=env["STT_LANGUAGE"],
                 chunk_ms=args.chunk_ms if args.provider == "scribe" else None)
        if r.get("text") is not None:
            r["wer"] = round(wer(case["reference"], r["text"]), 3)
            # Accuracy: the term was heard ("three" counts for "3"). Format: written exactly.
            r["missed"] = [t for t in case["terms"] if not contains(r["text"], t, case=False)
                           and not has_span(r["text"], t)]
            r["misformatted"] = [t for t in case["terms"] if not contains(r["text"], t)]
            r["shapes"] = {t: command_shape(t, r["text"]) for t in case["terms"]
                           if term_kind(t) in ("command", "path")}
            subs, dels, ins = alignment(case["reference"], r["text"])
            r["omitted"] = dels
        records.append(r)
    if out.returncode:
        print(out.stderr[-2000:], file=sys.stderr)
    write(f"stt-{label}", records)
    report_stt_live(records, cases)

def report_stt_live(records, cases):
    """Real voice and synthetic speech never share a row. Per category, `heard/exact/total`:
    heard ignores case and number spelling, exact is the written form."""
    kinds = ("term", "command", "path", "model", "number")
    print(f"\n{'config':18} {'kind':8} {'n':>3} {'WER':>6} {'omit':>4} "
          + " ".join(f"{k:>10}" for k in kinds)
          + f" {'ready50/95':>11} {'part50/95':>11} {'final50/95':>11} {'fail':>4}")
    for kind in ("author", "librivox", "neural", "espeak"):
        group = [r for r in records if r["kind"] == kind]
        if not group:
            continue
        ok = [r for r in group if r.get("text") is not None]
        by_case = {c["id"]: c for c in cases}
        cells = []
        for category in kinds:
            total = sum(1 for r in ok for t in by_case[r["case"]]["terms"] if term_kind(t) == category)
            heard = total - sum(1 for r in ok for t in r["missed"] if term_kind(t) == category)
            exact = total - sum(1 for r in ok for t in r["misformatted"] if term_kind(t) == category)
            cells.append(f"{heard}/{exact}/{total}" if total else "-")
        def p(key):
            s = summary([r[key] for r in ok if r.get(key) is not None])
            return f"{s['p50'] or '-'}/{s['p95'] or '-'}"
        print(f"{group[0]['provider']:18} {kind:8} {len(group):>3} "
              f"{statistics.mean(r['wer'] for r in ok) if ok else 0:>6.3f} {sum(len(r['omitted']) for r in ok):>4} "
              + " ".join(f"{c:>10}" for c in cells)
              + f" {p('ready_ms'):>11} {p('first_partial_ms'):>11} {p('release_to_final_ms'):>11} {len(group) - len(ok):>4}")
    failures = {}
    for r in records:
        if r.get("text") is None:
            failures[r.get("error")] = failures.get(r.get("error"), 0) + 1
    if failures:
        print("failures: " + ", ".join(f"{k}={v}" for k, v in failures.items()))
    shapes = {}
    for r in records:
        for shape in (r.get("shapes") or {}).values():
            shapes[shape] = shapes.get(shape, 0) + 1
    if shapes:
        print("commands/paths: " + ", ".join(f"{k}={v}" for k, v in sorted(shapes.items())))
    for r in sorted(records, key=lambda r: r["case"]):
        if r.get("misformatted") or (r.get("wer") or 0) > 0.15:
            print(f"  {r['case']}: WER {r['wer']}, missed {r['missed']}, format {sorted(set(r['misformatted']) - set(r['missed']))}: {r['text'][:120]}")

def cut_off(path):
    """True when the clip ends in speech: the last 200 ms are well above the room's noise floor.
    ponytail: energy heuristic; misses a clip cut during a soft consonant."""
    with wave.open(str(path)) as w:
        frames = w.readframes(w.getnframes())
        rate = w.getframerate()
    samples = memoryview(frames).cast("h")
    step = rate // 20  # 50 ms
    rms = [(sum(x * x for x in samples[i:i + step]) / step) ** 0.5 for i in range(0, len(samples) - step + 1, step)]
    floor = sorted(rms)[len(rms) // 10] or 1
    return max(rms[-4:]) > 2 * floor


def record(args):
    """Records the author's pending core clips: 16 kHz mono, push Enter to start and stop."""
    pending = [c for c in CORPUS["stt"] if c.get("core") and (args.again or not audio_path(c).exists())
               and (not args.case or c["id"] in args.case)]
    for case in pending:
        path = audio_path(case)
        print(f"\n{case['id']}: say naturally{' (' + case['note'] + ')' if case.get('note') else ''}:\n  {case['reference']}")
        input("  Enter to start recording... ")
        # Small fragments and SIGINT: parecord buffers ~2 s and SIGTERM drops the unwritten tail.
        proc = subprocess.Popen(["parecord", "--latency-msec=50", "--rate=16000", "--channels=1", "--format=s16le",
                                 "--file-format=wav", str(path)])
        input("  recording; Enter to stop ")
        time.sleep(0.4)  # people release Enter as the last word ends
        proc.send_signal(signal.SIGINT)
        proc.wait()
        if cut_off(path):
            print("  the clip ends mid-speech; pause a moment before Enter and record it again (--again --case)")
        print(f"  wrote {path.relative_to(ROOT)}. If you said it differently, edit its reference in corpus.json.")


# ---------- cleanup ----------

@functools.cache
def app_prompt():
    """CLEANUP_SYSTEM_PROMPT from providers.rs."""
    source = (ROOT / "src-tauri/src/providers.rs").read_text()
    prompt = source.split('pub const CLEANUP_SYSTEM_PROMPT: &str = "', 1)[1].split('";\n', 1)[0]
    return prompt.replace("\\\n", "").replace('\\"', '"')

def system_prompt(vocabulary, extra="", prompt=None, label=None):
    """Mirror of OpenAiCompatible::clean's system message."""
    prompt = prompt or app_prompt()
    if vocabulary:
        prompt += f"\n{label or VOCABULARY_LABEL}: " + ", ".join(vocabulary)
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


@functools.cache
def relevant_terms(vocabulary, transcript):
    return relevant(list(vocabulary), transcript)


def relevant(vocabulary, transcript, limit=24):
    """Mirror of settings::relevant_terms."""
    words = [w for w in (squash(t) for t in transcript.split()) if w and w not in ("dot", "slash", "dash", "underscore")]
    spoken = "zero one two three four five six seven eight nine".split()
    digits = [str(spoken.index(w)) if w in spoken else w for w in words]
    out, seen = [], set()
    for term in (t.strip() for t in vocabulary):
        key = squash(term)
        if not key or term.lower() in seen:
            continue
        seen.add(term.lower())
        def mentions(words):
            for start in range(len(words)):
                run = ""
                for joined, word in enumerate(words[start:start + 4]):
                    run += word
                    n = len(key)
                    allowed = n // 4 if n >= 8 else 1 if n >= 5 and joined else 0
                    if abs(len(run) - n) <= allowed and distance(run, key) <= allowed:
                        return True
            return False
        if mentions(words) or mentions(digits):
            out.append(term)
    return out[:limit]


VOCABULARY_LABEL = "Spelling hints"
# Candidate prompts, selected with --prompt; "app" is CLEANUP_SYSTEM_PROMPT. "label" names the
# term list appended to the system message.
PROMPTS = {"app": {}}
PROMPTS_FILE = HERE / "prompts.json"
if PROMPTS_FILE.exists():
    PROMPTS.update(json.loads(PROMPTS_FILE.read_text()))

# name: (base URL or None for the app's 9Router endpoint, keyring account, env override, headers).
# A new OpenAI-compatible provider (Cerebras) is one line here plus a PROVIDERS entry.
TRANSPORTS = {
    "router": (None, "9router-api-key", "VP_KEY", ROUTER_HEADERS),
    "groq": (GROQ_URL, "groq-api-key", "VP_GROQ_KEY", {}),
}
# --provider name: (transport, model, reasoning_effort). "" omits the field, as the app does for "none".
PROVIDERS = {
    "groq-20b": ("groq", "openai/gpt-oss-20b", "low"),
    "router-20b": ("router", "groq/openai/gpt-oss-20b", "low"),
    "router-haiku": ("router", "claude-haiku-5-5", ""),
    "haiku-off": ("router", "claude-haiku-5-5", "none"),
    "haiku-low": ("router", "claude-haiku-5-5", "low"),
    "haiku-medium": ("router", "claude-haiku-5-5", "medium"),
    "groq-120b": ("groq", "openai/gpt-oss-120b", "low"),
}

def transport(name):
    url, account, env, headers = TRANSPORTS[name]
    return Client(url, credential(account, env), headers)

def output_budget(raw):
    """Mirror of providers::output_budget."""
    return len(raw.encode()) // 2 + 512

CASES = HERE / "cleanup.json"
LOCAL_CASES = HERE / "cleanup.local.json"  # private dictations, gitignored
CACHE = HERE / ".cache" / "cleanup"
SUITES = {"smoke": lambda c: c.get("smoke"), "dev": lambda c: c["suite"] == "dev",
          "holdout": lambda c: c["suite"] == "holdout", "full": lambda c: True}


def prompt_text(name):
    variant = PROMPTS[name]
    return variant.get("text") or app_prompt(), variant.get("label") or VOCABULARY_LABEL


@functools.cache
def prompt_hash(name):
    text, label = prompt_text(name)
    return hashlib.sha256(f"{text}\n{label}".encode()).hexdigest()[:10]


def select(args):
    corpus = json.loads(CASES.read_text())
    if LOCAL_CASES.exists():
        corpus["cases"] += json.loads(LOCAL_CASES.read_text())["cases"]
    cases = [c for c in corpus["cases"] if SUITES[args.suite](c)
             and (not args.case or c["id"] in args.case)
             and (not args.category or c["category"] in args.category)
             and (not args.source or c["source"] in args.source)]
    if not cases:
        sys.exit("no cases match")
    return corpus["vocabulary"], cases


def request(case, vocabulary, prompt, model, effort):
    """The app's request body (OpenAiCompatible::clean) for one case."""
    text, label = prompt_text(prompt)
    hints = case["hints"] if "hints" in case else relevant_terms(tuple(vocabulary), case["input"])
    body = {"model": model, "stream": False, "max_tokens": output_budget(case["input"]), "messages": [
        {"role": "system", "content": system_prompt(hints, "", text, label)},
        {"role": "user", "content": f"<transcript>\n{case['input']}\n</transcript>"}]}
    if effort:
        body["reasoning_effort"] = effort
    if "gpt-oss" in model:
        body.update(include_reasoning=False, temperature=0)
    return body


def cleanup(args):
    """Stored transcript -> cleanup provider -> deterministic evaluator. No audio, no app.

    Requests run concurrently (one keep-alive connection per worker thread). Outputs are cached
    under bench/.cache keyed by the full request body, transport and trial, so a changed prompt,
    model, effort or hint list never reuses an old output; --fresh bypasses the cache."""
    vocabulary, cases = select(args)
    for case in cases:  # outside the pool: pure-Python matching holds the GIL
        relevant_terms(tuple(vocabulary), case["input"])
    jobs = [(provider, prompt, case, trial) for provider in args.provider for prompt in args.prompt
            for trial in range(args.trials) for case in cases]
    local = threading.local()
    stop = threading.Event()

    def run(job):
        provider, prompt, case, trial = job
        path, model, effort = PROVIDERS[provider]
        body = request(case, vocabulary, prompt, model, effort)
        key = hashlib.sha256(json.dumps([TRANSPORTS[path][0], body, trial], sort_keys=True).encode()).hexdigest()
        cached = CACHE / f"{key}.json"
        record = {"provider": provider, "model": model, "effort": effort, "prompt": prompt,
                  "prompt_hash": prompt_hash(prompt), "trial": trial, "case": case["id"], "suite": case["suite"],
                  "category": case["category"], "source": case["source"]}
        if stop.is_set():
            return None
        if cached.exists() and not args.fresh:
            record.update(json.loads(cached.read_text()), cached=True)
        else:
            clients = local.__dict__.setdefault("clients", {})
            client = clients.get(path) or clients.setdefault(path, transport(path))
            receipt, parsed = client.post("/chat/completions", json.dumps(body).encode(), "application/json",
                                          timeout=args.timeout, tries=1)
            choice = (parsed.get("choices") or [{}])[0]
            result = {**receipt, "finish": choice.get("finish_reason"),
                      "output": (choice.get("message") or {}).get("content"), "usage": parsed.get("usage") or {}}
            tagged = re.findall(r"<cleaned>(.*?)</cleaned>", result["output"] or "", re.S)
            if tagged:  # mirror of providers::cleaned_text: the last tagged block wins
                result["raw"], result["output"] = result["output"], tagged[-1].strip()
            if "error" not in receipt and result["finish"] != "length":
                CACHE.mkdir(parents=True, exist_ok=True)
                cached.write_text(json.dumps(result))
            record.update(result, cached=False)
        if "error" in record:
            record["verdict"] = {"hard": [("provider", record["error"])], "soft": []}
        elif record["finish"] == "length":
            record["verdict"] = {"hard": [("provider", "finish_reason=length")], "soft": []}
        else:
            record["verdict"] = evaluate(case, record["output"])
        if args.fail_fast and record["verdict"]["hard"]:
            stop.set()
        return record

    started = time.perf_counter()
    with concurrent.futures.ThreadPoolExecutor(args.concurrency) as pool:
        records = [r for r in pool.map(run, jobs) if r]
    elapsed = time.perf_counter() - started
    if not args.quiet:
        keep = ("provider", "model", "effort", "prompt", "prompt_hash", "trial", "case", "suite", "cached",
                "wall_ms", "status", "error", "finish", "output", "verdict")
        write("cleanup", [{k: r[k] for k in keep if k in r} for r in records])
    report_cleanup(records, cases, args.verbose)
    print(f"\n{len(records)} calls ({sum(r['cached'] for r in records)} cached) in {elapsed:.1f} s"
          + (" — stopped at the first hard failure" if stop.is_set() else ""))
    if any(r["verdict"]["hard"] for r in records):
        sys.exit(1)

def connection(args):
    """Cold, warm and idle-expired requests per path, on one short case. Round 0 opens a fresh
    connection (cold); later rounds follow `--idle` seconds of silence on a fresh connection, as
    the app's 90 s pool timeout would leave it (idle); the rest reuse the connection (warm)."""
    corpus = json.loads(CASES.read_text())
    case = next(c for c in corpus["cases"] if c["id"] == "correction-deploy")
    body_for = {}
    for provider in args.provider:
        path, model, effort = PROVIDERS[provider]
        body = request(case, corpus["vocabulary"], args.prompt, model, effort)
        body_for[provider] = (transport(path), json.dumps(body).encode())
    records = []
    for round_ in range(args.rounds):
        if round_:
            time.sleep(args.idle)
        for name, (client, body) in body_for.items():
            client.reset()
            for i in range(args.warm + 1):
                receipt, parsed = client.post("/chat/completions", body, "application/json", timeout=15, tries=1)
                phase = "warm" if i else ("idle" if round_ else "cold")
                records.append({"config": name, "phase": phase, **receipt})
                print(json.dumps(records[-1]), flush=True)
    write("connection", records)
    print(f"\n{'config':36} {'phase':5} {'n':>3} {'p50':>5} {'p95':>5} {'max':>5} {'err':>3}")
    for name in body_for:
        for phase in ("cold", "idle", "warm"):
            group = [r for r in records if r["config"] == name and r["phase"] == phase]
            if not group:
                continue
            lat = summary([r["ms"] for r in group])
            print(f"{name:36} {phase:5} {lat['n']:>3} {lat['p50']:>5} {lat['p95']:>5} {max(r['ms'] for r in group):>5} "
                  f"{sum(1 for r in group if 'error' in r):>3}")

HARD = ["lost", "negation", "quantity", "question", "superseded", "answered", "invented", "literal", "empty", "provider"]


def report_cleanup(records, cases, verbose=False):
    """Per provider x prompt: semantic pass (no hard failure) first, then clean (no soft
    failure either), then latency over uncached calls only."""
    by_id = {c["id"]: c for c in cases}
    groups = {}
    for r in records:
        groups.setdefault((r["provider"], r["prompt"], r["prompt_hash"]), []).append(r)
    print(f"\n{'provider':13} {'prompt':10} {'hash':10} {'suite':8} {'n':>3} {'semantic':>9} {'clean':>7} "
          f"{'exact':>5} {'p50':>5} {'p95':>5} {'err':>3}  hard failures by type")
    for (provider, prompt, hash_), group in groups.items():
        for suite in ("dev", "holdout"):
            rows = [r for r in group if r["suite"] == suite]
            if not rows:
                continue
            hard = [r for r in rows if r["verdict"]["hard"]]
            clean = [r for r in rows if not r["verdict"]["hard"] and not r["verdict"]["soft"]]
            fresh = [r.get("wall_ms", r["ms"]) for r in rows if not r["cached"] and "error" not in r]
            lat = summary(fresh)
            types = {}
            for r in hard:
                for kind, _ in r["verdict"]["hard"]:
                    types[kind] = types.get(kind, 0) + 1
            print(f"{provider:13} {prompt:10} {hash_:10} {suite:8} {len(rows):>3} "
                  f"{len(rows) - len(hard):>4}/{len(rows):<4} {len(clean):>3}/{len(rows):<3} "
                  f"{sum(1 for r in rows if r['verdict'].get('exact')):>5} {lat['p50'] or '-':>5} {lat['p95'] or '-':>5} "
                  f"{sum(1 for r in rows if 'error' in r):>3}  "
                  + ", ".join(f"{k}={types[k]}" for k in HARD if k in types))
        trials = {r["trial"] for r in group}
        if len(trials) > 1:  # full: output stability across repetitions
            unstable = [c for c in {r["case"] for r in group}
                        if len({r.get("output") for r in group if r["case"] == c}) > 1]
            print(f"    {len(trials)} trials; outputs differ across trials in {len(unstable)} cases"
                  + (": " + ", ".join(sorted(unstable)[:12]) if unstable else ""))
        for r in sorted(group, key=lambda r: (r["suite"], r["case"], r["trial"])):
            failures = r["verdict"]["hard"] + (r["verdict"]["soft"] if verbose else [])
            if failures:
                mark = "HARD" if r["verdict"]["hard"] else "soft"
                print(f"    {mark} {r['case']}: " + "; ".join(f"{k}: {d}" for k, d in failures)[:200])
                print(f"         in:  {by_id[r['case']]['input'][:150]}")
                print(f"         out: {(r.get('output') or '')[:150]}")

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
    p = sub.add_parser("stt-live", help="fixed audio -> live STT (the app's streaming code), no cleanup")
    p.add_argument("--provider", choices=["muse", "deepgram", "scribe"], default="muse")
    p.add_argument("--language", help="override settings.json language (auto: detect)")
    p.add_argument("--chunk-ms", type=int, default=50, help="Scribe message size; capture sends 50 ms")
    p.add_argument("--label", help="results label (default: provider[-keyterms])")
    p.add_argument("--kind", nargs="*", choices=["author", "librivox", "neural", "espeak"], help="default: all")
    p.add_argument("--core", action="store_true", help="the author's core clips only")
    p.add_argument("--case", nargs="*")
    p.add_argument("--keyterms", action="store_true", help="send the extended vocabulary for technical clips")
    p.add_argument("--trials", type=int, default=1)
    p.add_argument("--concurrency", type=int, default=4)
    p.set_defaults(run=stt_live)
    p = sub.add_parser("record", help="record the author's pending core STT clips")
    p.add_argument("--case", nargs="*")
    p.add_argument("--again", action="store_true", help="re-record existing clips")
    p.set_defaults(run=record)
    p = sub.add_parser("cleanup", help="stored transcripts -> cleanup provider -> evaluator")
    p.add_argument("--suite", choices=sorted(SUITES), default="smoke",
                   help="smoke: ~13 dev cases; dev: prompt-dev; holdout: never tune on it; full: both")
    p.add_argument("--provider", nargs="+", choices=sorted(PROVIDERS), default=["groq-20b"])
    p.add_argument("--prompt", nargs="+", choices=sorted(PROMPTS), default=["app"],
                   help="system prompt variants (bench/prompts.json; app = CLEANUP_SYSTEM_PROMPT)")
    p.add_argument("--case", nargs="*", help="case ids")
    p.add_argument("--category", nargs="*", help="categories, e.g. semantic negation")
    p.add_argument("--source", nargs="*", choices=["real", "real-tts", "constructed", "real-flow"])
    p.add_argument("--trials", type=int, default=1, help="repetitions; use 3+ only for finalists (--suite full)")
    p.add_argument("--concurrency", type=int, default=8)
    p.add_argument("--timeout", type=float, default=15, help="per-request timeout, as the app's cleanup_timeout_secs")
    p.add_argument("--fresh", action="store_true", help="ignore cached outputs (latency, nondeterminism)")
    p.add_argument("--fail-fast", action="store_true", help="stop scheduling after the first hard failure")
    p.add_argument("--verbose", "-v", action="store_true", help="also list soft failures")
    p.add_argument("--quiet", "-q", action="store_true", help="do not write bench/results")
    p.set_defaults(run=cleanup)
    p = sub.add_parser("connection")
    p.add_argument("--provider", nargs="+", choices=sorted(PROVIDERS), default=["router-haiku", "groq-20b", "router-20b"])
    p.add_argument("--prompt", choices=sorted(PROMPTS), default="app")
    p.add_argument("--rounds", type=int, default=6)
    p.add_argument("--warm", type=int, default=4, help="warm requests after each cold/idle one")
    p.add_argument("--idle", type=float, default=100, help="seconds of silence between rounds")
    p.set_defaults(run=connection)
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
