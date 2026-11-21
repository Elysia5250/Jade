//! Terminal lifecycle guard: raw mode, alternate screen, mouse capture,
//! bracketed paste, and CSI-u keyboard enhancement.

use anyhow::Result;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::cursor::Show;
use ratatui::crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::Terminal;
use std::io::{stdout, Stdout, Write};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Once;

pub(crate) const MODE_RAW: u8 = 1 << 0;
pub(crate) const MODE_ALT: u8 = 1 << 1;
pub(crate) const MODE_MOUSE: u8 = 1 << 2;
pub(crate) const MODE_PASTE: u8 = 1 << 3;
pub(crate) const MODE_ENHANCED: u8 = 1 << 4;

static MODE_MASK: AtomicU8 = AtomicU8::new(0);
static PANIC_HOOK_ONCE: Once = Once::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TerminalCaps {
    pub(crate) csi_u: bool,
    pub(crate) truecolor: bool,
    pub(crate) sync_update: bool,
}

impl TerminalCaps {
    pub(crate) fn detect() -> Self {
        detect_caps_from(|key| std::env::var(key).ok())
    }
}

pub(crate) fn detect_caps_from<F>(get: F) -> TerminalCaps
where
    F: Fn(&str) -> Option<String>,
{
    let term = get("TERM").unwrap_or_default();
    let term_program = get("TERM_PROGRAM").unwrap_or_default();
    let colorterm = get("COLORTERM").unwrap_or_default();
    let has_env = |key: &str| get(key).is_some_and(|v| !v.is_empty());

    let in_mux =
        has_env("TMUX") || has_env("STY") || term.starts_with("tmux") || term.starts_with("screen");
    let is_apple_terminal = term_program == "Apple_Terminal";

    let csi_u = !in_mux
        && !is_apple_terminal
        && (has_env("KITTY_WINDOW_ID")
            || has_env("WEZTERM_EXECUTABLE")
            || has_env("GHOSTTY_RESOURCES_DIR")
            || matches!(
                term_program.as_str(),
                "iTerm.app" | "WezTerm" | "ghostty" | "vscode" | "WarpTerminal" | "rio"
            )
            || matches!(
                term.as_str(),
                "xterm-kitty" | "xterm-ghostty" | "alacritty" | "foot" | "foot-extra"
            ));

    let colorterm_lower = colorterm.to_ascii_lowercase();
    let explicit_truecolor =
        colorterm_lower.contains("truecolor") || colorterm_lower.contains("24bit");
    let truecolor = explicit_truecolor
        || (!is_apple_terminal
            && !matches!(
                term.as_str(),
                "dumb" | "vt100" | "xterm" | "xterm-color" | "ansi"
            ));

    let sync_update = !is_apple_terminal && term != "dumb";

    TerminalCaps {
        csi_u,
        truecolor,
        sync_update,
    }
}

pub(crate) struct Tui {
    pub(super) terminal: Terminal<CrosstermBackend<Stdout>>,
    pub(super) enhanced_keys: bool,
    pub(super) mouse_capture: bool,
    pub(super) caps: TerminalCaps,
}

fn install_panic_hook() {
    PANIC_HOOK_ONCE.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_terminal_modes();
            prev(info);
        }));
    });
}

pub(crate) fn clear_mode_mask() {
    MODE_MASK.store(0, Ordering::Release);
}

pub(crate) fn set_mode_mask(mouse: bool, enhanced: bool) {
    let mut mask = MODE_RAW | MODE_ALT | MODE_PASTE;
    if mouse {
        mask |= MODE_MOUSE;
    }
    if enhanced {
        mask |= MODE_ENHANCED;
    }
    MODE_MASK.store(mask, Ordering::Release);
}

pub(crate) fn restore_to(
    out: &mut impl Write,
    mask: u8,
    disable_raw: &dyn Fn() -> std::io::Result<()>,
) {
    if mask & MODE_ENHANCED != 0 {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    if mask & MODE_MOUSE != 0 {
        let _ = execute!(out, DisableMouseCapture);
    }
    if mask & MODE_PASTE != 0 {
        let _ = execute!(out, DisableBracketedPaste);
    }
    if mask & MODE_RAW != 0 {
        let _ = disable_raw();
    }
    if mask & MODE_ALT != 0 {
        let _ = execute!(out, LeaveAlternateScreen);
    }
    let _ = execute!(out, Show);
    let _ = out.flush();
}

pub(crate) fn restore_terminal_modes() {
    let mask = MODE_MASK.swap(0, Ordering::AcqRel);
    if mask == 0 {
        return;
    }
    let mut out = stdout();
    restore_to(&mut out, mask, &|| disable_raw_mode());
}

struct EntryRollback(bool);

impl Drop for EntryRollback {
    fn drop(&mut self) {
        if self.0 {
            restore_terminal_modes();
        }
    }
}

impl Tui {
    pub(super) fn enter() -> Result<Self> {
        install_panic_hook();
        let mut rollback = EntryRollback(true);
        enable_raw_mode()?;
        MODE_MASK.fetch_or(MODE_RAW, Ordering::AcqRel);
        execute!(stdout(), EnterAlternateScreen)?;
        MODE_MASK.fetch_or(MODE_ALT, Ordering::AcqRel);
        execute!(stdout(), EnableMouseCapture)?;
        MODE_MASK.fetch_or(MODE_MOUSE, Ordering::AcqRel);
        execute!(stdout(), EnableBracketedPaste)?;
        MODE_MASK.fetch_or(MODE_PASTE, Ordering::AcqRel);
        let caps = TerminalCaps::detect();
        let mut enhanced_keys = false;
        if caps.csi_u
            && execute!(
                stdout(),
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
            )
            .is_ok()
        {
            MODE_MASK.fetch_or(MODE_ENHANCED, Ordering::AcqRel);
            enhanced_keys = true;
        }
        let terminal = Terminal::new(CrosstermBackend::new(stdout()))?;
        rollback.0 = false;
        Ok(Self {
            terminal,
            enhanced_keys,
            mouse_capture: true,
            caps,
        })
    }

    pub(super) fn set_mouse_capture(&mut self, enabled: bool) {
        self.mouse_capture = enabled;
        if enabled {
            MODE_MASK.fetch_or(MODE_MOUSE, Ordering::AcqRel);
            let _ = execute!(self.terminal.backend_mut(), EnableMouseCapture);
        } else {
            MODE_MASK.fetch_and(!MODE_MOUSE, Ordering::AcqRel);
            let _ = execute!(self.terminal.backend_mut(), DisableMouseCapture);
        }
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        restore_terminal_modes();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(pairs: &[(&str, &str)]) -> TerminalCaps {
        detect_caps_from(|k| {
            pairs
                .iter()
                .find(|(key, _)| *key == k)
                .map(|(_, val)| (*val).to_string())
        })
    }

    #[test]
    fn apple_terminal_disables_csi_u_truecolor_and_sync_update() {
        let c = caps(&[
            ("TERM", "xterm-256color"),
            ("TERM_PROGRAM", "Apple_Terminal"),
        ]);
        assert!(!c.csi_u);
        assert!(!c.truecolor);
        assert!(!c.sync_update);
    }

    #[test]
    fn tmux_suppresses_leaked_outer_csi_u() {
        let c = caps(&[
            ("TERM", "tmux-256color"),
            ("TERM_PROGRAM", "ghostty"),
            ("TMUX", "/tmp/tmux-501/default,123,0"),
            ("COLORTERM", "truecolor"),
        ]);
        assert!(!c.csi_u, "must not push CSI-u through tmux");
        assert!(c.truecolor);
        assert!(c.sync_update);
    }

    #[test]
    fn modern_terminals_enable_csi_u_and_truecolor() {
        let ghostty = caps(&[("TERM", "xterm-ghostty"), ("TERM_PROGRAM", "ghostty")]);
        assert!(ghostty.csi_u);
        assert!(ghostty.truecolor);
        assert!(ghostty.sync_update);

        let iterm = caps(&[("TERM", "xterm-256color"), ("TERM_PROGRAM", "iTerm.app")]);
        assert!(iterm.csi_u);
        assert!(iterm.truecolor);
    }
}
