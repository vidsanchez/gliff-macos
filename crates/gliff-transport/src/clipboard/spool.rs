//! Private clipboard spool directories with conservative cross-process safety.
//! Only this instance's owned directory is removed on Drop. No liveness guess
//! or automatic crash-leftover sweep is performed on macOS.

use std::io;
use std::path::{Path, PathBuf};

pub struct Spool {
    dir: PathBuf,
}

impl Spool {
    /// Create a fresh spool directory under `GLIFF_CLIPBOARD_DIR`, else
    /// `$XDG_CACHE_HOME/gliff/clipboard`, else the system temp dir. It is
    /// disk-backed on purpose: the runtime dir is RAM.
    pub fn create() -> io::Result<Self> {
        Self::create_in(&default_base())
    }

    pub fn create_in(base: &Path) -> io::Result<Self> {
        std::fs::create_dir_all(base)?;
        // Pasted files can be private; the spool must not be readable by
        // other local users, whatever the umask.
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        let pid = std::process::id();
        for n in 0u32.. {
            let dir = base.join(format!("{pid}-{n}"));
            match builder.create(&dir) {
                Ok(()) => return Ok(Self { dir }),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        unreachable!()
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

impl Drop for Spool {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.dir) {
            if e.kind() != io::ErrorKind::NotFound {
                eprintln!("clipboard spool not removed ({}): {e}", self.dir.display());
            }
        }
    }
}

fn default_base() -> PathBuf {
    if let Some(d) = std::env::var_os("GLIFF_CLIPBOARD_DIR") {
        return PathBuf::from(d);
    }
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    cache.join("gliff").join("clipboard")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_base() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base =
            std::env::temp_dir().join(format!("gliff-spool-test-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&base).unwrap();
        base
    }

    #[test]
    fn creating_spools_never_removes_another_instances_files() {
        let base = test_base();
        let other = base.join("4294967295-0");
        std::fs::create_dir(&other).unwrap();
        std::fs::write(other.join("paste.txt"), b"in use").unwrap();
        let first = Spool::create_in(&base).unwrap();
        let first_dir = first.dir().to_path_buf();
        let second = Spool::create_in(&base).unwrap();
        assert_ne!(first.dir(), second.dir());
        assert!(first_dir.exists());
        assert_eq!(std::fs::read(other.join("paste.txt")).unwrap(), b"in use");
        drop(second);
        assert!(first_dir.exists());
        drop(first);
        assert!(!first_dir.exists());
        assert!(other.join("paste.txt").exists());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn spool_directories_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let base = test_base();
        let spool = Spool::create_in(&base).unwrap();
        assert_eq!(
            std::fs::metadata(spool.dir()).unwrap().permissions().mode() & 0o777,
            0o700
        );
        drop(spool);
        std::fs::remove_dir(&base).unwrap();
    }
}
