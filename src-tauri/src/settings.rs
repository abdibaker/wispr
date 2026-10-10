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
    /// Stream audio to Deepgram while the key is held; batch STT stays the fallback.
    pub streaming: bool,
    pub streaming_model: String,
    /// deepgram | muse (experimental)
    pub streaming_provider: String,
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
            streaming: true,
            streaming_model: "nova-3".into(),
            streaming_provider: "deepgram".into(),
            cleanup_enabled: true,
            // Fastest model passing the full cleanup corpus (bench/results, 2026-10-09).
            cleanup_model: "claude-haiku-5-5".into(),
            // Omits the field: Haiku answers without thinking.
            reasoning_effort: "none".into(),
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
    /// The 9Router key, which also mints Deepgram tokens.
    pub const ROUTER: &str = "9router-api-key";
    /// Meta Model API key for the experimental Muse provider; Muse has no token broker.
    pub const MUSE: &str = "meta-muse-api-key";

    fn entry(account: &str) -> Result<keyring::Entry> {
        Ok(keyring::Entry::new(SERVICE, account)?)
    }

    pub fn get() -> Result<Option<String>> {
        get_for(ROUTER)
    }

    pub fn set(key: &str) -> Result<()> {
        set_for(ROUTER, key)
    }

    pub fn get_for(account: &str) -> Result<Option<String>> {
        match entry(account)?.get_password() {
            Ok(key) => Ok(Some(key)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub fn set_for(account: &str, key: &str) -> Result<()> {
        if key.is_empty() {
            return match entry(account)?.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(error) => Err(error.into()),
            };
        }
        Ok(entry(account)?.set_password(key)?)
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

/// Most terms the cleanup prompt receives; relevant ones only, so prose is not steered
/// toward product names.
pub const CLEANUP_TERMS: usize = 24;

/// Letters and digits, lowercased: "9 router", "9Router" and "9-router" compare equal.
fn squash(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn edit_distance(a: &[char], b: &[char]) -> usize {
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, x) in a.iter().enumerate() {
        let mut previous = row[0];
        row[0] = i + 1;
        for (j, y) in b.iter().enumerate() {
            let current = row[j + 1];
            row[j + 1] = (previous + (x != y) as usize)
                .min(row[j] + 1)
                .min(current + 1);
            previous = current;
        }
    }
    row[b.len()]
}

/// Whether the transcript plausibly mentions `term`, spelled right or as STT heard it
/// ("tense tack" for TanStack): some run of up to four words is within a small edit
/// distance of the term. A single word must match exactly unless the term is long, so
/// "code" does not select "Codex".
fn mentions(words: &[String], term: &str) -> bool {
    let key: Vec<char> = squash(term).chars().collect();
    if key.is_empty() {
        return false;
    }
    (0..words.len()).any(|start| {
        let mut run = String::new();
        words[start..]
            .iter()
            .take(4)
            .enumerate()
            .any(|(joined, word)| {
                run.push_str(word);
                let run: Vec<char> = run.chars().collect();
                // A lone common word one edit away ("request" for reqwest) is not evidence.
                let allowed = match key.len() {
                    n if n >= 8 => n / 4,
                    5..=7 if joined > 0 => 1,
                    _ => 0,
                };
                run.len().abs_diff(key.len()) <= allowed && edit_distance(&run, &key) <= allowed
            })
    })
}

/// Vocabulary terms the transcript appears to mention, in list order, at most `CLEANUP_TERMS`.
pub fn relevant_terms<'a>(vocabulary: &'a [String], transcript: &str) -> Vec<&'a str> {
    // Spoken separators ("cargo dot toml") carry no letters of the written term.
    let words: Vec<String> = transcript
        .split_whitespace()
        .map(squash)
        .filter(|w| !w.is_empty() && !["dot", "slash", "dash", "underscore"].contains(&w.as_str()))
        .collect();
    vocabulary_terms(vocabulary)
        .into_iter()
        .filter(|term| mentions(&words, term))
        .take(CLEANUP_TERMS)
        .collect()
}

/// File names in a window title ("main.rs - wispr - Visual Studio Code"), which are likely
/// to be dictated in that window.
pub fn title_terms(title: &str) -> Vec<String> {
    title
        .split(|c: char| c.is_whitespace() || "—–|•()[]\"'".contains(c))
        .map(|t| t.trim_matches(|c: char| ",:;!?*".contains(c)))
        .filter(|t| {
            let (stem, extension) = t.rsplit_once('.').unwrap_or_default();
            !stem.is_empty()
                && (1..=5).contains(&extension.len())
                && extension.chars().all(|c| c.is_ascii_alphanumeric())
                && extension.chars().any(|c| c.is_ascii_alphabetic())
        })
        .map(String::from)
        .take(4)
        .collect()
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
        assert!(
            hint.starts_with("term_000, term_001"),
            "first terms have priority"
        );
    }

    fn list(terms: &[&str]) -> Vec<String> {
        terms.iter().map(|t| t.to_string()).collect()
    }

    #[test]
    fn technical_speech_selects_mentioned_terms() {
        let vocabulary = list(&[
            "9Router",
            "T3 Code",
            "Tailscale",
            "TanStack",
            "shadcn",
            "Coolify",
            "Codex",
            "Claude Code",
            "Cargo.toml",
            "@tanstack/react-query",
            "reqwest",
        ]);
        assert_eq!(
            relevant_terms(
                &vocabulary,
                "Use T3 code with 9 router and tense tack for the dashboard."
            ),
            ["9Router", "T3 Code", "TanStack"]
        );
        assert_eq!(
            relevant_terms(
                &vocabulary,
                "Update cargo dot toml and add tanstack react query, keep reqwest."
            ),
            ["TanStack", "Cargo.toml", "@tanstack/react-query", "reqwest"]
        );
        assert!(relevant_terms(&vocabulary, "Don't log the request body.").is_empty());
    }

    #[test]
    fn unrelated_prose_selects_nothing() {
        let vocabulary = list(&[
            "9Router",
            "T3 Code",
            "Tailscale",
            "TanStack",
            "shadcn",
            "Coolify",
            "Codex",
            "Claude Code",
            "wispr",
            "pnpm",
            "evdev",
            "COSMIC",
        ]);
        for prose in [
            "The art of war, then, is governed by five constant factors, to be taken into account in one's deliberations.",
            "That Jane was yielding to the preference which she had begun to entertain for him from the first.",
            "Let's go to the Chinese restaurant tonight, but please do not bring Sam, you know, he is very annoying.",
            "Can you send me the report by Friday? The code of conduct says we close at five.",
        ] {
            assert!(relevant_terms(&vocabulary, prose).is_empty(), "{prose}");
        }
    }

    #[test]
    fn relevant_terms_are_bounded() {
        let many: Vec<String> = (0..100).map(|i| format!("term{i:03}")).collect();
        let transcript = many.join(" ");
        assert_eq!(relevant_terms(&many, &transcript).len(), CLEANUP_TERMS);
    }

    #[test]
    fn title_file_names() {
        assert_eq!(
            title_terms("main.rs - wispr - Visual Studio Code"),
            ["main.rs"]
        );
        assert_eq!(
            title_terms("● Cargo.toml — src-tauri (settings.json)"),
            ["Cargo.toml", "settings.json"]
        );
        assert!(title_terms("GroqCloud - Google Chrome").is_empty());
        assert!(title_terms("abdibaker@hmydev: ~ — COSMIC Terminal").is_empty());
        assert!(title_terms("Version 2.0 released").is_empty());
    }
}
