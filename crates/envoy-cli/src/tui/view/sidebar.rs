//! Right-hand sidebar entry point (delegates to the Universal Inspector).
use crate::tui::*;

pub(super) fn draw_sidebar(frame: &mut Frame, app: &mut App, theme: &Theme, area: Rect) {
    super::inspector::draw_inspector(frame, app, theme, area);
}
