//! AudioCapture via the PulseAudio API (served by PipeWire's pipewire-pulse on Pop!_OS).
use anyhow::{anyhow, Result};
use libpulse_binding::callbacks::ListResult;
use libpulse_binding::context::{Context, FlagSet, State};
use libpulse_binding::def::BufferAttr;
use libpulse_binding::mainloop::api::Mainloop as _;
use libpulse_binding::mainloop::events::io::FlagSet as IoFlags;
use libpulse_binding::mainloop::standard::{IterateResult, Mainloop};
use libpulse_binding::sample::{Format, Spec};
use libpulse_binding::stream::{Direction, PeekResult, Stream};
use libpulse_simple_binding::Simple;
use serde::Serialize;
use std::cell::RefCell;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::rc::Rc;
use std::sync::{mpsc, Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::Instant;

pub const RATE: u32 = 16_000;

fn spec() -> Spec {
    Spec {
        format: Format::S16le,
        channels: 1,
        rate: RATE,
    }
}

pub struct Recording {
    stop: UnixStream,
    handle: JoinHandle<Result<Vec<i16>>>,
    /// When the first chunk of audio arrived (capture latency instrumentation).
    first_sample: Arc<OnceLock<Instant>>,
}

impl Recording {
    /// Starts capturing from `device` (empty = default source). `on_chunk` receives each
    /// ~50 ms chunk on the capture thread and is dropped when capture stops.
    pub fn start(
        device: &str,
        max_secs: u64,
        mut on_chunk: impl FnMut(&[i16]) + Send + 'static,
    ) -> Result<Self> {
        let device = device.to_owned();
        let (stop, wake) = UnixStream::pair()?;
        let first_sample = Arc::new(OnceLock::new());
        let first = first_sample.clone();
        let (ready, started) = mpsc::sync_channel(1);
        let handle = std::thread::spawn(move || {
            let result = capture_loop(wake, |mainloop, stopped| {
                let mut context = Context::new(mainloop, "Voice Prompt")
                    .ok_or_else(|| anyhow!("pulse context"))?;
                context.connect(None, FlagSet::NOFLAGS, None)?;
                while context.get_state() != State::Ready {
                    if matches!(context.get_state(), State::Failed | State::Terminated) {
                        return Err(anyhow!("Cannot connect to audio server"));
                    }
                    iterate(mainloop)?;
                }
                let mut stream = Stream::new(&mut context, "dictation", &spec(), None)
                    .ok_or_else(|| anyhow!("pulse recording stream"))?;
                // Preserve the 50 ms fragment calibration (the server default is about 2 s).
                let attr = BufferAttr {
                    maxlength: u32::MAX,
                    tlength: u32::MAX,
                    prebuf: u32::MAX,
                    minreq: u32::MAX,
                    fragsize: RATE / 20 * 2,
                };
                stream.connect_record(
                    (!device.is_empty()).then_some(device.as_str()),
                    Some(&attr),
                    libpulse_binding::stream::FlagSet::NOFLAGS,
                )?;
                while stream.get_state() != libpulse_binding::stream::State::Ready {
                    check_stream(&stream)?;
                    iterate(mainloop)?;
                }
                ready
                    .send(Ok(()))
                    .map_err(|_| anyhow!("Capture startup abandoned"))?;
                let mut samples = Vec::with_capacity(RATE as usize * 10);
                let max = (RATE as u64 * max_secs) as usize;
                loop {
                    check_stream(&stream)?;
                    let start = samples.len();
                    match stream.peek()? {
                        PeekResult::Data(bytes) => {
                            append_audio(bytes, &mut samples, max, &first);
                            stream.discard()?;
                        }
                        PeekResult::Hole(bytes) => {
                            samples.resize(start + (bytes / 2).min(max.saturating_sub(start)), 0);
                            stream.discard()?;
                        }
                        PeekResult::Empty => {}
                    }
                    if samples.len() > start {
                        on_chunk(&samples[start..]);
                    }
                    // Peek once after the stop wakeup, preserving the final partial fragment.
                    if samples.len() >= max || (stopped.get() && samples.len() == start) {
                        break;
                    }
                    if samples.len() == start {
                        iterate(mainloop)?;
                    }
                }
                stream.disconnect()?;
                context.disconnect();
                Ok(samples)
            });
            if let Err(error) = &result {
                let _ = ready.send(Err(anyhow!("Microphone unavailable: {error:#}")));
            }
            result
        });
        if let Err(error) = started
            .recv()
            .unwrap_or_else(|_| Err(anyhow!("Capture startup failed")))
        {
            let _ = handle.join();
            return Err(error);
        }
        Ok(Self {
            stop,
            handle,
            first_sample,
        })
    }

    /// Wakes capture without waiting for audio or joining the worker.
    pub fn stop(&self) {
        // Shutdown is idempotent and also wakes a peer that has not started polling yet.
        let _ = self.stop.shutdown(std::net::Shutdown::Write);
    }

    /// Stops capture and returns the samples plus when the first chunk arrived. The
    /// timestamp is read after the join, so a chunk that arrived during it is not missed;
    /// `None` means no audio arrived at all.
    pub fn finish(self) -> (Result<Vec<i16>>, Option<Instant>) {
        self.stop();
        let samples = self
            .handle
            .join()
            .map_err(|_| anyhow!("Recording thread panicked"))
            .and_then(|r| r);
        (samples, self.first_sample.get().copied())
    }
}

fn append_audio(bytes: &[u8], samples: &mut Vec<i16>, max: usize, first: &OnceLock<Instant>) {
    first.get_or_init(Instant::now);
    samples.extend(
        bytes
            .as_chunks::<2>()
            .0
            .iter()
            .take(max.saturating_sub(samples.len()))
            .map(|b| i16::from_le_bytes(*b)),
    );
}

fn iterate(mainloop: &mut Mainloop) -> Result<()> {
    match mainloop.iterate(true) {
        IterateResult::Success(_) => Ok(()),
        result => Err(anyhow!("Capture mainloop stopped: {result:?}")),
    }
}

fn check_stream(stream: &Stream) -> Result<()> {
    if matches!(
        stream.get_state(),
        libpulse_binding::stream::State::Failed | libpulse_binding::stream::State::Terminated
    ) {
        return Err(anyhow!("Microphone stream failed"));
    }
    Ok(())
}

fn capture_loop<T>(
    wake: UnixStream,
    run: impl FnOnce(&mut Mainloop, &std::cell::Cell<bool>) -> Result<T>,
) -> Result<T> {
    let mut mainloop = Mainloop::new().ok_or_else(|| anyhow!("pulse mainloop"))?;
    let stopped = Rc::new(std::cell::Cell::new(false));
    let flag = stopped.clone();
    let _event = mainloop
        .new_io_event(
            wake.as_raw_fd(),
            IoFlags::INPUT | IoFlags::HANGUP | IoFlags::ERROR,
            Box::new(move |_, _, _| flag.set(true)),
        )
        .ok_or_else(|| anyhow!("capture wake event"))?;
    run(&mut mainloop, &stopped)
}

pub fn rms(samples: &[i16]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples.iter().map(|&s| (s as f64).powi(2)).sum();
    ((sum / samples.len() as f64).sqrt() / i16::MAX as f64) as f32
}

/// True when the clip is too quiet to contain speech, so we skip a pointless STT call.
pub fn is_silent(samples: &[i16]) -> bool {
    samples.chunks(RATE as usize / 20).all(|c| rms(c) < 0.004)
}

/// Plays a short tone (start/stop cue).
pub fn beep(freq: f32, millis: u32) {
    std::thread::spawn(move || {
        let Ok(simple) = Simple::new(
            None,
            "Voice Prompt",
            Direction::Playback,
            None,
            "cue",
            &spec(),
            None,
            None,
        ) else {
            return;
        };
        let n = RATE * millis / 1000;
        let bytes: Vec<u8> = (0..n)
            .flat_map(|i| {
                {
                    let t = i as f32 / RATE as f32;
                    let fade = (1.0 - i as f32 / n as f32).min(i as f32 / 80.0).min(1.0);
                    ((t * freq * std::f32::consts::TAU).sin() * 0.18 * fade * i16::MAX as f32)
                        as i16
                }
                .to_le_bytes()
            })
            .collect();
        let _ = simple.write(&bytes);
        let _ = simple.drain();
    });
}

#[derive(Serialize, Clone)]
pub struct Microphone {
    pub name: String,
    pub description: String,
}

/// Lists capture sources, excluding output monitors.
pub fn microphones() -> Result<Vec<Microphone>> {
    let mut mainloop = Mainloop::new().ok_or_else(|| anyhow!("pulse mainloop"))?;
    let mut context =
        Context::new(&mainloop, "Voice Prompt").ok_or_else(|| anyhow!("pulse context"))?;
    context.connect(None, FlagSet::NOFLAGS, None)?;
    loop {
        if let IterateResult::Err(e) = mainloop.iterate(true) {
            return Err(anyhow!("pulse: {e}"));
        }
        match context.get_state() {
            State::Ready => break,
            State::Failed | State::Terminated => {
                return Err(anyhow!("Cannot connect to audio server"))
            }
            _ => {}
        }
    }
    let found = Rc::new(RefCell::new(Vec::new()));
    let done = Rc::new(RefCell::new(false));
    let (found_cb, done_cb) = (found.clone(), done.clone());
    let _op = context
        .introspect()
        .get_source_info_list(move |result| match result {
            ListResult::Item(info) if info.monitor_of_sink.is_none() => {
                found_cb.borrow_mut().push(Microphone {
                    name: info.name.as_deref().unwrap_or_default().to_string(),
                    description: info.description.as_deref().unwrap_or_default().to_string(),
                })
            }
            ListResult::Item(_) => {}
            _ => *done_cb.borrow_mut() = true,
        });
    while !*done.borrow() {
        if let IterateResult::Err(e) = mainloop.iterate(true) {
            return Err(anyhow!("pulse: {e}"));
        }
    }
    context.disconnect();
    let list = found.borrow().clone();
    Ok(list)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_wakes_stalled_capture_and_allows_another_recording() {
        for cancel in [false, true, false] {
            let (stop, wake) = UnixStream::pair().unwrap();
            let (waiting, read_started) = mpsc::sync_channel(1);
            let (ended, end_of_audio) = mpsc::channel::<()>();
            let first_sample = Arc::new(OnceLock::new());
            let first = first_sample.clone();
            let (done, completed) = mpsc::channel();
            let handle = std::thread::spawn(move || {
                let samples = capture_loop(wake, |mainloop, stopped| {
                    // A partial fragment has arrived; the source then stalls indefinitely.
                    let samples = vec![12, -34, 56];
                    first.set(Instant::now()).unwrap();
                    waiting.send(()).unwrap();
                    while !stopped.get() {
                        iterate(mainloop)?;
                    }
                    drop(ended);
                    Ok(samples)
                });
                done.send(()).unwrap();
                samples
            });
            let recording = Recording {
                stop,
                handle,
                first_sample,
            };
            read_started
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap();
            let release = Instant::now();
            recording.stop();
            completed
                .recv_timeout(std::time::Duration::from_secs(1))
                .expect("stop must wake capture without another audio fragment");
            let (samples, first) = recording.finish();
            assert_eq!(samples.unwrap(), [12, -34, 56]);
            assert!(first.is_some());
            assert!(
                end_of_audio.recv().is_err(),
                "capture must drop the streaming sink"
            );
            eprintln!(
                "{} stalled capture shutdown: {:?}",
                if cancel { "cancel" } else { "release" },
                release.elapsed()
            );
        }
    }

    #[test]
    fn partial_fragment_preserves_captured_audio_and_recording_limit() {
        let first = OnceLock::new();
        let mut samples = vec![123];
        append_audio(&[0xfe, 0xff, 0x00, 0x01], &mut samples, 4, &first);
        let timestamp = *first.get().unwrap();
        assert_eq!(samples, [123, -2, 256]);
        append_audio(&[0xff, 0x7f, 0x00, 0x80], &mut samples, 4, &first);
        assert_eq!(samples, [123, -2, 256, 32767]);
        assert_eq!(first.get(), Some(&timestamp));
    }

    #[test]
    fn stop_before_poll_is_not_lost() {
        let (stop, wake) = UnixStream::pair().unwrap();
        stop.shutdown(std::net::Shutdown::Write).unwrap();
        let (done, completed) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            capture_loop(wake, |mainloop, stopped| {
                iterate(mainloop)?;
                assert!(stopped.get());
                Ok(())
            })
            .unwrap();
            done.send(()).unwrap();
        });
        completed
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn silence_detection() {
        assert!(is_silent(&vec![3i16; 16000]));
        let tone: Vec<i16> = (0..16000)
            .map(|i| ((i as f32 * 0.1).sin() * 8000.0) as i16)
            .collect();
        assert!(!is_silent(&tone));
    }
}
