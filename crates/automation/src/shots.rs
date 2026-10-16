//! Where bridge screenshots land. The app writes each PNG to a path the server names, and the
//! server reads it back. The paths are in one private directory per server: a random name, made
//! with `create_dir` (never an existing one), mode 0700 on unix, so another local user can neither
//! predict nor pre-create them (a symlink there would make the app write elsewhere, or feed the
//! server another file). Each file is removed whatever happens to its call, and read back capped.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

/// Largest screenshot read back.
pub(crate) const MAX_SHOT_BYTES: u64 = 64 << 20;

/// The server's screenshot directory, made on first use and removed when the server (its last
/// clone) is dropped.
#[derive(Default)]
pub(crate) struct ShotDir {
    dir: Mutex<Option<PathBuf>>,
    next: AtomicU64,
}

impl ShotDir {
    /// A new file name in the directory; the file is removed when the returned guard drops.
    pub(crate) fn file(&self) -> std::io::Result<ShotFile> {
        let mut dir = self.dir.lock().unwrap_or_else(PoisonError::into_inner);
        let d = match &*dir {
            Some(d) => d.clone(),
            None => dir.insert(private_dir(&std::env::temp_dir())?).clone(),
        };
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        Ok(ShotFile(d.join(format!("shot-{n}.png"))))
    }
}

impl Drop for ShotDir {
    fn drop(&mut self) {
        if let Some(d) = self.dir.get_mut().unwrap_or_else(PoisonError::into_inner).take() {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}

/// A screenshot's path; dropping it removes the file (a call that failed or was cancelled
/// included).
pub(crate) struct ShotFile(PathBuf);

impl ShotFile {
    pub(crate) fn path(&self) -> &Path {
        &self.0
    }

    /// The file's bytes: a regular file of at most [`MAX_SHOT_BYTES`].
    pub(crate) fn read(&self) -> std::io::Result<Vec<u8>> {
        let meta = std::fs::symlink_metadata(&self.0)?;
        if !meta.is_file() {
            return Err(std::io::Error::other("not a regular file"));
        }
        let too_big = || std::io::Error::other(format!("larger than {} MiB", MAX_SHOT_BYTES >> 20));
        if meta.len() > MAX_SHOT_BYTES {
            return Err(too_big());
        }
        let mut out = Vec::new();
        std::fs::File::open(&self.0)?.take(MAX_SHOT_BYTES.saturating_add(1)).read_to_end(&mut out)?;
        if out.len() as u64 > MAX_SHOT_BYTES {
            return Err(too_big());
        }
        Ok(out)
    }
}

impl Drop for ShotFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A new directory in `parent` that only this user can enter, under a name nobody can guess.
fn private_dir(parent: &Path) -> std::io::Result<PathBuf> {
    let mut last = std::io::Error::other("no free name");
    for _ in 0..8 {
        let path = parent.join(format!("filmcraft-mcp-{}-{:016x}", std::process::id(), random_u64()));
        match owner_only().create(&path) {
            Ok(()) => return Ok(path),
            // taken (planted, or a rare clash): try another name, never reuse it
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last = e,
            Err(e) => return Err(e),
        }
    }
    Err(last)
}

/// A `DirBuilder` for a directory only its owner can enter: mode 0700 on unix. (On Windows the
/// user's temp directory is already private to them.)
fn owner_only() -> std::fs::DirBuilder {
    let builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    let builder = {
        let mut b = builder;
        std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
        b
    };
    builder
}

/// 64 unpredictable bits: the standard library seeds `RandomState` from the operating system's
/// random source.
fn random_u64() -> u64 {
    use std::hash::BuildHasher;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    std::hash::RandomState::new().hash_one(now)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shots_live_in_a_private_directory_and_are_removed() {
        let shots = ShotDir::default();
        let a = shots.file().unwrap();
        let b = shots.file().unwrap();
        let dir = a.path().parent().unwrap().to_path_buf();
        assert_eq!(b.path().parent(), Some(dir.as_path()), "one directory per server");
        assert_ne!(a.path(), b.path());
        assert_ne!(dir, std::env::temp_dir(), "not in the shared temp directory itself");
        assert!(dir.file_name().unwrap().to_string_lossy().starts_with(&format!("filmcraft-mcp-{}-", std::process::id())));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        }
        std::fs::write(a.path(), b"png").unwrap();
        assert_eq!(a.read().unwrap(), b"png");
        let path = a.path().to_path_buf();
        drop(a);
        assert!(!path.exists(), "removed with its guard");
        // capped: a (sparse) file over the limit is refused without being read
        std::fs::File::create(b.path()).unwrap().set_len(MAX_SHOT_BYTES + 1).unwrap();
        assert!(b.read().unwrap_err().to_string().contains("larger than 64 MiB"));
        drop(b);
        drop(shots);
        assert!(!dir.exists(), "the directory goes with the server");
    }

    #[test]
    fn a_planted_name_is_never_reused() {
        let parent = std::env::temp_dir().join(format!("fc-private-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&parent);
        std::fs::create_dir_all(&parent).unwrap();
        let a = private_dir(&parent).unwrap();
        let b = private_dir(&parent).unwrap();
        assert_ne!(a, b, "random names");
        // a symlink or file under the next name would fail create_dir, not be followed
        assert!(std::fs::DirBuilder::new().create(&a).is_err());
        let _ = std::fs::remove_dir_all(&parent);
    }
}
