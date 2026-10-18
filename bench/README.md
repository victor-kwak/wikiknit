# Search benchmark

`search_eval.py` measures `search` (lexical, semantic, hybrid) and `context --query` against a
labeled query set for one vault. Use it before changing ranking code, constants, or chunking,
and compare the numbers before and after.

```bash
wikiknit --vault path/to/vault embed      # once, with the XDG_STATE_HOME you use below
uv run --no-project python bench/search_eval.py eval.json --vault path/to/vault --context
```

`--bin target/release/wikiknit` tests a build; `--per-query` lists nDCG@10 per query and mode;
`--out results.json` keeps the raw rankings for comparing two runs.

## Eval set format

```json
{
  "vault": "a note about which vault this is for",
  "grades": {"2": "primary: the note that answers the query", "1": "related"},
  "queries": [
    {"id": 0, "category": "exact", "split": "train",
     "query": "gatehouse", "relevant": {"Projects/gatehouse/overview": 2}},
    {"id": 1, "category": "negative", "split": "test",
     "query": "kubernetes helm chart", "relevant": {}}
  ]
}
```

- `relevant` maps note ids to grades; an empty map marks a negative query (nothing in the
  vault answers it), for which fewer hits is better.
- `category` is one of `exact`, `keyword`, `paraphrase`, `multi`, `ident`, `negative`: results
  are also reported per category.
- `split` is `train` or `test`. Tune on `train`, and keep a change only if `test` improves too.

Label sets name real notes, so they usually stay with the vault, not in this repository.

## Results (2026-10)

On a private 70-note vault (58 queries) and a 59-note docs vault (26 queries):

| Mode | nDCG@10 (vault) | nDCG@10 (docs) |
|---|---|---|
| lexical | 0.878 | 0.867 |
| semantic | 0.760 | 0.755 |
| hybrid | 0.941 | 0.922 |

What was tried and why the defaults are what they are: README.md, "Hybrid search".
