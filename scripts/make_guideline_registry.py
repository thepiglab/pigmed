"""Build configs/guideline_sources.json from the clinical-guideline research package.

Reads the research `source_registry.json` and `authentication_registry.json` (paths passed in; nothing site-specific
is stored here) and writes one entry per source with the fields the Rust `pigmed-rs guidelines` commands use:

  id, name, group, scope, catalog_url, access, enabled, disabled_reason, adapter, min_interval_ms, max_bytes,
  readiness, research (pointers back to the research dossier), config (adapter-specific; preserved on regenerate)

Access classes (from the research `source_setup_class`):
  public              some public routes observed; scrape public routes only
  mixed               access differs per item; adapters must skip items that need a login
  unknown             authentication not established; stop at any login wall
  needs_auth          an account, service key or paid/member entitlement is required -> disabled until an operator
                      provides access (never scraped)

Existing `adapter`, `enabled`, `config`, `min_interval_ms` and `max_bytes` values in OUT are kept when regenerating,
so adapter authors can edit the JSON by hand and rerun this script safely.

usage: python -I scripts/make_guideline_registry.py --research DIR [--out configs/guideline_sources.json]
"""
import argparse
import json
from pathlib import Path

ACCESS = {
    "some_public_routes_observed_remaining_scope_unverified": "public",
    "mixed_access_requires_item_selection": "mixed",
    "authentication_not_established": "unknown",
    "has_role_specific_account_key_or_entitlement_requirement": "needs_auth",
}
KEEP = ("adapter", "enabled", "disabled_reason", "config", "min_interval_ms", "max_bytes", "notes")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--research", type=Path, required=True, help="research outputs dir (source_registry.json etc.)")
    ap.add_argument("--out", type=Path, default=Path("configs/guideline_sources.json"))
    a = ap.parse_args()
    src = json.loads((a.research / "source_registry.json").read_text())["sources"]
    auth = {s["source_id"]: s for s in json.loads((a.research / "authentication_registry.json").read_text())["sources"]}
    old = {}
    if a.out.exists():
        old = {s["id"]: s for s in json.loads(a.out.read_text())["sources"]}
    rows = []
    for s in src:
        sid = s["source_id"]
        au = auth.get(sid, {})
        access = ACCESS.get(au.get("source_setup_class"), "unknown")
        blocked = s.get("readiness") == "blocked_cannot_validate"
        e = {
            "id": sid,
            "name": s.get("source_name") or s.get("name"),
            "group": s.get("group"),
            "scope": s.get("country_or_scope") or s.get("scope"),
            "languages": s.get("languages"),
            "catalog_url": s.get("catalog_url"),
            "access": access,
            "readiness": s.get("readiness"),
            "enabled": access != "needs_auth",
            "disabled_reason": "needs_auth: account/key/entitlement required; wait for an operator" if access == "needs_auth" else None,
            "adapter": None,
            "min_interval_ms": 2000,
            "max_bytes": 268435456,
            "config": {},
            "notes": ("research readiness: blocked_cannot_validate; try public routes, record the blocker if it holds"
                      if blocked else None),
            "research": {"profile": f"source_profiles/{sid}.md", "readiness": s.get("readiness"),
                         "rust_notes": s.get("rust_implementation_notes"), "update_strategy": s.get("update_strategy")},
        }
        if sid in old:
            for k in KEEP:
                if k in old[sid]:
                    e[k] = old[sid][k]
            if access == "needs_auth":  # access class from research always wins for auth-gated sources
                e["enabled"], e["disabled_reason"] = False, "needs_auth: account/key/entitlement required; wait for an operator"
        rows.append(e)
    rows.sort(key=lambda e: e["id"])
    out = {"schema_version": 1, "generated_from": "clinical guideline research package (source/auth registries)",
           "access_classes": {"public": "public routes only", "mixed": "skip items needing login",
                              "unknown": "stop at any login wall", "needs_auth": "disabled until an operator provides access"},
           "sources": rows}
    a.out.parent.mkdir(parents=True, exist_ok=True)
    a.out.write_text(json.dumps(out, indent=1, ensure_ascii=False) + "\n")
    from collections import Counter
    print(json.dumps({"sources": len(rows), "access": Counter(r["access"] for r in rows),
                      "enabled": sum(r["enabled"] for r in rows)}))


if __name__ == "__main__":
    main()
