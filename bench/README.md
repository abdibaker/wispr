# Benchmarks

Stdlib Python; uses the app's endpoint (`~/.config/voice-prompt/settings.json`) and keyring keys.

Three independent layers. Prompt and model work happens in the first; never use E2E for it.

| Layer | Input | Command | Time |
|---|---|---|---|
| Cleanup | stored transcripts (`cleanup.json`) | `run.py cleanup` | smoke ~2 s, prompt-dev ~5 s per provider, cached ~0 s |
| STT | fixed WAV clips (`corpus.json` `stt`) | `run.py stt-live` | ~1 min for 31 clips (real time) |
| E2E | WAV through the real app | `e2e.sh`, then `run.py e2e-report` | ~9 min per configuration |

## Cleanup

```sh
python3 bench/run.py cleanup                                   # smoke, groq-20b, shipped prompt
python3 bench/run.py cleanup --suite dev --prompt app v30      # compare a candidate on prompt-dev
python3 bench/run.py cleanup --suite holdout --prompt v30      # only for a candidate that passed dev
python3 bench/run.py cleanup --suite full --provider groq-20b router-haiku --trials 3 --fresh   # finalists
python3 bench/run.py cleanup --case clause-after-filler-annoying -v
python3 bench/run.py cleanup --category semantic negation --source real
```

Transcript → provider → deterministic evaluator (`evaluate.py`). No audio, no app, no LLM judge. Requests run concurrently (`--concurrency 8`) over keep-alive connections. Outputs are cached in `bench/.cache/` keyed by the full request body (prompt text, hints, model, effort), transport and trial, so a changed prompt never reuses an old output. `--fresh` bypasses the cache; use it for latency and nondeterminism. `--fail-fast` stops at the first hard failure. The command exits 1 on any hard failure.

Workflow for a prompt change: add a variant to `prompts.json` (`app` is always the shipped `CLEANUP_SYSTEM_PROMPT`), run `smoke`, then `dev`; run `holdout` only for a promising candidate. Reject a change that improves prompt-dev but worsens holdout. Never fix an individual holdout failure in the prompt. Every report row shows the prompt's hash.

Suites: `smoke` (13 dev cases marked `smoke`), `dev` (prompt-dev, 51), `holdout` (25), `full` (both; use `--trials 3` for finalists to get p50/p95, error rate and output stability).

Providers (`--provider`): `groq-20b`, `router-20b`, `router-haiku`, `groq-120b`. Adding an OpenAI-compatible provider is one `TRANSPORTS` line (base URL, keyring account, env override) and one `PROVIDERS` line (transport, model, effort). Request bodies mirror `OpenAiCompatible::clean`.

### Cleanup cases

Each case in `cleanup.json`:

| Field | Meaning | Failure |
|---|---|---|
| `input` | the raw transcript | |
| `expect` | canonical output, then acceptable alternatives | soft `rewrite` when the output is more than `tolerance` word edits (default 15%) from the closest |
| `keep` | spans that must survive; a list entry is a set of alternatives | **hard**: `lost`, `negation`, `quantity`, `question` (by the span's content) |
| `literal` | paths, commands, identifiers: exact, case-sensitive | **hard** `literal` |
| `forbid` | text that must not appear: the superseded value of a correction, an answer, an unspoken hint term | **hard** `superseded`, `answered` or `invented` |
| `drop` | fillers and false starts that should go | soft `kept` |
| `restore` | technical spellings cleanup should repair | soft `spelling` |
| `hints` | override the spelling hints the app would select | |
| `suite`, `smoke`, `category`, `source`, `tuned`, `note` | selection and provenance | |

Spans are compared on normalized text: case, punctuation, `don't`/`do not`, `three`/`3`, `ms`/`milliseconds`, `10–15`/`10 to 15`. Two more hard checks run on every case: recall of the closest expected output below 75% (`lost`: the summarization detector) and more than max(3, 20%) output words that appear in neither the input nor any expected output (`invented`: answers, explanations, leaked reasoning). A provider error or truncation is hard `provider`.

`source`: `real` is the author's own dictation (`history.db`, or E2E runs of the author's recordings), `real-tts` is a Muse transcript of a TTS clip (a real STT error on synthetic audio), `constructed` is hand-written. `tuned: true` marks cases the old prompts (v3 to v17) were iterated on. `real-flow` cases live in the gitignored `cleanup.local.json` (Muse transcripts of the author's private Wispr Flow dictations, exported to the gitignored `audio/flow/`); `select` merges them when the file exists.

Self-check: `python3 bench/evaluate.py`.

## STT

```sh
(cd src-tauri && cargo build --example stt)
python3 bench/run.py stt-live --provider muse                  # all recorded clips
python3 bench/run.py stt-live --provider deepgram --core --keyterms
python3 bench/run.py record                                    # record the author's pending core clips
```

`examples/stt.rs` streams each WAV at real time through the app's own `streaming::start`, so the provider code, handshake and finalization are the app's. Rows are split by speech kind (`author`, `librivox`, `neural`, `espeak`) and never mixed. Columns: WER, written-form terms, numbers, omitted reference words, `partial50` (first audio sent to first non-empty partial), `final50`/`final95` (last audio chunk to the provider's confirmed final transcript), failures by reason (connect, finalize timeout, disconnect). No cleanup runs. `--provider scribe` streams to ElevenLabs Scribe v2 Realtime (key `elevenlabs-api-key` in the keyring); `--chunk-ms` and `--language auto` vary its message size and language. Term cells read `heard/exact/total`: heard ignores case and number spelling, exact is the written form; `commands/paths` counts how each came out (exact, case, spoken, partial, missing).

The core corpus is `core: true` clips: the author's voice, 2 to 20 s, with exact references. `run.py record` prompts for missing ones (16 kHz mono via `parecord`, stopped with SIGINT after a short grace so the buffered tail is written) and warns when a clip ends mid-speech. Edit a reference if you said it differently. Synthetic clips stay a separate stress suite.

`python3 bench/run.py stt` is the older batch Whisper comparison through 9Router.

## E2E

`bench/e2e.sh [runs] [clips]`: the real app end to end (null-sink microphone, uinput shortcut, GTK target). Quit the running app first. Score with `python3 bench/run.py e2e-report OUT`. For finalists only: release → delivery latency, fallback, resource use.

## Other

- `python3 bench/run.py synth`: regenerate synthetic clips (espeak-ng, or edge-tts neural voices through 9Router).
- `python3 bench/run.py connection [--rounds 6] [--idle 100]`: cold, idle-expired and warm request latency per cleanup provider.
- `python3 bench/run.py latency [LOG]`: outcome/delivery counts and p50/p95 per stage from the app's `latency` log lines.
- `bench/insertion.sh`, `bench/targets.sh`: insertion into real applications.

Real clips: LibriVox (public domain) and the project author's recordings (CC0). Results land in `results/`.
