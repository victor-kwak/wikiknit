//! Local text embeddings through fastembed (ONNX Runtime). Built with the `semantic` feature.
//!
//! Models live in `$XDG_CACHE_HOME/wikiknit/models` (or `~/.cache/wikiknit/models`). Only
//! `wikiknit embed` downloads a model; `search` uses a model only if it is already there.

use std::path::PathBuf;

use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};

use crate::Error;
use crate::vectors::normalize;

pub const DEFAULT_MODEL: &str = "BGESmallENV15Q";

/// Batch size for passages (larger batches pad more; 32 was fastest in testing).
const BATCH: usize = 32;

fn cache_dir() -> Result<PathBuf, Error> {
    if let Some(dir) = std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from)
        && dir.is_absolute()
    {
        return Ok(dir.join("wikiknit/models"));
    }
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from)
        && home.is_absolute()
    {
        return Ok(home.join(".cache/wikiknit/models"));
    }
    Err(Error::Usage(
        "cannot find a cache directory for models: set XDG_CACHE_HOME or HOME".into(),
    ))
}

/// Text put before queries and passages, as each model family was trained.
fn prefixes(model: &EmbeddingModel) -> (&'static str, &'static str) {
    let name = format!("{model:?}");
    if name.starts_with("BGE") && !name.starts_with("BGEM3") && !name.contains("ZH") {
        (
            "Represent this sentence for searching relevant passages: ",
            "",
        )
    } else if name.starts_with("MultilingualE5") {
        ("query: ", "passage: ")
    } else if name.starts_with("NomicEmbedText") {
        ("search_query: ", "search_document: ")
    } else {
        ("", "")
    }
}

pub struct Embedder {
    model: TextEmbedding,
    pub name: String,
    pub dim: usize,
    query_prefix: &'static str,
    passage_prefix: &'static str,
}

fn parse(name: &str) -> Result<EmbeddingModel, Error> {
    name.parse::<EmbeddingModel>().map_err(|_| {
        Error::Usage(format!(
            "unknown embedding model `{name}` (fastembed names, e.g. {DEFAULT_MODEL}, MultilingualE5Small)"
        ))
    })
}

/// Whether the model's files are already in the cache (no download needed).
pub fn is_cached(name: &str) -> Result<bool, Error> {
    let model = parse(name)?;
    let info = TextEmbedding::get_model_info(&model)
        .map_err(|e| Error::Usage(format!("model `{name}`: {e}")))?;
    // The model file itself must be there: a cache interrupted mid-download would otherwise
    // make `search` download the rest.
    let snapshots = cache_dir()?
        .join(format!("models--{}", info.model_code.replace('/', "--")))
        .join("snapshots");
    let Ok(entries) = std::fs::read_dir(&snapshots) else {
        return Ok(false);
    };
    Ok(entries
        .filter_map(Result::ok)
        .any(|e| e.path().join(&info.model_file).is_file()))
}

impl Embedder {
    /// Load the model, downloading it into the cache if `download` and it is missing.
    /// Returns `None` when it is missing and `download` is false.
    pub fn load(name: &str, download: bool) -> Result<Option<Self>, Error> {
        let model = parse(name)?;
        if !download && !is_cached(name)? {
            return Ok(None);
        }
        let dim = TextEmbedding::get_model_info(&model)
            .map_err(|e| Error::Usage(format!("model `{name}`: {e}")))?
            .dim;
        let (query_prefix, passage_prefix) = prefixes(&model);
        let te = TextEmbedding::try_new(
            TextInitOptions::new(model)
                .with_cache_dir(cache_dir()?)
                .with_show_download_progress(false),
        )
        .map_err(|e| Error::Usage(format!("loading model `{name}`: {e}")))?;
        Ok(Some(Self {
            model: te,
            name: name.to_string(),
            dim,
            query_prefix,
            passage_prefix,
        }))
    }

    fn run(&mut self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, Error> {
        let mut out = self
            .model
            .embed(texts, Some(BATCH))
            .map_err(|e| Error::Usage(format!("embedding with `{}`: {e}", self.name)))?;
        out.iter_mut().for_each(|v| normalize(v));
        Ok(out)
    }

    pub fn embed_passages(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>, Error> {
        let prefixed = texts
            .iter()
            .map(|t| format!("{}{t}", self.passage_prefix))
            .collect();
        self.run(prefixed)
    }

    pub fn embed_query(&mut self, query: &str) -> Result<Vec<f32>, Error> {
        let mut v = self.run(vec![format!("{}{query}", self.query_prefix)])?;
        Ok(v.pop().unwrap_or_default())
    }
}
