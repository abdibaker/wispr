//! Minimal file logger (0600). Prompt and audio contents are never logged.
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

static PATH: OnceLock<PathBuf> = OnceLock::new();

struct Logger(Mutex<Option<File>>);

impl log::Log for Logger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.target().starts_with("voice_prompt")
    }
    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let line = format!("{secs} {} {}\n", record.level(), record.args());
        if cfg!(debug_assertions) {
            eprint!("{line}");
        }
        if let Some(file) = self.0.lock().unwrap().as_mut() {
            let _ = file.write_all(line.as_bytes());
        }
    }
    fn flush(&self) {}
}

pub fn init(path: &Path, debug: bool) {
    // ponytail: log rotation is a truncate past 1 MB at startup.
    if std::fs::metadata(path).is_ok_and(|m| m.len() > 1_000_000) {
        let _ = std::fs::remove_file(path);
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .ok();
    let _ = PATH.set(path.to_path_buf());
    if log::set_boxed_logger(Box::new(Logger(Mutex::new(file)))).is_ok() {
        set_debug(debug);
    }
}

pub fn set_debug(debug: bool) {
    log::set_max_level(if debug {
        log::LevelFilter::Debug
    } else {
        log::LevelFilter::Info
    });
}

pub fn path() -> PathBuf {
    PATH.get().cloned().unwrap_or_default()
}
