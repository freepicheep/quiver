use sha2::{Digest, Sha256};
use std::path::Path;
use walkdir::WalkDir;

use crate::error::Result;

/// Compute a deterministic SHA-256 hash over a directory's contents.
///
/// Walks all files in sorted order and hashes each file's relative path
/// concatenated with its contents, producing a single hex digest.
pub fn hash_directory(dir: &Path) -> Result<String> {
    let mut hasher = Sha256::new();

    // Collect and sort all file paths for determinism
    let mut entries: Vec<_> = WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .collect();

    entries.sort_by(|a, b| a.path().cmp(b.path()));

    for entry in entries {
        let rel_path = entry.path().strip_prefix(dir).unwrap_or(entry.path());

        // Hash the relative path
        hasher.update(rel_path.to_string_lossy().as_bytes());
        // Hash the file contents
        let contents = std::fs::read(entry.path())?;
        hasher.update(&contents);
    }

    Ok(hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn hash_is_deterministic() {
        let dir = std::env::temp_dir().join("quiver_test_checksum");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("a.txt"), "hello").unwrap();
        fs::write(dir.join("sub/b.txt"), "world").unwrap();

        let h1 = hash_directory(&dir).unwrap();
        let h2 = hash_directory(&dir).unwrap();
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64); // SHA-256 hex length

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn hash_changes_with_content() {
        let dir = std::env::temp_dir().join("quiver_test_checksum2");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("file.txt"), "version1").unwrap();
        let h1 = hash_directory(&dir).unwrap();

        fs::write(dir.join("file.txt"), "version2").unwrap();
        let h2 = hash_directory(&dir).unwrap();

        assert_ne!(h1, h2);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn single_file_directory_hash_changes_with_content() {
        let dir = std::env::temp_dir().join("quiver_test_checksum_file");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("script.nu");

        fs::write(&file, "print 'a'").unwrap();
        let h1 = hash_directory(&dir).unwrap();

        fs::write(&file, "print 'b'").unwrap();
        let h2 = hash_directory(&dir).unwrap();

        assert_ne!(h1, h2);

        let _ = fs::remove_dir_all(&dir);
    }
}
