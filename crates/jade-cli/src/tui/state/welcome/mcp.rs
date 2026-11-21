//! Hub MCP servers inspection and live probe sub-panel.
use super::super::*;

impl App {
    pub(super) fn open_hub_mcp(&mut self) {
        if let Some(welcome) = self.ui.welcome.as_mut() {
            welcome.mode = WelcomeMode::Mcp;
            welcome.list_selected = 0;
            welcome.error = None;
        }
        self.inputs.invalidate_catalog();
    }

    pub(super) fn handle_welcome_mcp(&mut self, key: KeyEvent) -> Action {
        let len = self.inputs.catalog.mcp.len();
        match key.code {
            KeyCode::Esc => {
                self.leave_welcome_panel();
                Action::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.bump_welcome_list(-1, len);
                Action::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.bump_welcome_list(1, len);
                Action::None
            }
            KeyCode::Char('p') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.trigger_hub_mcp_probe();
                Action::None
            }
            _ => Action::None,
        }
    }

    fn trigger_hub_mcp_probe(&mut self) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        if let Some(welcome) = self.ui.welcome.as_mut() {
            welcome.mcp_probing = true;
            welcome.error = None;
        }
        let data_dir = self.data_dir.clone();
        let cwd = self.selected_hub_cwd();
        let notice = self.notice_tx.clone();
        tokio::spawn(async move {
            let lines = match runtime::request(&data_dir, &AgentReq::ProbeMcp { cwd }).await {
                Ok(AgentResp::McpProbe(lines)) => lines,
                Ok(AgentResp::Error(err)) => vec![format!("probe failed: {err}")],
                Err(err) => vec![format!("probe failed: {err:#}")],
                _ => vec!["probe returned unexpected response".into()],
            };
            let _ = notice.send(UiNotice::McpProbeLines(lines));
        });
    }

    pub(in crate::tui) fn apply_hub_mcp_probe(&mut self, lines: Vec<String>) {
        if let Some(welcome) = self.ui.welcome.as_mut() {
            welcome.mcp_probe = lines;
            welcome.mcp_probing = false;
        }
    }
}
