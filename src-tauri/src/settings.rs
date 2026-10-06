//! SettingsStore, SecretStore and VocabularyService.
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    // General
    pub autostart: bool,
    pub shortcut: String,
    pub auto_insert: bool,
    pub keep_in_clipboard: bool,
    pub notifications: bool,
    pub sounds: bool,
    // Speech
    pub microphone: String,
    pub endpoint: String,
    pub stt_model: String,
    pub language: String,
    // Cleanup
    pub cleanup_enabled: bool,
    pub cleanup_model: String,
    pub reasoning_effort: String,
    pub cleanup_instructions: String,
    // Vocabulary
    pub vocabulary: Vec<String>,
    // History
    pub history_enabled: bool,
    pub history_retention_days: u32,
    // Advanced
    pub insertion_method: String,
    pub debug_logging: bool,
    pub stt_timeout_secs: u64,
    pub cleanup_timeout_secs: u64,
    pub min_hold_ms: u64,
    pub max_record_secs: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            autostart: true,
            shortcut: "Ctrl+Super".into(),
            auto_insert: true,
            keep_in_clipboard: true,
            notifications: true,
            sounds: true,
            microphone: String::new(),
            endpoint: "https://llm.abdibaker.com/v1".into(),
            // ponytail: spec's groq/distil-whisper-large-v3-en is decommissioned upstream.
            stt_model: "groq/whisper-large-v3-turbo".into(),
            language: "en".into(),
            cleanup_enabled: true,
            cleanup_model: "gpt-6-luna".into(),
            reasoning_effort: "low".into(),
            cleanup_instructions: String::new(),
            vocabulary: [
                "9Router",
                "T3 Code",
                "Tailscale",
                "TanStack",
                "shadcn",
                "Coolify",
                "Codex",
                "Claude Code",
            ]
            .map(String::from)
            .to_vec(),
            history_enabled: true,
            history_retention_days: 30,
            insertion_method: "type".into(),
            debug_logging: false,
            stt_timeout_secs: 30,
            cleanup_timeout_secs: 15,
            min_hold_ms: 250,
            max_record_secs: 300,
        }
    }
}

pub fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"))
        .join("voice-prompt")
}

pub fn data_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local/share"))
        .join("voice-prompt")
}

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/tmp".into()))
}

/// Creates a 0700 directory.
pub fn private_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

pub fn private_file(path: &Path) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// Writes a file atomically with 0600 permissions.
pub fn write_private(path: &Path, contents: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(contents)?;
    file.sync_all()?;
    fs::rename(&tmp, path)?;
    Ok(())
}

pub fn load(dir: &Path) -> Settings {
    fs::read(dir.join("settings.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

pub fn save(dir: &Path, settings: &Settings) -> Result<()> {
    private_dir(dir)?;
    write_private(
        &dir.join("settings.json"),
        &serde_json::to_vec_pretty(settings)?,
    )
}

/// Writes or removes the XDG autostart entry.
pub fn apply_autostart(enabled: bool) -> Result<()> {
    let dir = config_dir().parent().unwrap().join("autostart");
    let path = dir.join("voice-prompt.desktop");
    if enabled {
        fs::create_dir_all(&dir)?;
        let exe = std::env::current_exe()?;
        // Prefer the installed binary so autostart survives rebuilds in a dev tree.
        let exe = if Path::new("/usr/bin/voice-prompt").exists() {
            PathBuf::from("/usr/bin/voice-prompt")
        } else {
            exe
        };
        let entry = format!(
            "[Desktop Entry]\nType=Application\nName=Voice Prompt\nComment=Push-to-talk prompt dictation\nExec=\"{exe}\" --background\nTryExec={exe}\nIcon=voice-prompt\nTerminal=false\nX-GNOME-Autostart-enabled=true\n",
            exe = exe.display()
        );
        fs::write(&path, entry).context("writing autostart entry")?;
    } else if path.exists() {
        fs::remove_file(&path)?;
    }
    Ok(())
}

pub mod secret {
    use anyhow::Result;
    const SERVICE: &str = "voice-prompt";
    const ACCOUNT: &str = "9router-api-key";

    fn entry() -> Result<keyring::Entry> {
        Ok(keyring::Entry::new(SERVICE, ACCOUNT)?)
    }

    pub fn get() -> Result<Option<String>> {
        match entry()?.get_password() {
            Ok(key) => Ok(Some(key)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub fn set(key: &str) -> Result<()> {
        if key.is_empty() {
            return match entry()?.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(error) => Err(error.into()),
            };
        }
        Ok(entry()?.set_password(key)?)
    }
}

/// Character budget for the STT hint. Whisper keeps only the last 224 prompt tokens, and
/// identifiers tokenize at roughly 2 characters per token, so overflow would silently drop
/// the first (highest-priority) terms.
pub const STT_HINT_CHARS: usize = 400;

/// Trimmed, case-insensitively deduplicated terms in list order.
pub fn vocabulary_terms(vocabulary: &[String]) -> Vec<&str> {
    let mut seen = std::collections::HashSet::new();
    vocabulary
        .iter()
        .map(|t| t.trim())
        .filter(|t| !t.is_empty() && seen.insert(t.to_lowercase()))
        .collect()
}

/// Vocabulary terms joined as an STT prompt hint (Whisper reads the prompt as prior context),
/// up to `STT_HINT_CHARS`. Cleanup receives the full list via `vocabulary_terms`.
pub fn vocabulary_hint(vocabulary: &[String]) -> Option<String> {
    let mut hint = String::new();
    for term in vocabulary_terms(vocabulary) {
        let separator = if hint.is_empty() { "" } else { ", " };
        if hint.chars().count() + separator.len() + term.chars().count() > STT_HINT_CHARS {
            break;
        }
        hint.push_str(separator);
        hint.push_str(term);
    }
    (!hint.is_empty()).then_some(hint)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings {
            language: "sw".into(),
            ..Default::default()
        };
        save(dir.path(), &settings).unwrap();
        assert_eq!(load(dir.path()), settings);
        let mode = fs::metadata(dir.path().join("settings.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn partial_file_falls_back_to_defaults() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("settings.json"), r#"{"language":"de"}"#).unwrap();
        let settings = load(dir.path());
        assert_eq!(settings.language, "de");
        assert_eq!(settings.shortcut, "Ctrl+Super");
    }

    #[test]
    fn vocabulary_hint_skips_blanks() {
        let words = vec!["9Router".into(), " ".into(), "T3 Code".into()];
        assert_eq!(vocabulary_hint(&words).unwrap(), "9Router, T3 Code");
        assert!(vocabulary_hint(&[]).is_none());
    }

    #[test]
    fn vocabulary_hint_is_bounded_and_deduplicated() {
        let words = vec!["Cargo.toml".into(), "cargo.toml".into(), "pnpm".into()];
        assert_eq!(vocabulary_hint(&words).unwrap(), "Cargo.toml, pnpm");
        let many: Vec<String> = (0..200).map(|i| format!("term_{i:03}")).collect();
        let hint = vocabulary_hint(&many).unwrap();
        assert!(hint.chars().count() <= STT_HINT_CHARS);
        assert!(hint.starts_with("term_000, term_001"), "first terms have priority");
    }
}
