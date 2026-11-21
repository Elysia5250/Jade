//! Scramble-and-reveal ("Decrypt Effect") animation for progressive TUI statuses.
use super::links::LinkedSpan;
use super::theme::Theme;
use super::types::HitTarget;
use jade_agent::session::{CardStatus, TranscriptItem};
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;

pub(crate) const DECRYPT_POOL: &[u8] = b"!<>-_\\/[]{}=+*^?#0123456789";
pub(crate) const DECRYPT_TICK_MS: u64 = 50;
pub(crate) const DECRYPT_STEP_MS: u64 = 70;
pub(crate) const DECRYPT_HOLD_MS: u64 = 640;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DecryptPhase {
    Forward,
    Hold,
    Backward,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DecryptFrame {
    pub(crate) phase: DecryptPhase,
    pub(crate) cursor: usize,
    pub(crate) locked: String,
    pub(crate) noise: String,
}

fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

fn pick_noise(avoid: char, tick: u64, pos: usize, salt: u64) -> char {
    let n = DECRYPT_POOL.len();
    let h = splitmix64(
        salt ^ tick.wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ (pos as u64).wrapping_mul(0xBF58_476D_1CE4_E5B9),
    );
    let idx = (h as usize) % n;
    let ch = DECRYPT_POOL[idx] as char;
    if ch == avoid && n > 1 {
        let step = 1 + ((h >> 16) as usize % (n - 1));
        DECRYPT_POOL[(idx + step) % n] as char
    } else {
        ch
    }
}

/// Computes the ping-pong reveal cursor and single-width scrambled tail at `elapsed_ms`.
pub(crate) fn decrypt_frame(text: &str, prefix: usize, elapsed_ms: u64, salt: u64) -> DecryptFrame {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let prefix = prefix.min(n);
    if prefix >= n {
        return DecryptFrame {
            phase: DecryptPhase::Hold,
            cursor: n,
            locked: text.to_string(),
            noise: String::new(),
        };
    }

    let span = (n - prefix) as u64;
    let forward_ms = span * DECRYPT_STEP_MS;
    let hold_end_ms = forward_ms + DECRYPT_HOLD_MS;
    let cycle_ms = (2 * span - 1) * DECRYPT_STEP_MS + DECRYPT_HOLD_MS;
    let t = elapsed_ms % cycle_ms;

    let (phase, cursor) = if t < forward_ms {
        (
            DecryptPhase::Forward,
            prefix + (t / DECRYPT_STEP_MS) as usize,
        )
    } else if t < hold_end_ms {
        (DecryptPhase::Hold, n)
    } else {
        let back_steps = (t - hold_end_ms) / DECRYPT_STEP_MS;
        (
            DecryptPhase::Backward,
            n - 1 - (back_steps as usize).min(n - prefix - 1),
        )
    };

    let locked: String = chars[..cursor].iter().collect();
    if phase == DecryptPhase::Hold || cursor >= n {
        return DecryptFrame {
            phase,
            cursor: n,
            locked,
            noise: String::new(),
        };
    }

    let tick = elapsed_ms / DECRYPT_TICK_MS;
    let noise: String = (cursor..n)
        .map(|i| {
            let ch = chars[i];
            if ch == ' ' || !ch.is_ascii() {
                ch
            } else {
                pick_noise(ch, tick, i, salt)
            }
        })
        .collect();

    DecryptFrame {
        phase,
        cursor,
        locked,
        noise,
    }
}

pub(crate) fn decrypt_line_spans(
    text: &str,
    prefix: usize,
    elapsed_ms: u64,
    salt: u64,
    locked_style: Style,
    noise_style: Style,
) -> Vec<Span<'static>> {
    let frame = decrypt_frame(text, prefix, elapsed_ms, salt);
    let mut out = Vec::with_capacity(2);
    if !frame.locked.is_empty() {
        out.push(Span::styled(frame.locked, locked_style));
    }
    if !frame.noise.is_empty() {
        out.push(Span::styled(frame.noise, noise_style));
    }
    out
}

pub(crate) fn decrypt_spans(
    text: &str,
    prefix: usize,
    elapsed_ms: u64,
    salt: u64,
    theme: &Theme,
    target: Option<HitTarget>,
) -> Vec<LinkedSpan> {
    let locked = Style::default().fg(theme.text).add_modifier(Modifier::BOLD);
    let noise = Style::default().fg(theme.dim);
    decrypt_line_spans(text, prefix, elapsed_ms, salt, locked, noise)
        .into_iter()
        .map(|span| LinkedSpan {
            span,
            target: target.clone(),
        })
        .collect()
}

pub(crate) fn running_tool_phrase(name: &str) -> &'static str {
    match name {
        "edit" => "Editing",
        "write" => "Writing",
        "read" => "Reading",
        "exec" | "shell" => "Running command",
        "glob" | "grep" => "Searching",
        "ls" => "Listing",
        "webfetch" => "Fetching",
        "delegate" => "Delegating",
        "skill" => "Running skill",
        "todo" => "Updating plan",
        "job" => "Running job",
        "monitor" => "Monitoring",
        "ask" => "Waiting for input",
        _ => "Working",
    }
}

pub(crate) fn running_group_phrase(group: &[TranscriptItem]) -> &'static str {
    let mut active: Option<&'static str> = None;
    let mut exec_count = 0usize;
    let mut total_exec = 0usize;
    for item in group {
        let TranscriptItem::Tool { name, status, .. } = item else {
            continue;
        };
        if matches!(name.as_str(), "exec" | "shell") {
            total_exec += 1;
        }
        if *status != CardStatus::Running {
            continue;
        }
        if matches!(name.as_str(), "exec" | "shell") {
            exec_count += 1;
        }
        let kind = match name.as_str() {
            "exec" | "shell" => "Running command",
            "read" | "glob" | "grep" | "ls" => "Exploring",
            _ => running_tool_phrase(name),
        };
        match active {
            None => active = Some(kind),
            Some(prev) if prev == kind => {}
            Some(_) => return "Working",
        }
    }
    match active {
        Some("Running command") if exec_count > 1 || total_exec > 1 => "Running commands",
        Some(label) => label,
        None => "Working",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ping_pong_cursor_and_hold_match_spec_timings() {
        assert_eq!(DECRYPT_POOL.iter().position(|b| *b as char == '—'), None);

        let f0 = decrypt_frame("connecting", 0, 0, 7);
        assert_eq!((f0.phase, f0.cursor), (DecryptPhase::Forward, 0));
        assert!(f0.locked.is_empty());
        assert_eq!(f0.noise.chars().count(), 10);
        for (shown, real) in f0.noise.chars().zip("connecting".chars()) {
            assert!(DECRYPT_POOL.contains(&(shown as u8)));
            assert_ne!(shown, real);
        }

        let f_mid = decrypt_frame("connecting", 0, 350, 7);
        assert_eq!((f_mid.phase, f_mid.cursor), (DecryptPhase::Forward, 5));
        assert_eq!(f_mid.locked, "conne");

        let f_hold = decrypt_frame("connecting", 0, 700, 7);
        assert_eq!((f_hold.phase, f_hold.cursor), (DecryptPhase::Hold, 10));
        assert_eq!(f_hold.locked, "connecting");
        assert!(f_hold.noise.is_empty());

        let f_back = decrypt_frame("connecting", 0, 1340, 7);
        assert_eq!((f_back.phase, f_back.cursor), (DecryptPhase::Backward, 9));
        assert_eq!(f_back.locked, "connectin");

        let f_wrap = decrypt_frame("connecting", 0, 1970, 7);
        assert_eq!((f_wrap.phase, f_wrap.cursor), (DecryptPhase::Forward, 0));
    }

    #[test]
    fn scramble_preserves_spaces_and_prefix() {
        let f = decrypt_frame("Running commands", 4, 0, 3);
        assert_eq!(f.locked, "Runn");
        let tail_chars: Vec<char> = f.noise.chars().collect();
        assert_eq!(tail_chars.len(), "ing commands".len());
        assert_eq!(tail_chars[3], ' ');

        let held_left_before = decrypt_frame("Running commands", 4, 50, 3).locked;
        assert_eq!(held_left_before, "Runn");
    }
}
