#!/usr/bin/env python3
"""Reproducible STT and cleanup benchmark against the app's 9Router endpoint.

  python3 bench/run.py synth                      # (re)generate synthetic clips with espeak-ng
  python3 bench/run.py stt [--trials 3]           # STT models x vocabulary hints
  python3 bench/run.py cleanup [--trials 2]       # cleanup configurations vs raw baseline
  python3 bench/run.py latency LOG                # p50/p95 per stage from voice-prompt.log

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
        """Paced and retried on 429: Groq allows 20 requests/minute per Whisper model."""
        for attempt in range(4):
            if self.pace:
                time.sleep(max(0, self.last + self.pace - time.monotonic()))
            self.last = time.monotonic()
            out, parsed = self._post(path, body, content_type, timeout)
            if out.get("status") != 429:
                break
            print(f"  429, waiting 30 s (attempt {attempt + 1})", file=sys.stderr, flush=True)
            time.sleep(30)
        out["attempts"] = attempt + 1
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


def synth(_args):
    for case in CORPUS["stt"]:
        if "speak" in case:
            path = HERE / "audio" / f"{case['id']}.wav"
            espeak(case["speak"], path)
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
    records = []
    for trial in range(args.trials):
        for case in CORPUS["stt"]:
            audio = audio_path(case).read_bytes()
            for model in STT_MODELS:
                for hint_name in args.hints:
                    fields = {"model": model, "response_format": "json", "language": "en"}
                    if hints[hint_name]:
                        fields["prompt"] = hints[hint_name]
                    if args.cold:
                        client.reset()
                    receipt, parsed = client.post("/audio/transcriptions", *multipart(audio, fields))
                    text = (parsed.get("text") or "").strip()
                    record = {"trial": trial, "case": case["id"], "synthetic": case["synthetic"], "model": model,
                              "hint": hint_name, **receipt, "text": text}
                    if "error" not in receipt:
                        record["wer"] = round(wer(case["reference"], text), 3)
                        record["terms"] = f"{len(term_hits(case['terms'], text))}/{len(case['terms'])}"
                    records.append(record)
                    print(json.dumps(record), flush=True)
    write("stt", records)
    report_stt(records)


def report_stt(records):
    ok = [r for r in records if "error" not in r]
    print(f"\n{'model':32} {'hint':9} {'n':>3} {'p50':>6} {'p95':>6} {'WER real':>9} {'WER synth':>9} {'terms':>7} {'errors':>6}")
    for model in STT_MODELS:
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
            print(f"{model:32} {hint_name:9} {lat['n']:>3} {lat['p50']:>6} {lat['p95']:>6} "
                  f"{statistics.mean(real) if real else 0:>9.3f} {statistics.mean(synth_) if synth_ else 0:>9.3f} "
                  f"{hit:>3}/{total:<3} {errors:>6}")


# ---------- cleanup ----------

def system_prompt(vocabulary):
    source = (ROOT / "src-tauri/src/providers.rs").read_text()
    prompt = source.split('pub const CLEANUP_SYSTEM_PROMPT: &str = "', 1)[1].split('";', 1)[0]
    prompt = prompt.replace("\\\n", "").replace('\\"', '"')
    return prompt + "\nVocabulary: " + ", ".join(vocabulary)


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
    system = system_prompt(CORPUS["vocabulary"]["default"])
    records = []
    for case in CORPUS["cleanup"]:  # raw/no-cleanup baseline: 0 ms, judged on the raw text
        records.append({"config": "raw (no cleanup)", "trial": 0, "case": case["id"], "ms": 0,
                        "output": case["raw"], "failures": judge(case, case["raw"])})
    configs = [c for c in CLEANUP_CONFIGS if not args.only or c[0] in args.only]
    for trial in range(args.trials):
        for model, effort in configs:
            name = f"{model} effort={effort or 'omitted'}"
            for case in CORPUS["cleanup"]:
                body = {"model": model, "stream": False, "messages": [
                    {"role": "system", "content": system},
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
        lat = summary([r["ms"] for r in group if "error" not in r])
        passed = sum(1 for r in group if not r["failures"])
        failed = sorted({f"{r['case']}: {'; '.join(r['failures'])}" for r in group if r["failures"]})
        print(f"{name:48} {lat['n']:>3} {lat['p50'] or 0:>6} {lat['p95'] or 0:>6} {passed:>3}/{len(group):<3} {errors:>6}  "
              + " | ".join(failed)[:400])


# ---------- app log ----------

def latency(args):
    rows = []
    for line in pathlib.Path(args.log).read_text().splitlines():
        if " latency total=" in line:
            rows.append({k: int(v) for k, v in re.findall(r"(\w+)=(\d+)", line.split(" latency ", 1)[1])})
    if not rows:
        sys.exit("no new-format latency lines")
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
    p.add_argument("--hints", nargs="+", default=["none", "default", "extended"])
    p.add_argument("--cold", action="store_true", help="new connection per request")
    p.add_argument("--pace", type=float, default=3.5, help="seconds between requests (rate limit)")
    p.set_defaults(run=stt)
    p = sub.add_parser("cleanup")
    p.add_argument("--trials", type=int, default=2)
    p.add_argument("--only", nargs="*", help="model names to include")
    p.add_argument("--pace", type=float, default=0.0, help="seconds between requests (rate limit)")
    p.set_defaults(run=cleanup)
    p = sub.add_parser("latency")
    p.add_argument("log", nargs="?", default=str(pathlib.Path.home() / ".local/share/voice-prompt/voice-prompt.log"))
    p.set_defaults(run=latency)
    args = parser.parse_args()
    args.run(args)


if __name__ == "__main__":
    main()
