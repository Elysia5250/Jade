//! Per-job output tee logs and retention pruning.

use std::path::{Path, PathBuf};

/// Job logs kept on disk.
pub const KEEP_JOBS: usize = 50;
/// Bytes of one job's output written to disk.
pub const JOB_LOG_CAP: u64 = 16 * 1024 * 1024;

/// Output tee, so a dropped connection does not lose the log.
pub struct JobLog {
    file: Option<std::fs::File>,
    written: u64,
}

impl JobLog {
    pub fn create(root: &Path, pid: u32) -> Self {
        let dir = root.join("agent").join("jobs");
        if std::fs::create_dir_all(&dir).is_err() {
            return Self {
                file: None,
                written: 0,
            };
        }
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(dir.join(format!("{ts}-{pid}.log")))
            .ok();
        Self { file, written: 0 }
    }

    pub fn write(&mut self, fd: u8, data: &[u8]) {
        let Some(f) = self.file.as_mut() else { return };
        if self.written >= JOB_LOG_CAP {
            return;
        }
        use std::io::Write as _;
        if fd == 1 {
            let _ = f.write_all(b"[stderr] ");
        }
        let take = (JOB_LOG_CAP - self.written).min(data.len() as u64) as usize;
        let _ = f.write_all(&data[..take]);
        self.written += take as u64;
    }

    pub fn finish(mut self, code: i32) {
        if let Some(f) = self.file.as_mut() {
            use std::io::Write as _;
            let _ = writeln!(f, "\n[exit {code}]");
        }
    }
}

pub fn prune_jobs(root: &Path) {
    let dir = root.join("agent").join("jobs");
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return;
    };
    let mut files: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
    if files.len() <= KEEP_JOBS {
        return;
    }
    files.sort();
    let excess = files.len() - KEEP_JOBS;
    for f in files.into_iter().take(excess) {
        let _ = std::fs::remove_file(f);
    }
}
