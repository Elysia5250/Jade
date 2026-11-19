//! Per-turn workspace + conversation checkpoints and `/rewind`.

use crate::session::ChatMessage;
use anyhow::{bail, Context, Result};
use envoy_proto::{SubmitRequest, TaskSpec, DEFAULT_GROUP};
use envoy_service::{AgentDialer, Remote};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_CHECKPOINTS_PER_SESSION: usize = 30;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointSummary {
    pub id: u64,
    pub label: String,
    pub ts_ms: u64,
    pub remaining_user_turns: usize,
    pub files_touched: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileBaseline {
    pub existed: bool,
    pub content: String,
    pub restorable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub id: u64,
    pub label: String,
    pub ts_ms: u64,
    pub messages: Vec<ChatMessage>,
    pub remaining_user_turns: usize,
    pub file_baselines: BTreeMap<String, FileBaseline>,
}

impl Checkpoint {
    pub fn summary(&self) -> CheckpointSummary {
        CheckpointSummary {
            id: self.id,
            label: self.label.clone(),
            ts_ms: self.ts_ms,
            remaining_user_turns: self.remaining_user_turns,
            files_touched: self.file_baselines.len(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct PersistedCheckpoints {
    next_id: u64,
    checkpoints: Vec<Checkpoint>,
}

#[derive(Debug)]
pub struct CheckpointStore {
    data_dir: PathBuf,
    session_id: String,
    next_id: u64,
    checkpoints: Vec<Checkpoint>,
}

impl CheckpointStore {
    pub fn load(data_dir: PathBuf, session_id: String) -> Self {
        let path = store_path(&data_dir, &session_id);
        let persisted: PersistedCheckpoints = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default();
        let next_id = persisted.next_id.max(
            persisted
                .checkpoints
                .iter()
                .map(|c| c.id + 1)
                .max()
                .unwrap_or(1),
        );
        Self {
            data_dir,
            session_id,
            next_id: next_id.max(1),
            checkpoints: persisted.checkpoints,
        }
    }

    pub fn begin_turn(
        &mut self,
        prompt: &str,
        messages: Vec<ChatMessage>,
        remaining_user_turns: usize,
    ) -> u64 {
        let id = self.next_id.max(1);
        self.next_id = id + 1;
        let cp = Checkpoint {
            id,
            label: compact_label(prompt),
            ts_ms: now_ms(),
            messages,
            remaining_user_turns,
            file_baselines: BTreeMap::new(),
        };
        self.checkpoints.push(cp);
        if self.checkpoints.len() > MAX_CHECKPOINTS_PER_SESSION {
            let excess = self.checkpoints.len() - MAX_CHECKPOINTS_PER_SESSION;
            self.checkpoints.drain(..excess);
        }
        let _ = self.save();
        id
    }

    pub fn record_file_change(
        &mut self,
        checkpoint_id: u64,
        path: String,
        existed_before: bool,
        before: String,
        restorable_before: bool,
    ) {
        let Some(cp) = self.checkpoints.iter_mut().find(|c| c.id == checkpoint_id) else {
            return;
        };
        cp.file_baselines.entry(path).or_insert(FileBaseline {
            existed: existed_before,
            content: before,
            restorable: restorable_before || !existed_before,
        });
        let _ = self.save();
    }

    pub fn list_summaries(&self) -> Vec<CheckpointSummary> {
        self.checkpoints.iter().map(Checkpoint::summary).collect()
    }

    /// Compute the net per-file baseline across `[target_idx..]` in reverse chronological order.
    pub fn plan_rewind(
        &self,
        checkpoint_id: Option<u64>,
    ) -> Result<(usize, BTreeMap<String, FileBaseline>)> {
        if self.checkpoints.is_empty() {
            bail!("no checkpoints recorded for this session yet");
        }
        let idx = match checkpoint_id {
            Some(id) => self
                .checkpoints
                .iter()
                .position(|c| c.id == id)
                .with_context(|| format!("checkpoint #{id} not found"))?,
            None => self.checkpoints.len() - 1,
        };

        let mut net: BTreeMap<String, FileBaseline> = BTreeMap::new();
        for cp in self.checkpoints[idx..].iter().rev() {
            for (path, base) in &cp.file_baselines {
                net.insert(path.clone(), base.clone());
            }
        }
        Ok((idx, net))
    }

    pub fn commit_rewind(&mut self, idx: usize) -> Checkpoint {
        let target = self.checkpoints[idx].clone();
        self.checkpoints.truncate(idx);
        let _ = self.save();
        target
    }

    fn save(&self) -> Result<()> {
        let dir = checkpoints_dir(&self.data_dir);
        std::fs::create_dir_all(&dir)?;
        let tmp_dir = self.data_dir.join("tmp");
        std::fs::create_dir_all(&tmp_dir)?;
        let payload = PersistedCheckpoints {
            next_id: self.next_id,
            checkpoints: self.checkpoints.clone(),
        };
        let data = serde_json::to_string(&payload)?;
        let tmp = tmp_dir.join(format!("cp-{}.json.tmp", self.session_id));
        std::fs::write(&tmp, data)?;
        std::fs::rename(&tmp, store_path(&self.data_dir, &self.session_id))?;
        Ok(())
    }
}

pub async fn apply_file_baselines(
    remote: &Remote<Box<dyn AgentDialer>>,
    target: &str,
    net: &BTreeMap<String, FileBaseline>,
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let local = target.is_empty() || target == "local";
    let mut restored_files = Vec::new();
    let mut deleted_files = Vec::new();
    let mut skipped_files = Vec::new();

    for (path, base) in net {
        if base.existed && !base.restorable {
            skipped_files.push(format!(
                "{path} (file exceeded snapshot cap or was non-UTF8)"
            ));
            continue;
        }
        if base.existed {
            if local {
                let p = Path::new(path);
                if let Some(parent) = p.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                match std::fs::write(p, base.content.as_bytes()) {
                    Ok(()) => restored_files.push(path.clone()),
                    Err(e) => skipped_files.push(format!("{path} ({e})")),
                }
            } else {
                match remote
                    .write(target, path, base.content.as_bytes(), None)
                    .await
                {
                    Ok(_) => restored_files.push(path.clone()),
                    Err(e) => skipped_files.push(format!("{path} ({e:#})")),
                }
            }
        } else if local {
            let p = Path::new(path);
            if p.symlink_metadata().is_ok() {
                match std::fs::remove_file(p) {
                    Ok(()) => deleted_files.push(path.clone()),
                    Err(e) => skipped_files.push(format!("{path} ({e})")),
                }
            } else {
                deleted_files.push(path.clone());
            }
        } else {
            let escaped = shell_single_quote(path);
            let req = SubmitRequest {
                spec: TaskSpec::Shell {
                    command: format!("rm -f -- {escaped}"),
                },
                cwd: None,
                env: Vec::new(),
                label: Some("checkpoint-rewind-rm".into()),
                group: DEFAULT_GROUP.to_string(),
                timeout_s: 15,
                sandbox: envoy_proto::SandboxPolicy::Off,
            };
            match remote.submit(target, req).await {
                Ok(job) => {
                    let mut ok = false;
                    for _ in 0..30 {
                        if let Ok(st) = remote.status(target, job.id).await {
                            if !matches!(
                                st.state,
                                envoy_proto::JobState::Queued { .. }
                                    | envoy_proto::JobState::Running { .. }
                            ) {
                                ok = matches!(
                                    st.state,
                                    envoy_proto::JobState::Done {
                                        result: envoy_proto::JobResult::Success,
                                        ..
                                    }
                                );
                                break;
                            }
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                    if ok {
                        deleted_files.push(path.clone());
                    } else {
                        skipped_files
                            .push(format!("{path} (remote remove did not complete cleanly)"));
                    }
                }
                Err(e) => skipped_files.push(format!("{path} ({e:#})")),
            }
        }
    }

    (restored_files, deleted_files, skipped_files)
}

fn checkpoints_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("checkpoints")
}

fn store_path(data_dir: &Path, session_id: &str) -> PathBuf {
    let safe_id: String = session_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    checkpoints_dir(data_dir).join(format!("{safe_id}.json"))
}

fn compact_label(prompt: &str) -> String {
    let first = prompt
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("(turn)");
    let truncated: String = first.chars().take(48).collect();
    if first.chars().count() > 48 {
        format!("{truncated}…")
    } else {
        truncated
    }
}

fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::ChatRole;

    fn tmpdir(tag: &str) -> PathBuf {
        let n = now_ms();
        let p = std::env::temp_dir().join(format!("envoy-cp-{tag}-{n}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn reverse_fold_preserves_first_touch_per_turn_and_deletes_created_files() {
        let dir = tmpdir("fold");
        let mut store = CheckpointStore::load(dir.clone(), "s1".into());
        let msg0 = vec![];
        let c1 = store.begin_turn("first prompt", msg0, 0);
        // Edit file A twice in turn 1 -> baseline stays "v0"
        store.record_file_change(c1, "/w/a.txt".into(), true, "v0".into(), true);
        store.record_file_change(c1, "/w/a.txt".into(), true, "v1".into(), true);

        let msg1 = vec![ChatMessage {
            role: ChatRole::User,
            content: "first prompt".into(),
            reasoning: None,
            tool_calls: vec![],
            tool_call_id: None,
            tool_name: None,
        }];
        let c2 = store.begin_turn("second prompt", msg1, 1);
        store.record_file_change(c2, "/w/a.txt".into(), true, "v2".into(), true);
        store.record_file_change(c2, "/w/new.txt".into(), false, String::new(), true);

        let c3 = store.begin_turn("third prompt", vec![], 2);
        store.record_file_change(c3, "/w/new.txt".into(), true, "created in c2".into(), true);

        // Rewinding to c2 should restore a.txt to "v2" and delete new.txt (because c2 created it).
        let (idx, net) = store.plan_rewind(Some(c2)).unwrap();
        assert_eq!(idx, 1);
        assert_eq!(net["/w/a.txt"].content, "v2");
        assert!(net["/w/a.txt"].existed);
        assert!(!net["/w/new.txt"].existed);

        // Reload from disk and verify persistence.
        let reloaded = CheckpointStore::load(dir.clone(), "s1".into());
        assert_eq!(reloaded.list_summaries().len(), 3);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn apply_file_baselines_and_reverse_fold_edge_cases() {
        let dir = tmpdir("apply");
        let existing = dir.join("keep.txt");
        let created = dir.join("created.txt");
        let big = dir.join("big.bin");
        std::fs::write(&existing, "modified").unwrap();
        std::fs::write(&created, "new file").unwrap();
        std::fs::write(&big, "now small").unwrap();

        let mut store = CheckpointStore::load(dir.clone(), "s2".into());
        let c1 = store.begin_turn("t1", vec![], 0);
        store.record_file_change(
            c1,
            existing.display().to_string(),
            true,
            "original".into(),
            true,
        );
        store.record_file_change(
            c1,
            created.display().to_string(),
            false,
            String::new(),
            true,
        );
        store.record_file_change(c1, big.display().to_string(), true, String::new(), false);

        // Turn 2 later became non-restorable on keep.txt, but Turn 1 captured "original".
        let c2 = store.begin_turn("t2", vec![], 1);
        store.record_file_change(
            c2,
            existing.display().to_string(),
            true,
            String::new(),
            false,
        );

        let (_idx, net) = store.plan_rewind(Some(c1)).unwrap();
        assert!(net[&existing.display().to_string()].restorable);
        assert_eq!(net[&existing.display().to_string()].content, "original");
        assert!(!net[&big.display().to_string()].restorable);

        let remote = Remote::new(envoy_service::dialer_for(&dir, "local", None));
        let (restored, deleted, skipped) = apply_file_baselines(&remote, "local", &net).await;
        assert_eq!(restored, vec![existing.display().to_string()]);
        assert_eq!(deleted, vec![created.display().to_string()]);
        assert_eq!(skipped.len(), 1);
        assert_eq!(std::fs::read_to_string(&existing).unwrap(), "original");
        assert!(!created.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
