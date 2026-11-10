//! Private files (secrets and the store): opened first, refusing symlinks,
//! then checked through the open handle, so the file checked is the file
//! used. Each must be a regular file owned by this user that no one else
//! may read or write.

use std::fs::{DirBuilder, File, OpenOptions};
use std::io::Read as _;
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _};
use std::path::Path;

use anyhow::{Context as _, Result, bail};

/// This process's effective user ID.
pub fn euid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// Check an open file: a regular file, owned by this user, that others may
/// neither read nor write.
pub fn check_open(f: &File, path: &Path) -> Result<()> {
    let m = f
        .metadata()
        .with_context(|| format!("reading {}", path.display()))?;
    if !m.is_file() {
        bail!("{} is not a regular file", path.display());
    }
    let me = euid();
    if m.uid() != me {
        bail!(
            "{} is owned by uid {}, not this user (uid {me}); it must be this user's own file",
            path.display(),
            m.uid()
        );
    }
    if m.mode() & 0o077 != 0 {
        bail!(
            "{} is accessible to other users (mode {:o}); restrict it with chmod 600",
            path.display(),
            m.mode() & 0o777
        );
    }
    Ok(())
}

/// Open an existing private file read-only, refusing a symlink, and check it.
pub fn open_private(path: &Path) -> Result<File> {
    let f = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    check_open(&f, path)?;
    Ok(f)
}

/// Open (creating it owner-only if missing) a private file for writing,
/// refusing a symlink, and check it.
pub fn open_or_create_private(path: &Path) -> Result<File> {
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    check_open(&f, path)?;
    Ok(f)
}

/// Read a private file's contents as text.
pub fn read_private(path: &Path) -> Result<String> {
    let mut text = String::new();
    open_private(path)?
        .read_to_string(&mut text)
        .with_context(|| format!("reading {}", path.display()))?;
    Ok(text)
}

/// Create `dir` and any missing parents owner-only (0700). Directories that
/// already exist are left as they are.
pub fn create_private_dir(dir: &Path) -> Result<()> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))
}

/// Whether `path` and the open file `f` are the same file (device and
/// inode), so a path reopened by another library is the file checked.
pub fn same_file(f: &File, path: &Path) -> Result<bool> {
    let a = f.metadata()?;
    let b =
        std::fs::symlink_metadata(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(a.dev() == b.dev() && a.ino() == b.ino())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn private_files_are_checked_through_their_handle() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret");
        std::fs::write(&path, "s").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(read_private(&path).unwrap(), "s");

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        let err = read_private(&path).unwrap_err().to_string();
        assert!(err.contains("accessible to other users"), "{err}");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        // A symlink is refused, even to a good file.
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(read_private(&link).is_err());
        assert!(open_or_create_private(&link).is_err());

        // A directory is not a private file.
        assert!(open_private(dir.path()).is_err());

        // Created owner-only, and the same file by path.
        let made = dir.path().join("made");
        let f = open_or_create_private(&made).unwrap();
        let mode = std::fs::metadata(&made).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(same_file(&f, &made).unwrap());
        assert!(!same_file(&f, &path).unwrap());
    }

    #[test]
    fn new_directories_are_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let deep = dir.path().join("a/b");
        create_private_dir(&deep).unwrap();
        for d in [dir.path().join("a"), deep] {
            let mode = std::fs::metadata(&d).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "{}", d.display());
        }
    }
}
