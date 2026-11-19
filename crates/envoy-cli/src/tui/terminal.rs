//! Terminal lifecycle guard: raw mode, alternate screen, mouse capture,
//! bracketed paste, and CSI-u keyboard enhancement.

use anyhow::Result;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::Terminal;
use std::io::{stdout, Stdout};

pub(crate) struct Tui {
    pub(super) terminal: Terminal<CrosstermBackend<Stdout>>,
    enhanced_keys: bool,
    pub(super) mouse_capture: bool,
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
        enable_raw_mode()?;
        execute!(
            stdout(),
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        )?;
        let enhanced_keys = terminal_supports_csi_u();
        if enhanced_keys {
            let _ = execute!(
                stdout(),
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
            );
        }
        let terminal = Terminal::new(CrosstermBackend::new(stdout()))?;
        Ok(Self {
            terminal,
            enhanced_keys,
            mouse_capture: true,
        })
    }

    pub(super) fn set_mouse_capture(&mut self, enabled: bool) {
        self.mouse_capture = enabled;
        if enabled {
            let _ = execute!(self.terminal.backend_mut(), EnableMouseCapture);
        } else {
            let _ = execute!(self.terminal.backend_mut(), DisableMouseCapture);
        }
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        if self.enhanced_keys {
            let _ = execute!(self.terminal.backend_mut(), PopKeyboardEnhancementFlags);
        }
        if self.mouse_capture {
            let _ = execute!(self.terminal.backend_mut(), DisableMouseCapture);
        }
        let _ = execute!(self.terminal.backend_mut(), DisableBracketedPaste);
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}
