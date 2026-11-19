//! Suspend the console so `$VISUAL` or `$EDITOR` can edit one file.
use super::*;
use std::path::Path;

pub(super) fn run(tui: &mut Tui, path: &Path) -> Result<(), String> {
    suspend(tui);
    let result = {
        let _guard = Resume(tui);
        let editor = editor_program();
        // The variable is a program name. Do not split it through a shell.
        std::process::Command::new(&editor).arg(path).status()
    };
    result
        .map(|_| ())
        .map_err(|err| format!("editor failed: {err}"))
}

fn editor_program() -> String {
    std::env::var("VISUAL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            std::env::var("EDITOR")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .unwrap_or_else(|| "vi".to_string())
}

fn suspend(tui: &mut Tui) {
    let _ = disable_raw_mode();
    let _ = execute!(
        tui.terminal.backend_mut(),
        DisableMouseCapture,
        DisableBracketedPaste,
        LeaveAlternateScreen
    );
    let _ = tui.terminal.show_cursor();
}

fn resume(tui: &mut Tui) {
    let _ = enable_raw_mode();
    let _ = execute!(
        tui.terminal.backend_mut(),
        EnterAlternateScreen,
        EnableBracketedPaste
    );
    if tui.mouse_capture {
        let _ = execute!(tui.terminal.backend_mut(), EnableMouseCapture);
    }
    let _ = tui.terminal.clear();
}

struct Resume<'a>(&'a mut Tui);

impl Drop for Resume<'_> {
    fn drop(&mut self) {
        resume(self.0);
    }
}
