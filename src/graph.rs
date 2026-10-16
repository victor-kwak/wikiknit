//! Link graph: connected components with hub notes left out.

use std::collections::BTreeSet;

use serde::Serialize;

use crate::vault::Vault;

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub notes: usize,
    pub links: usize,
    pub hubs: Vec<String>,
    /// Notes with no link in either direction once hubs are removed.
    pub isolated: Vec<String>,
    /// Components (size >= 2), smallest first.
    pub components: Vec<Vec<String>>,
}

/// Undirected adjacency over note indexes.
#[must_use]
pub fn adjacency(vault: &Vault) -> Vec<BTreeSet<usize>> {
    let mut adj = vec![BTreeSet::new(); vault.notes.len()];
    for (i, n) in vault.notes.iter().enumerate() {
        for j in vault.outbound(n) {
            adj[i].insert(j);
            adj[j].insert(i);
        }
    }
    adj
}

#[must_use]
pub fn report(vault: &Vault) -> Report {
    let adj = adjacency(vault);
    let links = adj.iter().map(BTreeSet::len).sum::<usize>() / 2;
    let hub: Vec<bool> = vault
        .notes
        .iter()
        .map(|n| vault.config.is_hub(&n.id))
        .collect();
    let mut seen = vec![false; vault.notes.len()];
    let mut isolated = Vec::new();
    let mut components = Vec::new();
    for start in 0..vault.notes.len() {
        if seen[start] || hub[start] {
            continue;
        }
        let mut stack = vec![start];
        let mut comp = Vec::new();
        seen[start] = true;
        while let Some(x) = stack.pop() {
            comp.push(vault.notes[x].id.clone());
            for &y in &adj[x] {
                if !seen[y] && !hub[y] {
                    seen[y] = true;
                    stack.push(y);
                }
            }
        }
        comp.sort();
        if comp.len() == 1 {
            isolated.push(comp.remove(0));
        } else {
            components.push(comp);
        }
    }
    components.sort_by_key(|c| (c.len(), c[0].clone()));
    Report {
        notes: vault.notes.len(),
        links,
        hubs: vault.config.hubs.clone(),
        isolated,
        components,
    }
}
