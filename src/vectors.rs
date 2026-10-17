//! The embedding index: one vector per chunk, per note, kept in the state directory
//! (`<vault state>/vectors/<model>.bin`). Notes are re-embedded only when their content hash
//! changes.
//!
//! File format: `WKV1\n`, a little-endian `u32` header length, a JSON header (model, dimension,
//! chunker version, and per note its id, content hash, and chunk lines), then every vector as
//! little-endian `f32`s in header order. Vectors are unit length, so cosine = dot product.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::Error;

const MAGIC: &[u8] = b"WKV1\n";
/// Bump when chunking changes, so old indexes are rebuilt.
pub const CHUNKER_VERSION: u32 = 3;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Header {
    model: String,
    dim: usize,
    chunker: u32,
    notes: Vec<NoteMeta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct NoteMeta {
    id: String,
    hash: String,
    /// File line of each chunk.
    lines: Vec<usize>,
}

/// One note's chunk vectors.
#[derive(Debug, Clone)]
pub struct NoteVectors {
    pub hash: String,
    pub lines: Vec<usize>,
    pub vectors: Vec<Vec<f32>>,
}

#[derive(Debug, Clone)]
pub struct Index {
    pub model: String,
    pub dim: usize,
    pub notes: BTreeMap<String, NoteVectors>,
}

/// `vectors/<model>.bin` under the vault's state directory.
#[must_use]
pub fn path(state_dir: &Path, model: &str) -> PathBuf {
    let safe: String = model
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    state_dir.join("vectors").join(format!("{safe}.bin"))
}

/// Scale to unit length (zero vectors stay zero).
pub fn normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        v.iter_mut().for_each(|x| *x /= norm);
    }
}

#[must_use]
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

impl Index {
    #[must_use]
    pub fn empty(model: &str, dim: usize) -> Self {
        Self {
            model: model.to_string(),
            dim,
            notes: BTreeMap::new(),
        }
    }

    /// Load the index; `None` if missing, from another chunker version, or unreadable
    /// (a corrupt index is rebuilt, never trusted).
    pub fn load(path: &Path, model: &str) -> Result<Option<Self>, Error> {
        let mut f = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(Error::io(path, e)),
        };
        let mut bytes = Vec::new();
        f.read_to_end(&mut bytes).map_err(|e| Error::io(path, e))?;
        Ok(Self::decode(&bytes).filter(|i| i.model == model))
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        let rest = bytes.strip_prefix(MAGIC)?;
        let len = u32::from_le_bytes(rest.get(..4)?.try_into().ok()?) as usize;
        let header: Header = serde_json::from_slice(rest.get(4..4 + len)?).ok()?;
        if header.chunker != CHUNKER_VERSION || header.dim == 0 {
            return None;
        }
        let mut floats = rest
            .get(4 + len..)?
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes(b.try_into().expect("4 bytes")));
        let mut notes = BTreeMap::new();
        for meta in header.notes {
            let mut vectors = Vec::with_capacity(meta.lines.len());
            for _ in &meta.lines {
                let v: Vec<f32> = floats.by_ref().take(header.dim).collect();
                if v.len() != header.dim {
                    return None;
                }
                vectors.push(v);
            }
            notes.insert(
                meta.id,
                NoteVectors {
                    hash: meta.hash,
                    lines: meta.lines,
                    vectors,
                },
            );
        }
        if floats.next().is_some() {
            return None;
        }
        Some(Self {
            model: header.model,
            dim: header.dim,
            notes,
        })
    }

    /// Write atomically (temp file + rename) into a private directory.
    pub fn save(&self, path: &Path) -> Result<(), Error> {
        let dir = path.parent().expect("index path has a parent");
        crate::state::create_private_dir(dir)?;
        let header = Header {
            model: self.model.clone(),
            dim: self.dim,
            chunker: CHUNKER_VERSION,
            notes: self
                .notes
                .iter()
                .map(|(id, n)| NoteMeta {
                    id: id.clone(),
                    hash: n.hash.clone(),
                    lines: n.lines.clone(),
                })
                .collect(),
        };
        let json = serde_json::to_vec(&header).expect("header serializes");
        let mut bytes = Vec::with_capacity(MAGIC.len() + 4 + json.len());
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(
            &u32::try_from(json.len())
                .expect("header < 4 GiB")
                .to_le_bytes(),
        );
        bytes.extend_from_slice(&json);
        for n in self.notes.values() {
            for v in &n.vectors {
                for x in v {
                    bytes.extend_from_slice(&x.to_le_bytes());
                }
            }
        }
        let mut tmp = tempfile::NamedTempFile::new_in(dir).map_err(|e| Error::io(dir, e))?;
        tmp.write_all(&bytes)
            .map_err(|e| Error::io(tmp.path(), e))?;
        tmp.persist(path).map_err(|e| Error::io(path, e.error))?;
        Ok(())
    }

    /// Notes whose current hash differs from the indexed one (or that are not indexed).
    #[must_use]
    pub fn stale<'a>(&self, current: &'a BTreeMap<String, String>) -> BTreeSet<&'a str> {
        current
            .iter()
            .filter(|(id, h)| self.notes.get(*id).is_none_or(|n| &n.hash != *h))
            .map(|(id, _)| id.as_str())
            .collect()
    }

    /// Drop notes that no longer exist.
    pub fn retain_existing(&mut self, current: &BTreeMap<String, String>) -> usize {
        let before = self.notes.len();
        self.notes.retain(|id, _| current.contains_key(id));
        before - self.notes.len()
    }

    /// The index has vectors (current or older) for at least 90% of `ids`: enough to rank by
    /// meaning without leaving out most of the vault.
    #[must_use]
    pub fn covers<'a>(&self, ids: impl IntoIterator<Item = &'a str>) -> bool {
        let (mut have, mut total) = (0usize, 0usize);
        for id in ids {
            total += 1;
            have += usize::from(self.notes.contains_key(id));
        }
        total == 0 || have * 10 >= total * 9
    }

    /// Mean of a note's chunk vectors, unit length: the note as a whole.
    #[must_use]
    pub fn note_vector(&self, id: &str) -> Option<Vec<f32>> {
        let n = self.notes.get(id)?;
        let mut mean = vec![0.0f32; self.dim];
        for v in &n.vectors {
            for (m, x) in mean.iter_mut().zip(v) {
                *m += x;
            }
        }
        normalize(&mut mean);
        Some(mean)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coverage_needs_ninety_percent() {
        let mut idx = Index::empty("m", 1);
        for i in 0..9 {
            idx.notes.insert(
                format!("n{i}"),
                NoteVectors {
                    hash: String::new(),
                    lines: vec![1],
                    vectors: vec![vec![1.0]],
                },
            );
        }
        let ids: Vec<String> = (0..10).map(|i| format!("n{i}")).collect();
        assert!(idx.covers(ids.iter().map(String::as_str)));
        let more: Vec<String> = (0..11).map(|i| format!("n{i}")).collect();
        assert!(!idx.covers(more.iter().map(String::as_str)));
    }

    #[test]
    fn roundtrip_and_corruption() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = path(dir.path(), "Some/Model");
        let mut idx = Index::empty("Some/Model", 3);
        let mut v = vec![3.0, 0.0, 4.0];
        normalize(&mut v);
        idx.notes.insert(
            "a".into(),
            NoteVectors {
                hash: "h".into(),
                lines: vec![1, 5],
                vectors: vec![v.clone(), v],
            },
        );
        idx.save(&p).unwrap();
        let back = Index::load(&p, "Some/Model").unwrap().unwrap();
        assert_eq!(back.notes["a"].lines, vec![1, 5]);
        assert!((back.notes["a"].vectors[0][2] - 0.8).abs() < 1e-6);
        // Another model's name: not this index.
        assert!(Index::load(&p, "Other").unwrap().is_none());
        // Truncated file: rebuilt, not trusted.
        let bytes = std::fs::read(&p).unwrap();
        std::fs::write(&p, &bytes[..bytes.len() - 2]).unwrap();
        assert!(Index::load(&p, "Some/Model").unwrap().is_none());
    }
}
