# Streaming audio during dictation

Investigated on 2026-10-05. Native streaming is implemented and locally tested; live 9Router batch transcription is verified. Browser and native realtime-provider acceptance remain unverified.

## Current bottleneck

`src-tauri/src/audio.rs` already reads 16 kHz, mono, signed 16-bit little-endian PCM in 50 ms blocks. It retains the samples until release. `src-tauri/src/main.rs::process` then encodes the whole recording as WAV and invokes `src-tauri/src/providers.rs::transcribe`, which uploads it through `/audio/transcriptions`. Cleanup and insertion run afterward.

The last observed content-free latency log entry in `~/.local/share/voice-prompt/voice-prompt.log` reports a 3.1 s recording with:

| Stage | Time after the preceding stage |
| --- | ---: |
| Release to completed transcription | 1,897 ms |
| Transcription to completed cleanup | 2,295 ms |
| Cleanup to insertion | 916 ms |
| Total after release | 5,109 ms |

This is one existing observation, not a controlled benchmark. Streaming can overlap transcription with speech; it does not eliminate the remaining cleanup and insertion time.

## What the existing endpoint permits

The documented Groq transcription interface accepts a file or URL and returns a transcription. Its request schema does not expose a native live-audio session: [official SDK transcription reference](https://github.com/groq/groq-python/blob/main/_autodocs/api-reference/audio.md).

The public 9Router implementation awaits `request.formData()` before invoking its transcription core: [STT handler at a99cf572](https://github.com/decolua/9router/blob/a99cf57239ff778b61e434c2786009d5ed1c412c/src/sse/handlers/stt.js#L37). Therefore, sending an unfinished multipart upload to that implementation cannot start upstream recognition while the body is still arriving. This is a source inspection, not verification of the version deployed at this app's configured endpoint.

Streaming response text and streaming microphone input are different capabilities. Changing the response format alone does not turn this file-upload path into live recognition.

## Open-source comparisons

### Fono: directly relevant Rust implementation

Inspected revision `ac08ee8a269ac09cdbcca65ea386547b0e888a5c`.

- Its [Groq pseudo-stream implementation](https://github.com/bogdanr/fono/blob/ac08ee8a269ac09cdbcca65ea386547b0e888a5c/crates/fono-stt/src/groq_streaming.rs) re-uploads a trailing window of up to 28 seconds, with a nominal 700 ms preview cadence and a one-preview-request in-flight cap. It applies agreement between hypotheses for previews, but performs a full-segment batch decode at a boundary or EOF. This improves feedback during speech, not necessarily the final wait after release. Repeated recognition also increases usage and rate-limit pressure.
- Its [Deepgram streaming implementation](https://github.com/bogdanr/fono/blob/ac08ee8a269ac09cdbcca65ea386547b0e888a5c/crates/fono-stt/src/deepgram_streaming.rs) sends binary PCM over a WebSocket and separates interim results from final segments. This is the transport pattern that actually overlaps audio delivery and recognition with recording.
- Do not copy its lifecycle wholesale: it uses an unbounded update channel and calls the socket writer's `close()` immediately after sending `CloseStream`. Our implementation should bound queues and drain provider final results before initiating the WebSocket close handshake.

### WhisperStreaming: agreement, not independent chunk concatenation

Inspected revision `6da90b44b7e50d79695e68166d2a2c7609c75abb`.

The project's [README](https://github.com/ufal/whisper_streaming/blob/6da90b44b7e50d79695e68166d2a2c7609c75abb/README.md) describes local agreement over successive hypotheses and explicitly warns that the hosted API backend processes fragments repeatedly, increasing cost. This is a useful alternative to blindly concatenating independent chunk transcripts, which risks repeated or missing words at boundaries.

The maintainers now direct users toward SimulStreaming. Do not choose the older project as a new runtime dependency merely because it has streaming in its name.

### SimulStreaming: local inference is a different architecture

Inspected revision `077ea37d5ab4ff98bc567e4507f140dc4e5d5ad6`.

Its [README](https://github.com/ufal/SimulStreaming/blob/077ea37d5ab4ff98bc567e4507f140dc4e5d5ad6/README.md) documents incremental Whisper inference and recommends a GPU with at least 10 GB VRAM for its best-performing Whisper model. It is relevant if local inference becomes a requirement, but introduces a Python/model-serving stack rather than improving the existing Rust hosted-provider transport.

## Recommended approach

Add an opt-in native realtime provider, initially Deepgram Nova, while retaining the existing 9Router/Groq batch provider and cleanup path. Deepgram's [streaming API](https://developers.deepgram.com/reference/speech-to-text/listen-streaming) accepts `linear16` with an explicit sample rate and channel count, matching the existing capture format without resampling. This is an integration-fit recommendation, not an accuracy or pricing ranking.

1. Begin connecting asynchronously on press without delaying microphone capture. Buffer the initial audio, and do not transmit it until the minimum hold threshold and speech gate pass. Include that buffered prefix so the first word is not clipped.
2. Send the existing PCM blocks through a bounded queue while retaining the full local recording for retry. If setup, transmission, or queue capacity fails, report the failure and preserve the recording; do not silently drop blocks or deliver a partial transcript.
3. Accumulate ordered final segments. Treat interim text as replaceable feedback, never authoritative insertion text. Keep the current overlay initially; a live-text UI is not required for the latency improvement.
4. On release, stop capture, drain all queued audio, send `CloseStream`, and continue receiving the final results and terminal metadata/closure under a deadline. Deepgram documents that [CloseStream](https://developers.deepgram.com/docs/close-stream) flushes cached audio, sends results and summary metadata, then terminates the connection. Do not close the socket immediately after sending it.
5. On cancel, terminate the session and prevent late results from entering cleanup, history, or insertion. Cancellation cannot retract audio already transmitted.
6. Run cleanup once on the complete final transcript, then use the current insertion pipeline. Do not incrementally clean partial sentences: later speech can correct earlier intent.

For a reusable session, Deepgram also offers [Finalize](https://developers.deepgram.com/docs/finalize), but `from_finalize` is not guaranteed when little audio remains. A fresh session per recording with `CloseStream` is the smaller initial lifecycle; connection reuse can follow measured evidence.

## Approval and verification

Implementation is approved, including the provider architecture, WebSocket dependency, persisted settings, and separate provider credential and outbound audio destination.

Implementation files: `src-tauri/src/audio.rs`, `src-tauri/src/streaming.rs`, `src-tauri/src/main.rs`, `src-tauri/src/settings.rs`, `src/Settings.tsx`, `src-tauri/Cargo.toml`, and `src-tauri/Cargo.lock`, plus first-run instructions in `README.md`. The session lifecycle lives in `streaming.rs` rather than `providers.rs` so the batch-provider module remains reusable by the existing pipeline example without depending on microphone capture.

- [x] Preserve batch defaults and old settings; isolate provider credentials.
- [x] Implement gated PCM delivery, bounded queues, final-tail draining, cancellation, and retained retry audio.
- [x] Add local WebSocket lifecycle tests.
- [x] Run repository checks: `cargo fmt --check`, `cargo test` (24 tests), `cargo clippy --all-targets`, `pnpm check`, and `pnpm build`.
- [x] Complete one standards/spec review and fix its three distinct findings: unconfirmed close accepted as success, below-threshold release racing a capture callback, and hidden Deepgram keyring errors. Finalization now requires terminal metadata.
- [ ] Complete browser interaction/visual verification. The native collaborative preview timed out; loading the production bundle with mock IPC did not produce a usable verification result. The temporary bounded static server is stopped and preview tabs are closed.
- [ ] Complete live-provider authentication, transcript-quality, and latency comparison with representative recordings.

Verify with existing Rust tests plus a local mock WebSocket: initial buffering, audio order, final-tail delivery, final-segment accumulation, cancellation/late results, timeouts, and full-audio retention after failure. Run the documented Rust checks and `pnpm check`, then inspect changed settings in a real browser.

Live acceptance requires a configured provider key and recordings of the same representative technical prompts through both providers. Compare release-to-STT and release-to-prompt timings and transcript quality, including filenames, identifiers, vocabulary, pauses, and self-corrections. Direct realtime-provider testing, representative quality comparison, cost comparison, and controlled latency benchmarking remain unverified. The limited live 9Router batch and transport probes are recorded below.

## 9Router Deepgram clarification

2026-10-05: The user-provided `POST /v1/audio/transcriptions` multipart route with `model=dg/nova-3` is compatible with the existing batch-provider interface in `providers.rs`. It confirms prerecorded-file transcription, not live microphone-input support. Deepgram documents prerecorded transcription as an HTTP `POST`, while its native live STT uses `wss://api.deepgram.com/v1/listen`: upgrade to WebSocket (`101 Switching Protocols`), send audio frames as they arrive, and receive results on the open connection. Interim results are optional; send `Finalize` to flush buffered audio or `CloseStream` to finish the stream. A streamed HTTP response from a completed file-upload request is still not evidence that incoming audio is transcribed live. This does not establish whether the deployed 9Router endpoint supports a realtime transport.

Sources: [Deepgram Live Audio reference](https://developers.deepgram.com/reference/speech-to-text/listen-streaming), [pre-recorded audio reference](https://developers.deepgram.com/reference/speech-to-text/listen-pre-recorded), [Finalize message](https://developers.deepgram.com/docs/finalize), and [CloseStream message](https://developers.deepgram.com/docs/close-stream).

The public 9Router revision checked for this clarification is still `a99cf57239ff778b61e434c2786009d5ed1c412c`. Its [Deepgram registry](https://github.com/decolua/9router/blob/a99cf57239ff778b61e434c2786009d5ed1c412c/open-sse/providers/registry/deepgram.js) explicitly accepts alias `dg` and model `nova-3`. Its [Deepgram transport](https://github.com/decolua/9router/blob/a99cf57239ff778b61e434c2786009d5ed1c412c/open-sse/handlers/sttCore.js#L37) awaits `file.arrayBuffer()` before posting the whole buffer to Deepgram over HTTP, then normalizes the response to `{text}`. The existing app can therefore use this batch path with speech provider `9router`, endpoint `https://9router.hmytech.dev/v1`, and model `dg/nova-3`, without an app-side Deepgram credential.

No runtime defaults, stored settings, credentials, or streaming transport were changed in this clarification. A router-native live-input endpoint and its authentication protocol still need confirmation before replacing the approved direct streaming path. The pasted example credential was not used or stored.

## Live 9Router check

Tested on 2026-10-05 with the existing configured `CODEX_9ROUTER_TOKEN`, not a key copied from the chat. Credential values were never printed or written to files. The fixture was generated locally with the installed FFmpeg `flite` filter: 16 kHz mono PCM WAV, 4.48 seconds, no microphone or private speech.

`POST https://9router.hmytech.dev/v1/audio/transcriptions` with `model=dg/nova-3`, `language=en`, and `response_format=json` returned HTTP 200 and the exact synthetic sentence:

> This is a transcription test. Please edit the configuration file.

For the recorded request, half the multipart body was sent, then delivery paused for two seconds. No HTTP response arrived during that pause. After the remaining body was sent, the response completed in 1.558 seconds, including that final upload. Total request time was 3.578 seconds including the deliberate pause. This verifies batch routing and normalization, not natural-speech accuracy or end-to-end app latency.

Bearer-authenticated WebSocket upgrade probes using the same credential did not receive a successful upgrade:

| Candidate path | Observation |
| --- | --- |
| `/v1/listen?model=dg%2Fnova-3&encoding=linear16&sample_rate=16000&channels=1` | Handshake read timed out after five seconds |
| `/v1/realtime?model=dg%2Fnova-3` | Handshake read timed out after five seconds |
| `/v1/audio/transcriptions?model=dg%2Fnova-3` | HTTP 502 Bad Gateway |

These are guessed conventional routes, not proof that a custom realtime route is absent. `GET /v1/models/stt` separately returned HTTP 403; it did not prevent the successful transcription request. No authentication or TLS checks were disabled or bypassed.

Two short synthetic transcription requests were issued. The first probe aborted on a WebSocket timeout before saving its HTTP results; the repeat retained a receipt and caught handshake timeouts independently. The successful receipt is `/tmp/wispr-router-probe.json`, with the temporary test helper at `/tmp/wispr-router-probe.py`.
