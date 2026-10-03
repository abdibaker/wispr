//! AudioCapture via the PulseAudio API (served by PipeWire's pipewire-pulse on Pop!_OS).
use anyhow::{anyhow, Result};
use libpulse_binding::callbacks::ListResult;
use libpulse_binding::context::{Context, FlagSet, State};
use libpulse_binding::mainloop::standard::{IterateResult, Mainloop};
use libpulse_binding::sample::{Format, Spec};
use libpulse_binding::stream::Direction;
use libpulse_simple_binding::Simple;
use serde::Serialize;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

pub const RATE: u32 = 16_000;

fn spec() -> Spec {
    Spec {
        format: Format::S16le,
        channels: 1,
        rate: RATE,
    }
}

pub struct Recording {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<Result<Vec<i16>>>,
}

impl Recording {
    /// Starts capturing from `device` (empty = default source). `level` receives 0..1 RMS every ~50 ms.
    pub fn start(
        device: &str,
        max_secs: u64,
        level: impl Fn(f32) + Send + 'static,
    ) -> Result<Self> {
        let device = (!device.is_empty()).then(|| device.to_string());
        let simple = Simple::new(
            None,
            "Voice Prompt",
            Direction::Record,
            device.as_deref(),
            "dictation",
            &spec(),
            None,
            None,
        )
        .map_err(|e| anyhow!("Microphone unavailable: {e}"))?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = stop.clone();
        let handle = std::thread::spawn(move || {
            let mut samples: Vec<i16> = Vec::with_capacity(RATE as usize * 10);
            let mut chunk = vec![0u8; (RATE / 20 * 2) as usize];
            let max = (RATE as u64 * max_secs) as usize;
            while !stop_flag.load(Ordering::Relaxed) && samples.len() < max {
                simple
                    .read(&mut chunk)
                    .map_err(|e| anyhow!("Microphone read failed: {e}"))?;
                let start = samples.len();
                samples.extend(
                    chunk
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|b| i16::from_le_bytes(*b)),
                );
                level(rms(&samples[start..]));
            }
            Ok(samples)
        });
        Ok(Self { stop, handle })
    }

    pub fn finish(self) -> Result<Vec<i16>> {
        self.stop.store(true, Ordering::Relaxed);
        self.handle
            .join()
            .map_err(|_| anyhow!("Recording thread panicked"))?
    }
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
    fn silence_detection() {
        assert!(is_silent(&vec![3i16; 16000]));
        let tone: Vec<i16> = (0..16000)
            .map(|i| ((i as f32 * 0.1).sin() * 8000.0) as i16)
            .collect();
        assert!(!is_silent(&tone));
    }
}
