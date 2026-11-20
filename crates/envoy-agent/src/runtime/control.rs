//! Session metadata the console used to keep only in memory.

use super::server::Runtime;
use crate::agent::AgentEvent;
use anyhow::{bail, Result};
use std::path::PathBuf;

impl Runtime {
    pub(crate) fn set_cwd(&self, id: &str, cwd: PathBuf) -> Result<()> {
        self.retarget(id, Some(cwd), None)
    }

    pub(crate) fn set_target(&self, id: &str, target: String) -> Result<()> {
        self.retarget(id, None, Some(target))
    }

    /// Drop the model-visible history. The running turn already copied its
    /// inputs; the next claim of `RunningTurn` sees the empty vec.
    pub(crate) fn clear_context(&self, id: &str) -> Result<()> {
        let Some(live) = self.session(id) else {
            bail!("no such session: {id}");
        };
        let mut state = live.lock().unwrap();
        state.messages.clear();
        state.queued.clear();
        state.steer.lock().unwrap().clear();
        state.pending_session_context.clear();
        self.publish(&mut state, AgentEvent::SessionReset);
        Ok(())
    }

    fn retarget(&self, id: &str, cwd: Option<PathBuf>, target: Option<String>) -> Result<()> {
        let Some(live) = self.session(id) else {
            bail!("no such session: {id}");
        };
        let mut state = live.lock().unwrap();
        let cwd = cwd.unwrap_or_else(|| state.cwd.clone());
        let target = target.unwrap_or_else(|| state.target.clone());
        if state.cwd != cwd || state.target != target {
            state.permissions = state.permissions.rebind_workspace(cwd.clone(), target.clone());
            state.hooks = super::hooks::new_hook_engine(&self.data_dir, &cwd);
            state.cwd = cwd;
            state.target = target;
        }
        state.last_used = std::time::Instant::now();
        Ok(())
    }
}
