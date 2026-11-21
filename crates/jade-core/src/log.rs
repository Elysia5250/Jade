//! Append-only job output logs.
//!
//! One file per job, written as self-describing records:
//!
//! ```text
//! [fd: u8][len: u32 BE][payload: len bytes]
//! ```
//!
//! `fd` keeps stdout/stderr distinguishable after the fact, and the record
//! boundary is the resume cursor, so a follower can disconnect and reattach at
//! the last `seq` without losing or duplicating output.

use jade_proto::{LogRecord, AGENT_CHUNK};
use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Notify;

/// Payload bytes kept per job; the rest is dropped (and reported as truncated).
pub const MAX_LOG_BYTES: u64 = 16 * 1024 * 1024;
/// `fd` + length header of one record.
pub const RECORD_HEADER: u64 = 5;
/// Largest record payload accepted when reading (guards corruption).
const MAX_RECORD: usize = 2 * AGENT_CHUNK;

/// Directory layout: `<data_dir>/jobs/<id>.log`.
#[derive(Debug, Clone)]
pub struct JobLogs {
    pub root: PathBuf,
}

impl JobLogs {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn dir(&self) -> PathBuf {
        self.root.join("jobs")
    }

    pub fn path(&self, id: u64) -> PathBuf {
        self.dir().join(format!("{id}.log"))
    }

    /// Delete log files whose ids are not in `keep` and not reported live.
    ///
    /// `still_live` closes the race where a job is submitted after `keep` was
    /// snapshotted but before this scan runs. Evicted ids are removed
    /// immediately; there is no grace period that retains multi-megabyte logs.
    pub fn prune(&self, keep: &[u64], still_live: impl Fn(u64) -> bool) {
        let keep: HashSet<u64> = keep.iter().copied().collect();
        let Ok(rd) = std::fs::read_dir(self.dir()) else {
            return;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let Ok(id) = stem.parse::<u64>() else {
                continue; // state.json and anything else
            };
            if keep.contains(&id) || still_live(id) {
                continue;
            }
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Single writer for a job's log. Cheap to hold across the job's lifetime.
pub struct LogWriter {
    file: std::fs::File,
    path: PathBuf,
    /// Payload bytes recorded so far (the cap applies to payload, not headers).
    payload: u64,
    notify: Arc<Notify>,
    truncated: bool,
}

impl LogWriter {
    pub fn create(path: PathBuf, notify: Arc<Notify>) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)?;
        Ok(Self {
            file,
            path,
            payload: 0,
            notify,
            truncated: false,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one output chunk. Payloads larger than [`MAX_RECORD`] are split
    /// into consecutive records so the length header stays honest. Output
    /// beyond [`MAX_LOG_BYTES`] is dropped; a single truncation notice is
    /// appended so readers know why it stops.
    pub fn record(&mut self, fd: u8, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let mut rest = data;
        while !rest.is_empty() {
            if self.payload >= MAX_LOG_BYTES {
                self.note_truncated();
                break;
            }
            let room = (MAX_LOG_BYTES - self.payload) as usize;
            let take = room.min(rest.len()).min(MAX_RECORD);
            if take == 0 || self.write_record(fd, &rest[..take]).is_err() {
                break;
            }
            self.payload += take as u64;
            rest = &rest[take..];
        }
        if self.payload >= MAX_LOG_BYTES {
            self.note_truncated();
        }
        self.notify.notify_waiters();
    }

    fn note_truncated(&mut self) {
        if self.truncated {
            return;
        }
        self.truncated = true;
        let note = b"\n[log truncated: cap reached]\n";
        let _ = self.write_record(1, note);
    }

    fn write_record(&mut self, fd: u8, data: &[u8]) -> std::io::Result<()> {
        let len = data.len() as u32;
        let mut header = [0u8; 5];
        header[0] = fd;
        header[1..].copy_from_slice(&len.to_be_bytes());
        self.file.write_all(&header)?;
        self.file.write_all(data)?;
        self.file.flush()?;
        Ok(())
    }

    pub fn payload_bytes(&self) -> u64 {
        self.payload
    }
}

/// Read at most `max_bytes` of payload starting at record boundary `offset`.
///
/// Returns the records and the cursor for the next call. A partially written
/// trailing record is left untouched so the next call re-reads it whole.
pub fn read_from(
    path: &Path,
    offset: u64,
    max_bytes: usize,
) -> std::io::Result<(Vec<LogRecord>, u64)> {
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), offset)),
        Err(e) => return Err(e),
    };
    if file.seek(SeekFrom::Start(offset)).is_err() {
        return Ok((Vec::new(), offset));
    }

    let mut records = Vec::new();
    let mut cursor = offset;
    let mut budget = max_bytes;
    loop {
        if budget == 0 && !records.is_empty() {
            break;
        }
        let mut header = [0u8; 5];
        if !read_exact_or_eof(&mut file, &mut header)? {
            break;
        }
        let fd = header[0];
        let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
        if len > MAX_RECORD {
            break; // corrupt or not ours; stop rather than allocate
        }
        let mut data = vec![0u8; len];
        if !read_exact_or_eof(&mut file, &mut data)? {
            break; // payload still in flight
        }
        cursor += RECORD_HEADER + len as u64;
        budget = budget.saturating_sub(len);
        records.push(LogRecord {
            seq: cursor - RECORD_HEADER - len as u64,
            fd,
            data,
        });
    }
    Ok((records, cursor))
}

/// `read_exact`, but `false` on a clean EOF at a record boundary.
fn read_exact_or_eof<R: Read>(r: &mut R, buf: &mut [u8]) -> std::io::Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) => return Ok(false),
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("jade-log-{tag}-{n}"));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn records_roundtrip_with_cursor() {
        let dir = tmpdir("roundtrip");
        let path = dir.join("1.log");
        let wake = Arc::new(Notify::new());
        {
            let mut w = LogWriter::create(path.clone(), wake).unwrap();
            w.record(0, b"hello ");
            w.record(1, b"oops");
        }
        let (recs, next) = read_from(&path, 0, 4096).unwrap();
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].data, b"hello ");
        assert_eq!(recs[0].fd, 0);
        assert_eq!(recs[1].fd, 1);
        assert!(recs[1].seq > recs[0].seq);

        // Reading from the cursor yields nothing new.
        let (empty, same) = read_from(&path, next, 4096).unwrap();
        assert!(empty.is_empty());
        assert_eq!(same, next);

        // Resuming from the second record's cursor returns only that record.
        let (tail, _) = read_from(&path, recs[1].seq, 4096).unwrap();
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].data, b"oops");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_partial_trailing_record_is_not_returned() {
        let dir = tmpdir("partial");
        let path = dir.join("2.log");
        let wake = Arc::new(Notify::new());
        {
            let mut w = LogWriter::create(path.clone(), wake).unwrap();
            w.record(0, b"complete");
        }
        // Simulate a writer caught mid-record: header claims 10 bytes, only 3 exist.
        {
            use std::io::Write as _;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            f.write_all(&[0u8]).unwrap();
            f.write_all(&10u32.to_be_bytes()).unwrap();
            f.write_all(b"abc").unwrap();
        }
        let (recs, next) = read_from(&path, 0, 4096).unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].data, b"complete");
        assert_eq!(next, recs[0].seq + RECORD_HEADER + 8);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn oversized_payload_is_split_into_readable_records() {
        let dir = tmpdir("split");
        let path = dir.join("3.log");
        let wake = Arc::new(Notify::new());
        let mut payload = vec![0xABu8; MAX_RECORD + 100];
        payload[0] = 1;
        payload[MAX_RECORD] = 2;
        {
            let mut w = LogWriter::create(path.clone(), wake).unwrap();
            w.record(0, &payload);
        }
        let (recs, _) = read_from(&path, 0, payload.len() + 64).unwrap();
        assert!(recs.len() >= 2, "expected a split, got {}", recs.len());
        let joined: Vec<u8> = recs.iter().flat_map(|r| r.data.clone()).collect();
        assert_eq!(joined, payload);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_deletes_evicted_logs_immediately() {
        let dir = tmpdir("prune");
        let logs = JobLogs::new(dir.clone());
        std::fs::create_dir_all(logs.dir()).unwrap();
        std::fs::write(logs.path(1), b"keep").unwrap();
        std::fs::write(logs.path(2), b"drop").unwrap();
        logs.prune(&[1], |_| false);
        assert!(logs.path(1).exists());
        assert!(!logs.path(2).exists(), "evicted log must not linger");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
