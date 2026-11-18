from __future__ import annotations
import math
from collections import defaultdict
from .common import stable_id


def article_group(row: dict) -> str:
    """Caller-supplied duplicate_cluster overrides identifiers (required for dedup-aware splits)."""
    if row.get("duplicate_cluster"):
        return str(row["duplicate_cluster"])
    if row.get("pmcid"):
        return str(row["pmcid"]).split(".")[0]
    if row.get("doi"):
        return row["doi"].lower().removeprefix("https://doi.org/")
    if row.get("pmid"):
        return "PMID:" + str(row["pmid"])
    if row.get("article_id"):
        return str(row["article_id"]).split(".")[0]
    raise ValueError("A paper/duplicate-cluster ID is required; never split individual chunks at random")


def assign_split(row: dict, seed="pigmed-v1", train_fraction=.96, dev_fraction=.02) -> str:
    if not 0 <= train_fraction <= 1 or not 0 <= dev_fraction <= 1 - train_fraction:
        raise ValueError("Invalid split fractions")
    value = int(stable_id(seed, article_group(row)), 16) / 16**32
    return "train" if value < train_fraction else "dev" if value < train_fraction + dev_fraction else "test"


def evaluate(qrels: dict[str, dict[str, float]], rankings: dict[str, list[str]], k=10):
    if not qrels:
        raise ValueError("Empty judgments")
    results = []
    for query, judgments in qrels.items():
        relevant = {d for d, grade in judgments.items() if grade > 0}
        if not relevant:
            continue
        # Duplicate hits must not improve scores.
        hits = list(dict.fromkeys(rankings.get(query, [])))[:k]
        recall = len(relevant.intersection(hits)) / len(relevant)
        rr = next((1/(i+1) for i, d in enumerate(hits) if d in relevant), 0.0)
        gain = lambda d: 2 ** judgments.get(d, 0) - 1
        dcg = sum(gain(d) / math.log2(i+2) for i, d in enumerate(hits))
        ideal = sorted(judgments.values(), reverse=True)[:k]
        idcg = sum((2**g - 1) / math.log2(i+2) for i, g in enumerate(ideal))
        results.append({"query": query, "recall": recall, "mrr": rr, "ndcg": dcg/idcg if idcg else 0})
    if not results:
        raise ValueError("No query has positive judgments")
    return {"queries": len(results), "k": k,
            **{metric: sum(r[metric] for r in results)/len(results) for metric in ("recall", "mrr", "ndcg")},
            "per_query": results}


def reciprocal_rank_fusion(rankings: list[list[str]], constant=60):
    scores = defaultdict(float)
    for ranking in rankings:
        for rank, doc in enumerate(dict.fromkeys(ranking), start=1):
            scores[doc] += 1 / (constant + rank)
    return sorted(scores, key=lambda x: (-scores[x], x))


def wilson(successes: int, total: int, z=1.96):
    if total == 0:
        return [None, None]
    p = successes/total
    denominator = 1 + z*z/total
    center = (p + z*z/(2*total))/denominator
    half = z*math.sqrt(p*(1-p)/total + z*z/(4*total*total))/denominator
    return [center-half, center+half]
