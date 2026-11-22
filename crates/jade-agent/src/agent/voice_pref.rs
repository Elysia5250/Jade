//! User-configurable output register (`expert`, `engineer`, `plain`).
//!
//! Stored in `<data_dir>/voice.json` (mode 0600). A missing or unreadable file
//! defaults to `engineer`. The directive is injected only into the depth-0
//! `<environment>` block so it never invalidates the stable system prompt cache
//! and never leaks into internal subagents.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VoiceStyle {
    Expert,
    #[default]
    Engineer,
    Plain,
}

impl VoiceStyle {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "expert" | "pro" => Some(Self::Expert),
            "engineer" | "normal" | "default" => Some(Self::Engineer),
            "plain" | "vibe" | "simple" => Some(Self::Plain),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Expert => "expert",
            Self::Engineer => "engineer",
            Self::Plain => "plain",
        }
    }

    /// Concise English directive injected into the depth-0 environment note.
    pub fn instruction(self) -> &'static str {
        match self {
            Self::Expert => {
                "Output register: expert. Use precise technical terminology directly without explaining standard concepts; prioritize mechanisms, invariants, edge cases, and trade-offs."
            }
            Self::Engineer => {
                "Output register: engineer. Use common engineering terms directly; on first mention of specialized, low-level, or domain-specific jargon, add a brief plain-language gloss in parentheses (under 10 words). Match terms the user already used without over-explaining."
            }
            Self::Plain => {
                "Output register: plain (vibe-coding). Speak in product and user-observable behavior terms; avoid internal implementation jargon, protocol names, or code identifiers in prose. Focus on what changed, whether it works, any risks, and what to decide next."
            }
        }
    }
}

impl fmt::Display for VoiceStyle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Pref {
    #[serde(default)]
    style: VoiceStyle,
}

fn prefs_path(data_dir: &Path) -> PathBuf {
    data_dir.join("voice.json")
}

pub fn load(data_dir: &Path) -> VoiceStyle {
    let Ok(raw) = std::fs::read_to_string(prefs_path(data_dir)) else {
        return VoiceStyle::Engineer;
    };
    serde_json::from_str::<Pref>(&raw)
        .map(|prefs| prefs.style)
        .unwrap_or(VoiceStyle::Engineer)
}

pub fn instruction(data_dir: &Path) -> &'static str {
    load(data_dir).instruction()
}

pub fn set(data_dir: &Path, style: VoiceStyle) -> Result<(), String> {
    let text = serde_json::to_string_pretty(&Pref { style }).map_err(|err| err.to_string())?;
    jade_core::fs::write_atomic_sync(&prefs_path(data_dir), text.as_bytes(), 0o600)
        .map_err(|err| format!("cannot create: {err:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let mut b = [0u8; 4];
        let _ = getrandom::fill(&mut b);
        let p = std::env::temp_dir().join(format!(
            "jade-voice-{tag}-{:02x}{:02x}{:02x}{:02x}",
            b[0], b[1], b[2], b[3]
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn missing_file_defaults_to_engineer_and_round_trips() {
        let dir = tmpdir("roundtrip");
        assert_eq!(load(&dir), VoiceStyle::Engineer);
        set(&dir, VoiceStyle::Plain).unwrap();
        assert_eq!(load(&dir), VoiceStyle::Plain);
        set(&dir, VoiceStyle::Expert).unwrap();
        assert_eq!(load(&dir), VoiceStyle::Expert);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_file_falls_back_to_engineer() {
        let dir = tmpdir("corrupt");
        std::fs::write(prefs_path(&dir), "not json").unwrap();
        assert_eq!(load(&dir), VoiceStyle::Engineer);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_supports_canonical_and_alias_names() {
        assert_eq!(VoiceStyle::parse("expert"), Some(VoiceStyle::Expert));
        assert_eq!(VoiceStyle::parse("pro"), Some(VoiceStyle::Expert));
        assert_eq!(VoiceStyle::parse("engineer"), Some(VoiceStyle::Engineer));
        assert_eq!(VoiceStyle::parse("default"), Some(VoiceStyle::Engineer));
        assert_eq!(VoiceStyle::parse("plain"), Some(VoiceStyle::Plain));
        assert_eq!(VoiceStyle::parse("vibe"), Some(VoiceStyle::Plain));
        assert_eq!(VoiceStyle::parse("unknown"), None);
    }

    #[test]
    fn voice_style_rides_depth_zero_env_only() {
        let dir = tmpdir("depth");
        set(&dir, VoiceStyle::Plain).unwrap();

        let parent =
            crate::agent::prompt::environment_note_for_depth(&dir, &dir, "sess", 0).unwrap();
        assert!(parent.contains("Output register: plain"));

        let child = crate::agent::prompt::environment_note_for_depth(&dir, &dir, "sess", 1);
        assert!(child
            .as_deref()
            .is_none_or(|n| !n.contains("Output register:")));

        let system = crate::agent::prompt::build_system_prompt("local", &dir);
        assert!(!system.contains("Output register:"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
