"""Search and context benchmark for wikiknit (see bench/README.md).

Runs `wikiknit --format json search <q> --mode <m> --limit 10` for each query of an eval set
and reports recall@5, recall@10, MRR, nDCG@10 (graded), P@1, and hits on negative queries,
per split (train/test) and query category. With --context, also runs `context --query` and
reports how many relevant notes each pack holds, and how large packs for negative queries are.

Usage: search_eval.py EVAL.json --vault DIR [--bin wikiknit] [--modes lexical,semantic,hybrid]
       [--env K=V ...] [--tag name] [--per-query] [--context] [--out results.json]
Env: XDG_STATE_HOME / XDG_CACHE_HOME pass through: point XDG_STATE_HOME at a state directory
holding the vault's semantic index (`wikiknit embed`), not your real one if you are tuning.
"""

import argparse
import json
import math
import os
import subprocess
from concurrent.futures import ThreadPoolExecutor

K = 10


def run(bin_, vault, query, mode, env, limit=K):
    cmd = [
        bin_,
        "--format",
        "json",
        "search",
        query,
        "--mode",
        mode,
        "--limit",
        str(limit),
    ]
    p = subprocess.run(
        cmd, cwd=vault, env=env, capture_output=True, text=True, check=False
    )
    if p.returncode != 0:
        raise RuntimeError(f"{cmd}: {p.stderr}")
    if "using lexical" in p.stderr and mode != "lexical":
        raise RuntimeError(f"semantic unavailable: {p.stderr}")
    return json.loads(p.stdout)["items"]


def metrics(ids, rel):
    if not rel:
        return {}
    relevant = {k for k, g in rel.items() if g > 0}
    r5 = len(relevant & set(ids[:5])) / len(relevant)
    r10 = len(relevant & set(ids[:10])) / len(relevant)
    rr = next((1 / (i + 1) for i, d in enumerate(ids[:K]) if d in relevant), 0.0)
    dcg = sum(
        (2 ** rel.get(d, 0) - 1) / math.log2(i + 2) for i, d in enumerate(ids[:K])
    )
    ideal = sorted(rel.values(), reverse=True)[:K]
    idcg = sum((2**g - 1) / math.log2(i + 2) for i, g in enumerate(ideal))
    top_primary = 1.0 if ids and rel.get(ids[0], 0) == 2 else 0.0
    return {"r5": r5, "r10": r10, "mrr": rr, "ndcg": dcg / idcg, "p1": top_primary}


def mean(xs):
    return sum(xs) / len(xs) if xs else float("nan")


def summarize(rows, key=lambda r: True):
    sel = [r for r in rows if key(r) and r["category"] != "negative"]
    neg = [r for r in rows if key(r) and r["category"] == "negative"]
    s = {m: mean([r["m"][m] for r in sel]) for m in ("r5", "r10", "mrr", "ndcg", "p1")}
    s["n"] = len(sel)
    s["neg_hits"] = mean([r["hits"] for r in neg]) if neg else float("nan")
    s["neg_sem"] = mean([r["sem_hits"] for r in neg]) if neg else float("nan")
    return s


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("eval")
    ap.add_argument("--modes", default="lexical,semantic,hybrid")
    ap.add_argument("--env", action="append", default=[])
    ap.add_argument("--tag", default="")
    ap.add_argument("--bin", default="wikiknit")
    ap.add_argument("--vault", required=True)
    ap.add_argument("--per-query", action="store_true")
    ap.add_argument("--context", action="store_true")
    ap.add_argument("--out")
    a = ap.parse_args()
    # Commands run in the vault folder: a relative --bin path means relative to here.
    if os.sep in a.bin:
        a.bin = os.path.abspath(a.bin)
    env = dict(os.environ)
    for kv in a.env:
        k, v = kv.split("=", 1)
        env[k] = v
    with open(a.eval) as f:
        data = json.load(f)
    qs = data["queries"]
    out = {}
    for mode in a.modes.split(","):

        def one(q, mode=mode):
            items = run(a.bin, a.vault, q["query"], mode, env)
            ids = [it["id"] for it in items]
            missing = [
                d
                for d in q["relevant"]
                if not os.path.exists(os.path.join(a.vault, d + ".md"))
            ]
            if missing:
                raise SystemExit(f"eval error: unknown note ids {missing}")
            return {
                "id": q["id"],
                "query": q["query"],
                "category": q["category"],
                "split": q["split"],
                "ids": ids,
                "m": metrics(ids, q["relevant"]),
                "hits": len(ids),
                "sem_hits": sum("semantic" in it["matched"] for it in items),
            }

        with ThreadPoolExecutor(8) as ex:
            rows = list(ex.map(one, qs))
        out[mode] = rows
    # Report
    cats = ["exact", "keyword", "paraphrase", "multi", "ident"]
    hdr = (
        f"{'tag':<14}{'mode':<9}{'split':<6}{'nDCG10':>7}{'R@5':>6}{'R@10':>6}{'MRR':>6}{'P@1':>6}{'negHit':>7}{'negSem':>7}  "
        + " ".join(f"{c[:5]:>6}" for c in cats)
    )
    print(hdr)
    for mode, rows in out.items():
        for split in ("all", "train", "test"):
            s = summarize(
                rows, lambda r, split=split: split == "all" or r["split"] == split
            )
            per = " ".join(
                f"{summarize(rows, lambda r, c=c, split=split: r['category'] == c and (split == 'all' or r['split'] == split))['ndcg']:6.3f}"
                for c in cats
            )
            print(
                f"{a.tag:<14}{mode:<9}{split:<6}{s['ndcg']:7.3f}{s['r5']:6.3f}{s['r10']:6.3f}{s['mrr']:6.3f}{s['p1']:6.3f}{s['neg_hits']:7.1f}{s['neg_sem']:7.1f}  {per}"
            )
    if a.per_query:
        modes = list(out)
        print("\nper query nDCG@10: " + " ".join(modes))
        for i, q in enumerate(qs):
            vals = " ".join(
                f"{out[m][i]['m'].get('ndcg', out[m][i]['hits']):6.3f}" for m in modes
            )
            print(
                f"{q['id']:>3} {q['split']:<5} {q['category']:<10} {vals}  {q['query']}"
            )
    if a.context:
        out["context"] = context_report(a, env, qs)
    if a.out:
        with open(a.out, "w") as f:
            json.dump(out, f, indent=1)


def context_report(a, env, qs):
    """Relevant notes in `context --query` packs (default budget), and negative-pack sizes."""

    def one(q):
        p = subprocess.run(
            [a.bin, "--format", "json", "context", "--query", q["query"]],
            cwd=a.vault,
            env=env,
            capture_output=True,
            text=True,
            check=True,
        )
        d = json.loads(p.stdout)
        items = {it["id"]: it for it in d["items"]}
        return q, d, items

    with ThreadPoolExecutor(8) as ex:
        res = list(ex.map(one, qs))
    tot = {
        "primary": 0,
        "primary_in": 0,
        "primary_text": 0,
        "relevant": 0,
        "relevant_in": 0,
        "negative_items": 0,
        "negative_weak": 0,
        "negatives": 0,
    }
    for q, d, items in res:
        rel = q["relevant"]
        if not rel:
            tot["negatives"] += 1
            tot["negative_items"] += len(items)
            tot["negative_weak"] += bool(d.get("weak"))
            continue
        prim = [k for k, g in rel.items() if g == 2]
        tot["primary"] += len(prim)
        tot["primary_in"] += sum(k in items for k in prim)
        tot["primary_text"] += sum(
            k in items and items[k]["content"] != "summary" for k in prim
        )
        tot["relevant"] += len(rel)
        tot["relevant_in"] += sum(k in items for k in rel)
    print(
        f"\ncontext: primary in pack {tot['primary_in']}/{tot['primary']} "
        f"(as text {tot['primary_text']}), relevant {tot['relevant_in']}/{tot['relevant']}, "
        f"negative packs: {tot['negative_items']} items over {tot['negatives']} queries, "
        f"{tot['negative_weak']} marked weak"
    )
    return tot


if __name__ == "__main__":
    main()
