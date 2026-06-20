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

///
/// # Errors
///
/// Returns an I/O error if the temporary file or rename fails.
pub fn atomic_write(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let temp_path = temp_path_for(path)?;
    let result = (|| {
        let file = std::fs::File::create(&temp_path)?;
        let mut file = std::io::BufWriter::new(file);
        std::io::Write::write_all(&mut file, contents)?;
        let file = file.into_inner()?;
        file.sync_all()?;
        std::fs::rename(&temp_path, path)
    })();

    if result.is_err() {
        let _ = std::fs::remove_file(&temp_path);
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
}
