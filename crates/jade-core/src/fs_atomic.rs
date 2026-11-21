//! Directory-fd pinned atomic file writes (`openat` + `fchmod` + `renameat`).
//!
//! Pinning the canonical parent directory via an `O_DIRECTORY | O_NOFOLLOW`
//! file descriptor closes the parent-directory symlink swap TOCTOU window
//! between temp-file creation, `chmod`, and `rename` into place.

use anyhow::{bail, Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(unix)]
use std::ffi::CString;
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;

/// A staged temporary file pinned to its parent directory.
///
/// Dropping a `PinnedWrite` without calling [`Self::commit`] automatically
/// unlinks the staged `.part` file inside the pinned directory.
pub struct PinnedWrite {
    file: Option<std::fs::File>,
    #[cfg(unix)]
    dirfd: OwnedFd,
    #[cfg(unix)]
    part_name: CString,
    #[cfg(unix)]
    target_name: CString,
    #[cfg(not(unix))]
    part_path: PathBuf,
    #[cfg(not(unix))]
    target_path: PathBuf,
    committed: bool,
}

impl PinnedWrite {
    /// Create a unique `.part` file inside `path`'s parent directory with
    /// `O_CREAT | O_EXCL | O_NOFOLLOW` and `mode` (defaults to `0o600`).
    pub fn create(path: &Path, mode: Option<u32>) -> Result<Self> {
        let target_os = path
            .file_name()
            .filter(|n| !n.is_empty())
            .with_context(|| format!("missing file name in {}", path.display()))?;
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating parent dir {}", parent.display()))?;

        let part_file_name = unique_part_name(&target_os.to_string_lossy());
        let create_mode = mode.unwrap_or(0o600);

        #[cfg(unix)]
        {
            let parent_canon = std::fs::canonicalize(parent)
                .with_context(|| format!("canonicalizing parent dir {}", parent.display()))?;
            let parent_c = cstring_from_os(parent_canon.as_os_str())?;
            let dir_raw = unsafe {
                libc::open(
                    parent_c.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                )
            };
            if dir_raw < 0 {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("opening parent dir {}", parent_canon.display()));
            }
            // SAFETY: `dir_raw` is a newly opened valid file descriptor.
            let dirfd = unsafe { OwnedFd::from_raw_fd(dir_raw) };

            let target_name = cstring_from_os(target_os)?;
            let part_name = CString::new(part_file_name.as_bytes())
                .context("invalid temp file name containing NUL")?;
            let raw_fd = unsafe {
                libc::openat(
                    dirfd.as_raw_fd(),
                    part_name.as_ptr(),
                    libc::O_WRONLY
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC,
                    create_mode as libc::c_uint,
                )
            };
            if raw_fd < 0 {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("creating temp file in {}", parent_canon.display()));
            }
            // SAFETY: `raw_fd` is a newly opened valid file descriptor.
            let file = unsafe { std::fs::File::from_raw_fd(raw_fd) };
            Ok(Self {
                file: Some(file),
                dirfd,
                part_name,
                target_name,
                committed: false,
            })
        }
        #[cfg(not(unix))]
        {
            let part_path = parent.join(&part_file_name);
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&part_path)
                .with_context(|| format!("creating {}", part_path.display()))?;
            Ok(Self {
                file: Some(file),
                part_path,
                target_path: path.to_path_buf(),
                committed: false,
            })
        }
    }

    pub fn write_all(&mut self, data: &[u8]) -> Result<()> {
        let file = self.file.as_mut().context("staged file already closed")?;
        file.write_all(data).context("writing staged file")
    }

    /// Flush, apply `mode` via `fchmod` (when `Some`), and atomically rename
    /// into place inside the pinned parent directory.
    pub fn commit(mut self, mode: Option<u32>, sync_data: bool, strict_mode: bool) -> Result<()> {
        let mut file = self.file.take().context("staged file already closed")?;
        file.flush().context("flushing staged file")?;
        if sync_data {
            file.sync_all().context("syncing staged file")?;
        }
        #[cfg(unix)]
        if let Some(m) = mode {
            let rc = unsafe { libc::fchmod(file.as_raw_fd(), m as libc::mode_t) };
            if rc != 0 && strict_mode {
                let err = std::io::Error::last_os_error();
                drop(file);
                return Err(err).context("fchmod staged file");
            }
        }
        #[cfg(not(unix))]
        let _ = (mode, strict_mode);
        drop(file);

        #[cfg(unix)]
        {
            let rc = unsafe {
                libc::renameat(
                    self.dirfd.as_raw_fd(),
                    self.part_name.as_ptr(),
                    self.dirfd.as_raw_fd(),
                    self.target_name.as_ptr(),
                )
            };
            if rc != 0 {
                return Err(std::io::Error::last_os_error()).context("renameat staged file");
            }
            if sync_data {
                let _ = unsafe { libc::fsync(self.dirfd.as_raw_fd()) };
            }
        }
        #[cfg(not(unix))]
        {
            std::fs::rename(&self.part_path, &self.target_path)
                .with_context(|| format!("renaming into {}", self.target_path.display()))?;
        }
        self.committed = true;
        Ok(())
    }
}

impl Drop for PinnedWrite {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let _ = self.file.take();
        #[cfg(unix)]
        unsafe {
            libc::unlinkat(self.dirfd.as_raw_fd(), self.part_name.as_ptr(), 0);
        }
        #[cfg(not(unix))]
        {
            let _ = std::fs::remove_file(&self.part_path);
        }
    }
}

/// Write `data` atomically to `path` via a directory-fd pinned `.part` file.
pub fn write_atomic_pinned(
    path: &Path,
    data: &[u8],
    mode: Option<u32>,
    sync_data: bool,
    strict_mode: bool,
) -> Result<()> {
    let mut staged = PinnedWrite::create(path, mode)?;
    staged.write_all(data)?;
    staged.commit(mode, sync_data, strict_mode)
}

fn unique_part_name(target_name: &str) -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!(".{target_name}.part-{}-{seq}", std::process::id())
}

#[cfg(unix)]
fn cstring_from_os(s: &std::ffi::OsStr) -> Result<CString> {
    let bytes = s.as_bytes();
    if bytes.contains(&0) {
        bail!("path component contains NUL byte");
    }
    CString::new(bytes).context("converting path to CString")
}

/// Silence unused import on unix when PathBuf is only needed on non-unix.
#[allow(dead_code)]
fn _pathbuf_anchor(_: Option<PathBuf>) {}
