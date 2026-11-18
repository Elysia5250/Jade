//! Ask broker: routes questions from tools to the UI.
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

/// Routes `ask` questions to the UI and waits for the chosen option.
#[derive(Default)]
pub struct AskBroker {
    next: AtomicU64,
    pending: Mutex<HashMap<u64, oneshot::Sender<String>>>,
    prompt_gate: tokio::sync::Mutex<()>,
    non_interactive: bool,
}

impl std::fmt::Debug for AskBroker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AskBroker")
    }
}

impl AskBroker {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn new_headless() -> Arc<Self> {
        Arc::new(Self {
            non_interactive: true,
            ..Self::default()
        })
    }

    pub fn is_non_interactive(&self) -> bool {
        self.non_interactive
    }

    /// Serialize UI ask/approval prompts so concurrent subagents never overwrite
    /// the single active prompt slot in the TUI.
    pub(crate) async fn acquire_prompt(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.prompt_gate.lock().await
    }

    pub(crate) fn next_id(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub(crate) fn register(&self, id: u64, tx: oneshot::Sender<String>) {
        self.pending.lock().unwrap().insert(id, tx);
    }

    pub(crate) fn forget(&self, id: u64) {
        self.pending.lock().unwrap().remove(&id);
    }

    /// Answer a pending question. Returns false if it already timed out.
    pub fn answer(&self, id: u64, label: String) -> bool {
        match self.pending.lock().unwrap().remove(&id) {
            Some(tx) => tx.send(label).is_ok(),
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ask_broker_routes_an_answer_once() {
        let asks = AskBroker::new();
        let (tx, rx) = oneshot::channel();
        asks.register(7, tx);
        assert!(asks.answer(7, "yes".into()));
        assert_eq!(rx.await.unwrap(), "yes");
        assert!(!asks.answer(7, "again".into()));
    }

}
