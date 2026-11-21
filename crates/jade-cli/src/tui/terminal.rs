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

pub(crate) struct Tui {
    pub(super) terminal: Terminal<CrosstermBackend<Stdout>>,
    pub(super) enhanced_keys: bool,
    pub(super) mouse_capture: bool,
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

fn terminal_supports_csi_u() -> bool {
    if std::env::var_os("KITTY_WINDOW_ID").is_some()
        || std::env::var_os("WEZTERM_EXECUTABLE").is_some()
        || std::env::var_os("GHOSTTY_RESOURCES_DIR").is_some()
    {
        return true;
    }
    if matches!(
        std::env::var("TERM_PROGRAM").as_deref(),
        Ok("iTerm.app" | "WezTerm" | "ghostty" | "vscode" | "WarpTerminal" | "rio")
    ) {
        return true;
    }
    matches!(
        std::env::var("TERM").as_deref(),
        Ok("xterm-kitty" | "xterm-ghostty" | "alacritty" | "foot" | "foot-extra")
    )
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
        let enhanced_keys = terminal_supports_csi_u();
        if enhanced_keys
            && execute!(
                stdout(),
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
            )
            .is_ok()
        {
            MODE_MASK.fetch_or(MODE_ENHANCED, Ordering::AcqRel);
        }
        let terminal = Terminal::new(CrosstermBackend::new(stdout()))?;
        rollback.0 = false;
        Ok(Self {
            terminal,
            enhanced_keys,
            mouse_capture: true,
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
