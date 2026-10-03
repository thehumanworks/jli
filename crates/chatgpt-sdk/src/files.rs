//! Private files: written whole and atomically, readable by the owner only, and locked while a
//! token is refreshed.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;

use crate::error::{Error, Result};

/// Create `dir` and its parents, readable by the owner only on Unix.
pub(crate) fn private_dir(dir: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir).map_err(|error| Error::io(format!("creating {}", dir.display()), error))
}

/// Replace `path` with `bytes` in one step: write a temporary file beside it (mode 0600 on Unix),
/// flush it to disk, then rename it over the original.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("file");
    let temporary = dir.join(format!(".{name}.{}.{}.tmp", std::process::id(), crate::auth::random_token(6)?));
    let context = || format!("writing {}", path.display());
    let written = (|| -> std::io::Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    written.map_err(|error| Error::io(context(), error))
}

/// Read `path`, or `None` when it does not exist.
pub(crate) fn read_optional(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(Error::io(format!("reading {}", path.display()), error)),
    }
}

/// An exclusive advisory lock on `path`, held until the returned file is dropped. Other processes
/// that take the same lock wait for it.
pub(crate) fn lock(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .map_err(|error| Error::io(format!("opening the lock {}", path.display()), error))?;
    file.lock().map_err(|error| Error::io(format!("locking {}", path.display()), error))?;
    Ok(file)
}

/// An exclusive advisory lock on an existing file, without creating anything.
pub(crate) fn lock_existing(path: &Path) -> Result<File> {
    let file = File::open(path).map_err(|error| Error::io(format!("opening {}", path.display()), error))?;
    file.lock().map_err(|error| Error::io(format!("locking {}", path.display()), error))?;
    Ok(file)
}

/// A unique, empty directory under the system's temporary directory, for tests.
#[cfg(test)]
pub(crate) fn scratch_dir(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "chatgpt-sdk-{label}-{}-{}",
        std::process::id(),
        crate::auth::random_token(6).unwrap()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_private_file_is_replaced_whole_and_readable_by_its_owner_only() {
        let dir = scratch_dir("files");
        let path = dir.join("record.json");
        write_private(&path, b"one").unwrap();
        write_private(&path, b"two").unwrap();
        assert_eq!(read_optional(&path).unwrap().as_deref(), Some("two"));
        assert_eq!(read_optional(&dir.join("missing")).unwrap(), None);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let leftovers: Vec<_> = fs::read_dir(&dir).unwrap().filter_map(|entry| entry.ok()).collect();
        assert_eq!(leftovers.len(), 1, "no temporary file is left behind");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_lock_is_exclusive_until_dropped() {
        let dir = scratch_dir("lock");
        let path = dir.join("x.lock");
        let held = lock(&path).unwrap();
        let other = File::open(&path).unwrap();
        assert!(other.try_lock().is_err(), "a second lock waits");
        drop(held);
        assert!(other.try_lock().is_ok());
        fs::remove_dir_all(dir).unwrap();
    }
}
