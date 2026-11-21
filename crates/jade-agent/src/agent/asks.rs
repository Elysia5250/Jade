//! Ask broker: routes questions from tools to the UI.
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::oneshot;

/// How often a waiting prompt re-checks whether the user is still interacting.
const ENGAGED_TICK: Duration = Duration::from_millis(250);

/// Routes `ask` questions to the UI and waits for the chosen option.
#[derive(Default)]
pub struct AskBroker {
    next: AtomicU64,
    pending: Mutex<HashMap<u64, oneshot::Sender<String>>>,
    engaged: Mutex<HashSet<u64>>,
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
        self.engaged.lock().unwrap().remove(&id);
    }

    /// The console reports that the user is working inside this prompt, so the
    /// answer clock must stop instead of firing underneath them.
    pub fn engage(&self, id: u64) -> bool {
        let mut engaged = self.engaged.lock().unwrap();
        if self.pending.lock().unwrap().contains_key(&id) {
            engaged.insert(id);
            true
        } else {
            false
        }
    }

    pub fn is_engaged(&self, id: u64) -> bool {
        self.engaged.lock().unwrap().contains(&id)
    }

    /// Wait for an answer, stopping the clock once the user has interacted.
    ///
    /// Returns `None` when the prompt expired unanswered or the asker went away.
    pub(crate) async fn wait_for_answer(
        &self,
        id: u64,
        timeout: Duration,
        recv: &mut oneshot::Receiver<String>,
    ) -> Option<String> {
        let mut remaining = timeout;
        loop {
            let step = ENGAGED_TICK.min(remaining);
            match tokio::time::timeout(step, &mut *recv).await {
                Ok(Ok(label)) => return Some(label),
                Ok(Err(_)) => return None,
                Err(_) => {
                    if self.is_engaged(id) {
                        continue;
                    }
                    remaining = remaining.saturating_sub(step);
                    if remaining.is_zero() {
                        return None;
                    }
                }
            }
        }
    }

    /// Answer a pending question. Returns false if it already timed out.
    pub fn answer(&self, id: u64, label: String) -> bool {
        match self.pending.lock().unwrap().remove(&id) {
            Some(tx) => {
                self.engaged.lock().unwrap().remove(&id);
                tx.send(label).is_ok()
            }
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

    #[tokio::test]
    async fn an_engagement_stops_the_answer_clock() {
        let asks = AskBroker::new();
        let (tx, mut rx) = oneshot::channel();
        asks.register(9, tx);
        assert!(!asks.engage(99), "unknown asks cannot be engaged");
        assert!(asks.engage(9));

        let waiter = asks.clone();
        let waited = tokio::time::timeout(
            Duration::from_millis(600),
            waiter.wait_for_answer(9, Duration::from_millis(100), &mut rx),
        )
        .await;
        assert!(waited.is_err(), "engaged prompt must not expire");

        asks.answer(9, "B".into());
        let label = asks
            .wait_for_answer(9, Duration::from_millis(100), &mut rx)
            .await;
        assert_eq!(label.as_deref(), Some("B"));
        assert!(!asks.is_engaged(9), "forget clears the engagement");
    }

    #[tokio::test]
    async fn an_idle_prompt_still_expires() {
        let asks = AskBroker::new();
        let (tx, mut rx) = oneshot::channel();
        asks.register(11, tx);
        let started = std::time::Instant::now();
        let label = asks
            .wait_for_answer(11, Duration::from_millis(150), &mut rx)
            .await;
        assert!(label.is_none());
        assert!(started.elapsed() >= Duration::from_millis(140));
    }
}
