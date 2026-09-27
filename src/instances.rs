//! Cross-instance registry: which folder pair each running dirsync is using.
//!
//! Several GUI windows (and CLI runs) may work side by side, as long as they
//! stay out of each other's way: two mirrors writing into one destination
//! delete each other's files as orphans. Each instance records its pair in
//! `<dir>/<id>.json` and holds an OS file lock on `<dir>/<id>.lock` for as long
//! as it lives. An entry whose lock can be taken belongs to a process that is
//! gone (crashed or killed) and is removed, so a crash never blocks a folder
//! for good. Nothing is ever written into SRC or DST.

use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// The folder pair one instance is working on (canonical paths).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub src: PathBuf,
    pub dst: PathBuf,
}

/// Why a claim was refused.
#[derive(Debug)]
pub enum ClaimError {
    /// Another live instance uses an overlapping folder.
    Conflict {
        /// Which of our endpoints collides: `"SRC"` or `"DST"`.
        side: &'static str,
        path: PathBuf,
        theirs: Entry,
    },
    /// The registry itself could not be read or written.
    Io(io::Error),
}

impl std::fmt::Display for ClaimError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClaimError::Conflict { side, path, theirs } => write!(
                f,
                "{side} {} is in use by another dirsync instance ({} -> {})",
                crate::paths::display_path(path),
                crate::paths::display_path(&theirs.src),
                crate::paths::display_path(&theirs.dst),
            ),
            ClaimError::Io(e) => write!(f, "instance registry unavailable: {e}"),
        }
    }
}

impl std::error::Error for ClaimError {}

impl From<io::Error> for ClaimError {
    fn from(e: io::Error) -> Self {
        ClaimError::Io(e)
    }
}

/// `a` and `b` are the same folder or one contains the other.
fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

/// Whether mirroring `src -> dst` would interfere with `theirs`. Writing
/// (our DST) must stay clear of everything they touch; reading (our SRC)
/// only of what they write. Two instances reading one SRC is fine.
fn conflict(src: &Path, dst: &Path, theirs: &Entry) -> Option<ClaimError> {
    let side = if overlaps(dst, &theirs.dst) || overlaps(dst, &theirs.src) {
        ("DST", dst)
    } else if overlaps(src, &theirs.dst) {
        ("SRC", src)
    } else {
        return None;
    };
    Some(ClaimError::Conflict {
        side: side.0,
        path: side.1.to_path_buf(),
        theirs: theirs.clone(),
    })
}

fn lock_error(e: std::fs::TryLockError) -> io::Error {
    match e {
        std::fs::TryLockError::Error(e) => e,
        std::fs::TryLockError::WouldBlock => io::Error::from(io::ErrorKind::WouldBlock),
    }
}

/// This process's membership in the registry.
pub struct Registry {
    dir: PathBuf,
    id: String,
    /// The liveness lock, held for the instance's lifetime. `None` only after
    /// `forget_without_cleanup`.
    lock: Mutex<Option<File>>,
}

impl Registry {
    /// `locks/` next to the config file (`DIRSYNC_CONFIG` moves it too).
    pub fn default_dir() -> Option<PathBuf> {
        let config = crate::config::AppConfig::config_path()?;
        Some(config.parent()?.join("locks"))
    }

    /// Join the registry in `dir`, creating it if needed.
    pub fn open(dir: &Path) -> io::Result<Self> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        fs::create_dir_all(dir)?;
        let id = format!(
            "{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join(format!("{id}.lock")))?;
        lock.try_lock().map_err(lock_error)?;
        Ok(Self {
            dir: dir.to_path_buf(),
            id,
            lock: Mutex::new(Some(lock)),
        })
    }

    /// Record `src -> dst` (canonical paths) as this instance's pair, unless
    /// another live instance uses an overlapping folder. Replaces this
    /// instance's previous pair. Check and write happen under one registry
    /// lock, so two instances claiming at the same moment cannot both win.
    pub fn claim(&self, src: &Path, dst: &Path) -> Result<(), ClaimError> {
        let _guard = self.registry_lock()?;
        for (id, theirs) in self.live_entries()? {
            if id == self.id {
                continue;
            }
            if let Some(e) = conflict(src, dst, &theirs) {
                return Err(e);
            }
        }
        let entry = Entry {
            src: src.to_path_buf(),
            dst: dst.to_path_buf(),
        };
        let json = serde_json::to_vec(&entry).map_err(io::Error::other)?;
        let tmp = self.dir.join(format!("{}.json.tmp", self.id));
        fs::write(&tmp, json)?;
        fs::rename(&tmp, self.entry_path())?;
        Ok(())
    }

    /// Give up this instance's pair (it stays registered as alive).
    pub fn release(&self) {
        if let Ok(_guard) = self.registry_lock() {
            let _ = fs::remove_file(self.entry_path());
        }
    }

    /// Test hook: behave like a process that crashed, releasing the OS lock
    /// but leaving its files behind for the next claimant to clean up.
    #[doc(hidden)]
    pub fn forget_without_cleanup(self) {
        drop(self.lock.lock().unwrap().take());
        std::mem::forget(self);
    }

    fn entry_path(&self) -> PathBuf {
        self.dir.join(format!("{}.json", self.id))
    }

    /// Serializes check-and-claim across instances; released on drop.
    fn registry_lock(&self) -> io::Result<File> {
        let f = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join("registry.lock"))?;
        f.lock()?;
        Ok(f)
    }

    /// Every entry whose owner is still alive; stale ones are deleted on the
    /// way. Caller holds the registry lock.
    fn live_entries(&self) -> io::Result<Vec<(String, Entry)>> {
        let mut live = Vec::new();
        for dirent in fs::read_dir(&self.dir)?.flatten() {
            let path = dirent.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|s| s.to_str()).map(str::to_owned) else {
                continue;
            };
            if id != self.id && !self.is_alive(&id) {
                let _ = fs::remove_file(&path);
                let _ = fs::remove_file(self.dir.join(format!("{id}.lock")));
                continue;
            }
            // A torn or foreign file is not a claim: ignore it.
            if let Ok(entry) = fs::read(&path)
                .map_err(|_| ())
                .and_then(|b| serde_json::from_slice::<Entry>(&b).map_err(|_| ()))
            {
                live.push((id, entry));
            }
        }
        Ok(live)
    }

    /// The owner of `id` still holds its lock file.
    fn is_alive(&self, id: &str) -> bool {
        let Ok(f) = OpenOptions::new()
            .write(true)
            .open(self.dir.join(format!("{id}.lock")))
        else {
            return false; // no lock file at all: nobody owns the entry
        };
        match f.try_lock() {
            Ok(()) => false, // we could take it: the owner is gone
            Err(std::fs::TryLockError::WouldBlock) => true,
            // Unknown state: err on the side of keeping the folder blocked.
            Err(std::fs::TryLockError::Error(_)) => true,
        }
    }
}

impl Drop for Registry {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.entry_path());
        if let Some(lock) = self.lock.lock().unwrap().take() {
            drop(lock);
            let _ = fs::remove_file(self.dir.join(format!("{}.lock", self.id)));
        }
    }
}
