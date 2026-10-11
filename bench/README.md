# Regression checks

Stdlib Python; uses the app's endpoint (`~/.config/voice-prompt/settings.json`) and keyring keys. Results land in the gitignored `results/`.

| Check | Input | Command | Time |
|---|---|---|---|
| Cleanup | stored transcripts (`cleanup.json`) | `run.py cleanup` | smoke ~2 s, dev ~15 s, cached ~0 s |
| Muse | fixed WAV clips (`corpus.json`) | `run.py stt` | ~1 min (real time) |
| E2E | WAV through the real app | `e2e.sh`, then `run.py e2e-report` | ~1 min per clip and run |

Prompt work happens in the cleanup suite; never use E2E for it.

## Cleanup

```sh
python3 bench/run.py cleanup                                   # smoke: the rapid loop
python3 bench/run.py cleanup --suite dev                       # prompt-dev
python3 bench/run.py cleanup --suite holdout                   # only for a candidate that passed dev
python3 bench/run.py cleanup --suite dev --prompt app C-preserve   # compare against a rejected candidate
python3 bench/run.py cleanup --suite full --effort none low --trials 3 --fresh   # thinking off vs low effort
python3 bench/run.py cleanup --case clause-after-filler-annoying -v
```

Transcript → Haiku (the app's exact request body) → deterministic evaluator (`evaluate.py`). No audio, no app, no LLM judge. Requests run concurrently; outputs are cached in `.cache/` keyed by the full request body and trial, so a changed prompt never reuses an old output. `--fresh` bypasses the cache (latency, nondeterminism). `--fail-fast` stops at the first hard failure. The command exits 1 on any hard failure.

To change the prompt: edit `CLEANUP_SYSTEM_PROMPT` in `src-tauri/src/providers.rs` (`app` is always read from there), or add a candidate to `prompts.json`. Run smoke, then dev; run holdout only for a promising candidate. Reject a change that improves dev but worsens holdout, and never fix an individual holdout failure in the prompt. Every report row shows the prompt's hash.

Suites: `smoke` (dev cases marked `smoke`), `dev` (prompt-dev), `holdout`, `full` (both). `--effort none` is the app's setting (thinking disabled); `low` and `medium` are adaptive thinking at that effort.

### Cases

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

Spans are compared on normalized text: case, punctuation, `don't`/`do not`, `three`/`3`, `ms`/`milliseconds`, `10–15`/`10 to 15`. Two more hard checks run on every case: recall of the closest expected output below 75% (`lost`: the deletion detector) and more than max(3, 20%) output words that appear in neither the input nor any expected output (`invented`: answers, explanations, leaked reasoning). A provider error or truncation is hard `provider`.

`source`: `real` is the author's own dictation, `real-tts` a Muse transcript of a TTS clip, `constructed` hand-written. `real-flow` cases live in the gitignored `cleanup.local.json` (the author's private dictations); `select` merges them when the file exists.

Self-check: `python3 bench/evaluate.py`.

## Muse

```sh
(cd src-tauri && cargo build --example stt)
python3 bench/run.py stt --core                 # the author's clips
python3 bench/run.py stt --core --keyterms      # with the vocabulary as keywords
python3 bench/run.py record                     # record missing core clips
```

`examples/stt.rs` streams each WAV at real time through the app's own `streaming::start`, so the handshake, early-audio queue and finalization are the app's. Columns: WER, written-form terms, omitted words, `ready`, `partial50` (first audio sent to first partial), `final50`/`final95` (last audio chunk to Muse's final transcript), failures by reason. No cleanup runs. Speech kinds (`author`, `librivox`, `neural`) are never mixed in one row.

## E2E

`bench/e2e.sh [runs] [clips]`: the real app end to end (null-sink microphone, uinput shortcut, GTK target). Quit the running app first. `VP_STREAMING=0` forces the batch Whisper path. Score with `python3 bench/run.py e2e-report OUT`.

## Other

- `python3 bench/run.py latency [LOG]`: outcome, delivery and STT-mode counts, and p50/p95 per stage from the app's `latency` log lines.
- `bench/insertion.sh`, `bench/targets.sh`: insertion into real applications.

Real clips: LibriVox (public domain) and the project author's recordings (CC0).
