use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_path_for(path: &Path) -> std::io::Result<PathBuf> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no file name")
        })?;
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp_name = format!(".{file_name}.tmp.{}.{counter}", std::process::id());
    Ok(path.with_file_name(temp_name))
}

/// Best-effort fsync of the directory containing `path` so the renamed entry is
/// durable across a crash/power loss, not just the file contents. Errors are
/// ignored: durability is a nice-to-have and some filesystems reject directory
/// fsync.
fn sync_parent_dir(path: &Path) {
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    let dir = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    if let Ok(dir_file) = std::fs::File::open(dir) {
        let _ = dir_file.sync_all();
    }
}

///
/// # Errors
///
/// Returns an I/O error if the temporary file or rename fails.
/// Write `contents` to `path` atomically via a same-directory temp file and
/// `rename`, so readers never see a partial file if the process crashes
/// mid-write. The new file inherits the process umask for its mode; use
/// [`atomic_write_with_mode`] when the destination needs an exact mode.
pub fn atomic_write(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    atomic_write_inner(path, contents, None)
}

///
/// # Errors
///
/// Returns an I/O error if the temporary file or rename fails.
/// Like [`atomic_write`], but sets the destination file's permission bits to
/// `mode` regardless of the process umask. Needed for files whose consumer
/// rejects overly permissive modes — e.g. `launchctl` refuses a system
/// LaunchDaemon plist that is group/world-writable, which a permissive umask
/// would otherwise produce.
pub fn atomic_write_with_mode(path: &Path, contents: &[u8], mode: u32) -> std::io::Result<()> {
    atomic_write_inner(path, contents, Some(mode))
}

fn atomic_write_inner(path: &Path, contents: &[u8], mode: Option<u32>) -> std::io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let temp_path = temp_path_for(path)?;
    let result = (|| {
        // Create the temp file with the target mode up front (not the umask
        // default), so it is never momentarily group/world-readable-or-writable
        // between creation and the set_permissions below. `O_CREAT`'s mode is
        // still masked by the umask, so a tightened mode here can only be more
        // restrictive than requested; the explicit set_permissions afterwards
        // restores the exact bits regardless of umask.
        let mut open_opts = std::fs::OpenOptions::new();
        open_opts.write(true).create_new(true);
        if let Some(mode) = mode {
            open_opts.mode(mode);
        }
        let file = open_opts.open(&temp_path)?;
        let mut file = std::io::BufWriter::new(file);
        std::io::Write::write_all(&mut file, contents)?;
        let file = file.into_inner()?;
        file.sync_all()?;
        // Set the exact mode (umask can't be relied on) before the rename so the
        // destination atomically appears with the right permissions.
        if let Some(mode) = mode {
            std::fs::set_permissions(&temp_path, std::fs::Permissions::from_mode(mode))?;
        }
        std::fs::rename(&temp_path, path)
    })();

    if result.is_err() {
        let _ = std::fs::remove_file(&temp_path);
    } else {
        sync_parent_dir(path);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "caffeinate2_atomic_write_{}_{}_{name}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn atomic_write_creates_and_overwrites_file() {
        let path = temp_file_path("round_trip.txt");
        let _ = std::fs::remove_file(&path);

        atomic_write(&path, b"first").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first");

        atomic_write(&path, b"second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn atomic_write_with_mode_sets_exact_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let path = temp_file_path("modes.txt");
        let _ = std::fs::remove_file(&path);

        atomic_write_with_mode(&path, b"data", 0o644).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644);

        std::fs::remove_file(&path).unwrap();
    }
}
