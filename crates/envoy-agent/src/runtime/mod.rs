//! Daemon-side single-loop agent sessions.
//!
//! Transport is a local Unix socket (`<data_dir>/agent.sock`, 0600) with
//! postcard framing (`envoy_proto::codec`).

mod client;
mod control;
pub(crate) mod hello;
mod hooks;
mod listen;
mod live;
mod monitors;
mod ops;
mod protocol;
mod server;
mod snapshot;
#[cfg(test)]
mod tests_hooks;
#[cfg(test)]
mod tests_phase;
#[cfg(test)]
mod tests_protocol;
#[cfg(test)]
mod tests_stress;

pub use client::{attach, request, tell, AttachOrigin};
pub use listen::serve;
#[allow(unused_imports)]
pub use protocol::{AgentReq, AgentResp, SendMode, SeqEvent, SessionInfo};
pub use server::Runtime;

use std::path::{Path, PathBuf};

pub fn socket_path(data_dir: &Path) -> PathBuf {
    data_dir.join("agent.sock")
}

pub(crate) fn remote_for(
    data_dir: &Path,
    target: &str,
    conduit: Option<PathBuf>,
    local_socket: Option<PathBuf>,
    local_server: Option<envoy_service::AgentServer>,
) -> envoy_service::Remote<Box<dyn envoy_service::AgentDialer>> {
    let is_local = target.is_empty() || target == "local";
    if is_local {
        if local_server.is_some() {
            let dialer = envoy_service::dialer_for(data_dir, target, conduit);
            return envoy_service::Remote::with_local(dialer, local_server);
        }
        if let Some(sock) = local_socket {
            return envoy_service::Remote::new(Box::new(envoy_service::UnixDialer::new(sock)));
        }
    }
    envoy_service::Remote::new(envoy_service::dialer_for(data_dir, target, conduit))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recall_returns_queued_follow_ups_lifo() {
        let runtime = Runtime::new(PathBuf::from("/tmp"));
        let live = runtime.ensure(
            "s1".into(),
            PathBuf::from("/tmp"),
            "local".into(),
            Vec::new(),
        );
        live.lock().unwrap().running = true;
        runtime.send("s1", "first".into(), SendMode::FollowUp);
        runtime.send("s1", "second".into(), SendMode::FollowUp);
        assert_eq!(runtime.recall("s1").as_deref(), Some("second"));
        assert_eq!(runtime.recall("s1").as_deref(), Some("first"));
        assert_eq!(runtime.recall("s1"), None);
    }

    #[tokio::test]
    async fn start_compact_validates_session_provider_busy_and_empty() {
        let dir = std::env::temp_dir().join(format!(
            "envoy-rt-compact-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let runtime = Runtime::new(dir.clone());
        let err = runtime.start_compact("missing", None, false).unwrap_err();
        assert!(err.to_string().contains("no such session"));

        let live = runtime.ensure("s1".into(), dir.clone(), "local".into(), Vec::new());
        let err = runtime.start_compact("s1", None, false).unwrap_err();
        assert!(err.to_string().contains("no model configured"));

        let prov = crate::auth::StoredProvider {
            id: "openai".into(),
            name: "OpenAI".into(),
            protocol: crate::auth::ProviderProtocol::OpenAiChat,
            base_url: "https://api.openai.com/v1".into(),
            credential: crate::auth::Credential::ApiKey {
                key: "sk-test".into(),
            },
            custom_models: vec![],
            fetched_models: vec![],
            model_routes: std::collections::BTreeMap::new(),
            updated_ms: 0,
        };
        crate::auth::record_provider_login(&dir, prov, Some("gpt-6-astra".into())).unwrap();

        let err = runtime.start_compact("s1", None, false).unwrap_err();
        assert!(err.to_string().contains("nothing to compact"));
        let err = runtime.start_compact("s1", None, true).unwrap_err();
        assert!(err.to_string().contains("nothing to hand off"));

        live.lock().unwrap().running = true;
        let err = runtime.start_compact("s1", None, true).unwrap_err();
        assert!(err.to_string().contains("busy"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
