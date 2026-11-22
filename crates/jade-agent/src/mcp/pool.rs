//! Daemon-level persistent MCP connection pool keyed by canonical workspace directory.
//!
//! Avoids cold-starting stdio MCP servers on every turn while automatically
//! reconnecting when config files or workspace trust change (via config fingerprint).

use super::client::McpManager;
use super::config::load_mcp_configs;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

const DEFAULT_MAX_IDLE: Duration = Duration::from_secs(30 * 60);

struct PoolEntry {
    fingerprint: String,
    manager: Arc<McpManager>,
    last_used: Instant,
}

pub struct McpPool {
    data_dir: PathBuf,
    entries: Mutex<HashMap<PathBuf, PoolEntry>>,
}

impl std::fmt::Debug for McpPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpPool")
            .field("data_dir", &self.data_dir)
            .finish()
    }
}

impl McpPool {
    pub fn new(data_dir: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            data_dir,
            entries: Mutex::new(HashMap::new()),
        })
    }

    fn key_for(cwd: &Path) -> PathBuf {
        std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf())
    }

    /// Return a warm [`McpManager`] for `cwd`, reconnecting only if the effective
    /// MCP configuration or trust status changed. Also refreshes any server that
    /// sent `notifications/tools/list_changed`.
    pub async fn get_or_connect(&self, cwd: &Path) -> Arc<McpManager> {
        let key = Self::key_for(cwd);
        let report = load_mcp_configs(&self.data_dir, cwd);
        let fp = report.fingerprint.clone();

        {
            let mut guard = self.entries.lock().await;
            evict_expired(&mut guard, DEFAULT_MAX_IDLE);
            if let Some(entry) = guard.get_mut(&key) {
                if entry.fingerprint == fp {
                    entry.last_used = Instant::now();
                    let mgr = Arc::clone(&entry.manager);
                    drop(guard);
                    let _ = mgr.refresh_dirty_tools().await;
                    return mgr;
                }
            }
        }

        // Connect outside the pool lock so other workspaces never block.
        let connected = Arc::new(McpManager::connect_report(report, cwd).await);
        let mut guard = self.entries.lock().await;
        guard.insert(
            key,
            PoolEntry {
                fingerprint: fp,
                manager: Arc::clone(&connected),
                last_used: Instant::now(),
            },
        );
        connected
    }

    /// Force a fresh probe/reconnect for `cwd` and store the resulting warm manager.
    pub async fn reconnect(&self, cwd: &Path) -> Arc<McpManager> {
        let key = Self::key_for(cwd);
        let report = load_mcp_configs(&self.data_dir, cwd);
        let fp = report.fingerprint.clone();
        let connected = Arc::new(McpManager::connect_report(report, cwd).await);
        let mut guard = self.entries.lock().await;
        guard.insert(
            key,
            PoolEntry {
                fingerprint: fp,
                manager: Arc::clone(&connected),
                last_used: Instant::now(),
            },
        );
        connected
    }

    /// Drop cached connections for `cwd` (e.g. on session close or cwd switch).
    pub async fn invalidate(&self, cwd: &Path) {
        let key = Self::key_for(cwd);
        self.entries.lock().await.remove(&key);
    }

    /// Drop all pooled MCP connections (killing child stdio processes via `kill_on_drop`).
    pub async fn shutdown(&self) {
        self.entries.lock().await.clear();
    }
}

fn evict_expired(entries: &mut HashMap<PathBuf, PoolEntry>, max_idle: Duration) {
    let now = Instant::now();
    entries.retain(|_, entry| now.duration_since(entry.last_used) < max_idle);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::config::{load_mcp_configs_with_managed, McpScope};
    use crate::mcp::extras::McpTurnView;
    use crate::permissions::set_workspace_trusted;
    use serde_json::json;

    fn tmpdir(tag: &str) -> PathBuf {
        let mut b = [0u8; 6];
        let _ = getrandom::fill(&mut b);
        let p = std::env::temp_dir().join(format!(
            "jade-mcp-pool-{tag}-{:02x}{:02x}{:02x}{:02x}",
            b[0], b[1], b[2], b[3]
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[tokio::test]
    async fn pool_reuses_live_stdio_process_across_turns_and_reconnects_on_config_change() {
        let root = tmpdir("reuse");
        let data = root.join("data");
        let cwd = root.join("repo");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();

        // Stateful counter in the stdio server proves the same child process stays alive.
        let script = r#"
n=0
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-03-26","capabilities":{"tools":{}}}}\n' "$id"
      ;;
    *'"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"bump","description":"Increment counter","inputSchema":{"type":"object"}}]}}\n' "$id"
      ;;
    *'"tools/call"'*)
      n=$((n+1))
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"count=%s"}],"isError":false}}\n' "$id" "$n"
      ;;
  esac
done
"#;
        std::fs::write(
            data.join("mcp.json"),
            serde_json::to_string_pretty(&json!({
                "mcpServers": {
                    "counter": { "command": "sh", "args": ["-c", script] }
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let pool = McpPool::new(data.clone());
        let mgr1 = pool.get_or_connect(&cwd).await;
        let out1 = mgr1.call_tool("mcp__counter__bump", json!({})).await.unwrap();
        assert_eq!(out1, "count=1");

        // Second "turn": same pool entry -> same live child -> counter is 2!
        let mgr2 = pool.get_or_connect(&cwd).await;
        assert!(Arc::ptr_eq(&mgr1, &mgr2));
        let out2 = mgr2.call_tool("mcp__counter__bump", json!({})).await.unwrap();
        assert_eq!(out2, "count=2");

        // Mutate config (timeout_s) -> fingerprint changes -> fresh process -> counter resets to 1.
        std::fs::write(
            data.join("mcp.json"),
            serde_json::to_string_pretty(&json!({
                "mcpServers": {
                    "counter": { "command": "sh", "args": ["-c", script], "timeout_s": 30 }
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let mgr3 = pool.get_or_connect(&cwd).await;
        assert!(!Arc::ptr_eq(&mgr1, &mgr3));
        let out3 = mgr3.call_tool("mcp__counter__bump", json!({})).await.unwrap();
        assert_eq!(out3, "count=1");

        pool.shutdown().await;
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn managed_mcp_precedence_and_fail_closed_on_malformed_json() {
        let root = tmpdir("managed");
        let data = root.join("data");
        let cwd = root.join("repo");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        set_workspace_trusted(&data, &cwd, true).unwrap();

        let managed = root.join("managed-mcp.json");
        std::fs::write(&managed, "{ broken json").unwrap();
        let bad_report = load_mcp_configs_with_managed(&data, &cwd, Some(&managed));
        assert!(bad_report.managed_error.is_some());
        assert!(bad_report.servers.is_empty());
        let bad_mgr = McpManager::connect_report(bad_report, &cwd).await;
        assert!(bad_mgr.managed_error().is_some());
        assert!(bad_mgr.tools().is_empty());

        std::fs::write(
            &managed,
            serde_json::to_string(&json!({
                "mcpServers": {
                    "corp": { "command": "echo", "args": ["managed"] }
                }
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            data.join("mcp.json"),
            serde_json::to_string(&json!({
                "mcpServers": {
                    "corp": { "command": "echo", "args": ["user-override-attempt"] },
                    "personal": { "command": "echo", "args": ["user"] }
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let ok_report = load_mcp_configs_with_managed(&data, &cwd, Some(&managed));
        assert!(ok_report.managed_error.is_none());
        let corp = ok_report.servers.iter().find(|s| s.name == "corp").unwrap();
        assert_eq!(corp.scope, McpScope::Managed);
        let personal = ok_report
            .servers
            .iter()
            .find(|s| s.name == "personal")
            .unwrap();
        assert_eq!(personal.scope, McpScope::User);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn deferred_mcp_search_activates_matching_tools_for_turn() {
        let root = tmpdir("deferred");
        let data = root.join("data");
        let cwd = root.join("repo");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();

        let script = r#"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-03-26","capabilities":{"tools":{}}}}\n' "$id"
      ;;
    *'"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"alpha_read","description":"Read alpha docs","inputSchema":{"type":"object"}},{"name":"beta_deploy","description":"Deploy beta service","inputSchema":{"type":"object"}},{"name":"gamma_sql","description":"Query postgres","inputSchema":{"type":"object"}}]}}\n' "$id"
      ;;
  esac
done
"#;
        std::fs::write(
            data.join("mcp.json"),
            serde_json::to_string(&json!({
                "mcpServers": {
                    "big": { "command": "sh", "args": ["-c", script] }
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let mgr = Arc::new(McpManager::connect_configured(&data, &cwd).await);
        // Set threshold to 2 so 3 tools trigger deferred mode.
        let view = McpTurnView::with_threshold(mgr, 2);
        assert!(view.is_deferred());
        let before = view.extra_tool_schemas();
        assert_eq!(before.len(), 1);
        assert_eq!(before[0]["function"]["name"], "mcp_search");

        let reply = view
            .search_and_activate(&json!({ "query": "postgres" }))
            .unwrap();
        assert!(reply.contains("mcp__big__gamma_sql"));

        let after = view.extra_tool_schemas();
        assert_eq!(after.len(), 2);
        assert!(after
            .iter()
            .any(|s| s["function"]["name"] == "mcp__big__gamma_sql"));

        let _ = std::fs::remove_dir_all(&root);
    }
}
