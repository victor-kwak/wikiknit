//! wikiknit: lint and knit wikilinks in Markdown vaults.
//!
//! The library loads a vault into a [`Vault`], resolves every link, and runs
//! rules over it. The binary in `main.rs` is a thin CLI on top.

pub mod catalog;
pub mod config;
pub mod filter;
pub mod git;
pub mod graph;
pub mod links;
pub mod mentions;
pub mod parse;
pub mod rules;
pub mod text;
pub mod vault;

use std::path::{Path, PathBuf};

pub use config::{Config, LinkStyle, Severity};
pub use vault::{Note, Vault};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    // The io error is part of the message, not a `source`, so `{:#}` chains don't repeat it.
    #[error("{path}: {err}")]
    Io { path: PathBuf, err: std::io::Error },
    #[error("{path}: {message}")]
    Config { path: PathBuf, message: String },
    #[error("{0}")]
    Usage(String),
}

impl Error {
    pub(crate) fn io(path: &Path, err: std::io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            err,
        }
    }
}
