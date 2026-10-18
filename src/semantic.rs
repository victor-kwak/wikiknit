//! Semantic side of search: keep the embedding index current and rank notes by meaning.
//! Built with the `semantic` feature.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Serialize;

use crate::Error;
use crate::chunk;
use crate::embedder::Embedder;
use crate::search::Meaning;
use crate::state::{self, Store};
use crate::vault::Vault;
use crate::vectors::{self, Index, NoteVectors};

#[derive(Debug, Clone, Serialize)]
pub struct Update {
    pub model: String,
    /// Notes (re-)embedded now, and their chunks.
    pub embedded: usize,
    pub chunks: usize,
    /// Notes removed from the index (deleted from the vault).
    pub removed: usize,
    /// Notes still out of date (over the budget).
    pub stale: usize,
    pub total_notes: usize,
    pub total_chunks: usize,
}

/// Chunks embedded and saved together (whole notes; a note larger than this is one slice).
const SLICE_CHUNKS: usize = 512;

/// Notes to embed together: their ids, chunks, and each chunk's note (index into `ids`).
#[derive(Default)]
struct Slice {
    ids: Vec<String>,
    chunks: Vec<chunk::Chunk>,
    owners: Vec<usize>,
}

pub struct Semantic {
    pub embedder: Embedder,
    pub index: Index,
    path: PathBuf,
    /// Our vectors replace disk entries even when those look current (`embed --rebuild`).
    pub rebuild: bool,
}

impl Semantic {
    /// Open the model and index. With `download`, fetch a missing model; without it, `None`
    /// when the model is not cached yet.
    pub fn open(vault: &Vault, download: bool) -> Result<Option<Self>, Error> {
        let name = &vault.config.embedding_model;
        let Some(embedder) = Embedder::load(name, download)? else {
            return Ok(None);
        };
        let store = Store::open(&vault.config.vault)?;
        let path = vectors::path(store.dir(), name);
        let index = Index::load(&path, name)?
            .filter(|i| i.dim == embedder.dim)
            .unwrap_or_else(|| Index::empty(name, embedder.dim));
        Ok(Some(Self {
            embedder,
            index,
            path,
            rebuild: false,
        }))
    }

    /// Notes of the vault that have vectors (current or from an older version), and all notes.
    #[must_use]
    pub fn coverage(&self, vault: &Vault) -> (usize, usize) {
        let indexed = vault
            .notes
            .iter()
            .filter(|n| self.index.notes.contains_key(&n.id))
            .count();
        (indexed, vault.notes.len())
    }

    /// Enough of the vault is indexed to rank by meaning ([`Index::covers`]).
    #[must_use]
    pub fn covers(&self, vault: &Vault) -> bool {
        self.index.covers(vault.notes.iter().map(|n| n.id.as_str()))
    }

    /// Save, merged with the index on disk under a lock: another `embed` or `search` may have
    /// saved notes meanwhile, and those must not be lost. `ours` are the notes this run
    /// embedded; everything else comes from disk.
    fn save_merged(
        &mut self,
        ours: BTreeMap<String, NoteVectors>,
        current: &BTreeMap<String, String>,
    ) -> Result<(), Error> {
        let dir = self.path.parent().expect("index path has a parent");
        crate::state::create_private_dir(dir)?;
        let lock_path = self.path.with_extension("lock");
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .map_err(|e| Error::io(&lock_path, e))?;
        lock.lock().map_err(|e| Error::io(&lock_path, e))?;
        let mut merged = Index::load(&self.path, &self.index.model)?
            .filter(|i| i.dim == self.index.dim)
            .unwrap_or_else(|| self.index.clone());
        merged.retain_existing(current);
        for (id, nv) in ours {
            // Keep a disk entry that is already current (embedded by someone else meanwhile).
            let disk_current = merged
                .notes
                .get(&id)
                .is_some_and(|d| current.get(&id) == Some(&d.hash));
            if self.rebuild || !disk_current {
                merged.notes.insert(id, nv);
            }
        }
        merged.save(&self.path)?;
        self.index = merged;
        Ok(())
    }

    /// Embed one slice of notes and save it merged; returns (notes, chunks) embedded.
    fn embed_slice(
        &mut self,
        slice: Slice,
        hashes: &BTreeMap<String, String>,
    ) -> Result<(usize, usize), Error> {
        let Slice {
            ids,
            chunks,
            owners,
        } = slice;
        if chunks.is_empty() {
            return Ok((ids.len(), 0));
        }
        // Embed in length order: a batch pads to its longest text, so similar lengths
        // together waste far less work. Then put the vectors back in chunk order.
        let mut order: Vec<usize> = (0..chunks.len()).collect();
        order.sort_by_key(|&k| chunks[k].text.len());
        let texts: Vec<String> = order.iter().map(|&k| chunks[k].text.clone()).collect();
        let sorted = self.embedder.embed_passages(&texts)?;
        let mut vecs: Vec<Vec<f32>> = vec![Vec::new(); chunks.len()];
        for (v, &k) in sorted.into_iter().zip(&order) {
            vecs[k] = v;
        }
        let mut per_note: BTreeMap<usize, NoteVectors> = BTreeMap::new();
        for ((owner, c), v) in owners.iter().zip(&chunks).zip(vecs) {
            let e = per_note.entry(*owner).or_insert_with(|| NoteVectors {
                hash: hashes[&ids[*owner]].clone(),
                lines: Vec::new(),
                vectors: Vec::new(),
            });
            e.lines.push(c.line);
            e.vectors.push(v);
        }
        let ours: BTreeMap<String, NoteVectors> = per_note
            .into_iter()
            .map(|(owner, nv)| (ids[owner].clone(), nv))
            .collect();
        self.save_merged(ours, hashes)?;
        Ok((ids.len(), chunks.len()))
    }

    /// Embed notes that changed since they were indexed, at most `budget` chunks (whole notes
    /// only; `None` = no limit), drop deleted notes, and save.
    pub fn update(&mut self, vault: &Vault, budget: Option<usize>) -> Result<Update, Error> {
        let hashes = state::current_hashes(vault);
        let removed = self.index.retain_existing(&hashes);
        let stale: Vec<String> = self
            .index
            .stale(&hashes)
            .into_iter()
            .map(str::to_string)
            .collect();
        let generic = &vault.config.generic_names;
        // Embed and save in slices of whole notes: memory stays bounded on a large vault, and
        // an interrupted `embed` keeps every slice saved so far.
        let mut slice = Slice::default();
        let (mut embedded, mut chunks_done) = (0usize, 0usize);
        for id in &stale {
            let Some(note) = vault.get(id) else { continue };
            let chunks = chunk::chunks(note, generic);
            let used = chunks_done + slice.chunks.len();
            if budget.is_some_and(|b| used + chunks.len() > b) {
                if used == 0 && budget.is_some_and(|b| chunks.len() > b) {
                    continue; // one note larger than the whole budget: leave it for `embed`
                }
                break;
            }
            if !slice.chunks.is_empty() && slice.chunks.len() + chunks.len() > SLICE_CHUNKS {
                let (n, c) = self.embed_slice(std::mem::take(&mut slice), &hashes)?;
                embedded += n;
                chunks_done += c;
            }
            slice
                .owners
                .extend(std::iter::repeat_n(slice.ids.len(), chunks.len()));
            slice.ids.push(id.clone());
            slice.chunks.extend(chunks);
        }
        if !slice.ids.is_empty() {
            let (n, c) = self.embed_slice(slice, &hashes)?;
            embedded += n;
            chunks_done += c;
        } else if embedded == 0 && removed > 0 {
            self.save_merged(BTreeMap::new(), &hashes)?;
        }
        Ok(Update {
            model: self.embedder.name.clone(),
            embedded,
            chunks: chunks_done,
            removed,
            stale: stale.len() - embedded,
            total_notes: self.index.notes.len(),
            total_chunks: self.index.notes.values().map(|n| n.vectors.len()).sum(),
        })
    }

    /// Notes by meaning for a query, best first (each note's best chunk), skipping hubs unless
    /// `include_hubs`.
    pub fn rank(
        &mut self,
        vault: &Vault,
        query: &str,
        include_hubs: bool,
    ) -> Result<Vec<Meaning>, Error> {
        let q = self.embedder.embed_query(query)?;
        let current = state::current_hashes(vault);
        let mut hits: Vec<Meaning> = self
            .index
            .notes
            .iter()
            .filter_map(|(id, nv)| {
                let note = vault.index_of(id)?;
                if !include_hubs && vault.config.is_hub(id) {
                    return None;
                }
                let (best, line) = nv
                    .vectors
                    .iter()
                    .zip(&nv.lines)
                    .map(|(v, l)| (vectors::dot(&q, v), *l))
                    .max_by(|a, b| a.0.total_cmp(&b.0))?;
                // A note edited since it was embedded: its chunk lines may have moved.
                let fresh = current.get(id) == Some(&nv.hash);
                Some(Meaning {
                    note,
                    similarity: best,
                    line: if fresh { line } else { 0 },
                })
            })
            .collect();
        hits.sort_by(|a, b| {
            b.similarity
                .total_cmp(&a.similarity)
                .then(a.note.cmp(&b.note))
        });
        Ok(hits)
    }
}
