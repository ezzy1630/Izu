use serde::{Deserialize, Serialize};
use std::{fs, io, path::Path};

#[derive(Clone, Debug, Serialize)]
pub struct CleanupObservation {
    pub path: std::path::PathBuf,
    pub removed: bool,
    pub error: Option<String>,
}

/// This capability accepts only a TempDir created and owned by the lab. Native
/// cache directories are intentionally read-only; restore directory traversal
/// and removal rights after all readbacks, without following links or changing files.
pub fn cleanup_owned(temp: tempfile::TempDir) -> CleanupObservation {
    let path = temp.keep();
    let result = (|| -> io::Result<()> {
        if !fs::symlink_metadata(&path)?.is_dir() {
            return Err(io::Error::other(
                "owned fixture root is no longer a directory",
            ));
        }
        make_directories_removable(&path)?;
        fs::remove_dir_all(&path)
    })();
    CleanupObservation {
        path,
        removed: result.is_ok(),
        error: result.err().map(|error| error.to_string()),
    }
}

fn make_directories_removable(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode();
        if mode & 0o700 != 0o700 {
            fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o700))?;
        }
    }
    for entry in fs::read_dir(path)? {
        make_directories_removable(&entry?.path())?;
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FixtureSpec {
    pub seed: u64,
    pub files: usize,
    pub bytes_per_file: usize,
}

impl FixtureSpec {
    pub fn validate(&self) -> io::Result<()> {
        let size = self
            .files
            .checked_mul(self.bytes_per_file)
            .ok_or_else(|| io::Error::other("fixture size overflow"))?;
        if self.files == 0
            || self.files > 100_000
            || self.bytes_per_file < 128
            || size > 256 * 1024 * 1024
        {
            return Err(io::Error::other(
                "fixture requires 1..100000 files, >=128 bytes/file, <=256 MiB",
            ));
        }
        Ok(())
    }
}

/// Deterministic code-like text and binary input. No arbitrary user paths are removed.
pub fn create(root: &Path, spec: &FixtureSpec) -> io::Result<()> {
    spec.validate()?;
    fs::create_dir_all(root.join("src"))?;
    fs::create_dir_all(root.join("assets"))?;
    for index in 0..spec.files {
        fs::write(root.join(file_path(index)), file_bytes(spec, index))?;
    }
    fs::write(root.join(".gitignore"), "build/\n*.tmp\n")?;
    Ok(())
}

/// Expected fixture input, shared by creation and independent post-run readbacks.
pub fn file_path(index: usize) -> std::path::PathBuf {
    if index.is_multiple_of(8) {
        format!("assets/{index:06}.bin").into()
    } else {
        format!("src/{index:06}.rs").into()
    }
}

pub fn file_bytes(spec: &FixtureSpec, index: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(spec.bytes_per_file);
    if index.is_multiple_of(8) {
        let mut state = spec.seed.wrapping_add(index as u64);
        while data.len() < spec.bytes_per_file {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            data.push(state as u8);
        }
    } else {
        let line = format!(
            "fn operation_{index}() {{ let seed: u64 = {}; /* retained source text */ }}\n",
            spec.seed
        );
        while data.len() < spec.bytes_per_file {
            data.extend_from_slice(line.as_bytes());
        }
        data.truncate(spec.bytes_per_file);
    }
    data
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[test]
    fn cleanup_removes_owned_readonly_directories_without_following_external_links() {
        use std::os::unix::fs::PermissionsExt;
        let owned = tempfile::tempdir().expect("owned fixture");
        let external = tempfile::tempdir().expect("owned external sentinel");
        let sentinel = external.path().join("sentinel");
        fs::write(&sentinel, b"retained external bytes").expect("sentinel");
        fs::set_permissions(external.path(), fs::Permissions::from_mode(0o555))
            .expect("readonly external directory");
        let cache = owned.path().join("cache/payload");
        fs::create_dir_all(&cache).expect("cache");
        fs::write(cache.join("artifact"), b"immutable fixture payload").expect("payload");
        fs::set_permissions(cache.join("artifact"), fs::Permissions::from_mode(0o444))
            .expect("readonly artifact");
        fs::set_permissions(&cache, fs::Permissions::from_mode(0o555))
            .expect("readonly payload directory");
        fs::set_permissions(
            owned.path().join("cache"),
            fs::Permissions::from_mode(0o555),
        )
        .expect("readonly cache");
        std::os::unix::fs::symlink(external.path(), owned.path().join("external")).expect("link");
        let cleanup = cleanup_owned(owned);
        assert!(cleanup.removed, "{cleanup:?}");
        assert!(!cleanup.path.exists());
        assert_eq!(
            fs::read(&sentinel).expect("sentinel readback"),
            b"retained external bytes"
        );
        assert_eq!(
            fs::metadata(external.path())
                .expect("external mode")
                .permissions()
                .mode()
                & 0o777,
            0o555
        );
        assert!(cleanup_owned(external).removed);
    }
    #[test]
    fn cleanup_failure_preserves_replacement_and_reports_its_owned_locator() {
        let parent = tempfile::tempdir().expect("owned test parent");
        let fixture = tempfile::tempdir_in(parent.path()).expect("owned fixture");
        let original = parent.path().join("moved-original");
        let locator = fixture.path().to_owned();
        fs::rename(&locator, &original).expect("preserve original");
        fs::write(&locator, b"replacement must remain").expect("replacement");
        let cleanup = cleanup_owned(fixture);
        assert!(!cleanup.removed);
        assert_eq!(cleanup.path, locator);
        assert!(cleanup.error.is_some());
        assert_eq!(
            fs::read(&locator).expect("retained replacement"),
            b"replacement must remain"
        );
        assert!(original.is_dir());
    }
    #[test]
    fn budget_rejects_overflow_before_allocation() {
        assert!(
            FixtureSpec {
                seed: 1,
                files: usize::MAX,
                bytes_per_file: 4096
            }
            .validate()
            .is_err()
        );
        assert!(
            FixtureSpec {
                seed: 1,
                files: 100000,
                bytes_per_file: 4096
            }
            .validate()
            .is_err()
        );
    }
    #[test]
    fn deterministic_fixture_has_binary_and_source() {
        let a = tempfile::tempdir().expect("owned fixture");
        let b = tempfile::tempdir().expect("owned fixture");
        let spec = FixtureSpec {
            seed: 17,
            files: 2,
            bytes_per_file: 512,
        };
        create(a.path(), &spec).expect("fixture creation");
        create(b.path(), &spec).expect("fixture creation");
        assert_eq!(
            fs::read(a.path().join("assets/000000.bin")).expect("binary"),
            fs::read(b.path().join("assets/000000.bin")).expect("binary")
        );
        assert!(
            fs::read_to_string(a.path().join("src/000001.rs"))
                .expect("source")
                .starts_with("fn operation_1")
        );
    }
}
