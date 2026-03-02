use std::path::PathBuf;

#[derive(thiserror::Error, Debug)]
pub enum QuiverError {
    #[error("manifest error: {0}")]
    Manifest(String),

    #[error("lockfile error: {0}")]
    Lockfile(String),

    #[error("git error: {0}")]
    Git(#[from] git2::Error),

    #[error("dependency conflict: package '{name}' required at {rev_a} and {rev_b}")]
    Conflict {
        name: String,
        rev_a: String,
        rev_b: String,
    },

    #[error("config error: {0}")]
    Config(String),

    #[error("no nupackage.toml found in {0}")]
    NoManifest(PathBuf),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("toml parse error: {0}")]
    TomlParse(#[from] toml::de::Error),

    #[error("toml serialize error: {0}")]
    TomlSerialize(#[from] toml::ser::Error),

    #[error("checksum source not found for asset '{asset}': {details}")]
    ChecksumSourceNotFound { asset: String, details: String },

    #[error("checksum parse failure for asset '{asset}': {details}")]
    ChecksumParse { asset: String, details: String },

    #[error(
        "checksum mismatch for asset '{asset}': expected {expected_sha256}, got {actual_sha256}"
    )]
    ChecksumMismatch {
        asset: String,
        expected_sha256: String,
        actual_sha256: String,
    },

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, QuiverError>;
