//! wikiknit: lint and knit wikilinks in Markdown vaults.
//!
//! The library loads a vault into a [`Vault`], resolves every link, and runs
//! rules over it. The binary in `main.rs` is a thin CLI on top.

pub mod catalog;
pub mod config;
pub mod graph;
pub mod hook;
pub mod mentions;
pub mod parse;
pub mod rules;
pub mod shell;
pub mod state;
pub mod vault;

use std::path::{Path, PathBuf};

pub use config::{Config, LinkStyle, Severity};
pub use vault::{Note, Vault};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path}: {message}")]
    Config { path: PathBuf, message: String },
    #[error("{0}")]
    Usage(String),
}

impl Error {
    pub(crate) fn io(path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}
