"""Fill the generated sections of docs/RUST_EMBED.md from `final_report.py --doc` output.

  python final_report.py $EMBED_ROOT/out/final --doc > report.json
  python fill_doc.py report.json ../../../docs/RUST_EMBED.md [tldr.md]

Placeholders (__TLDR__, __PARITY__, __TABLES__, __CTX__, __MM__, __OPS__, __JOBS__) are replaced in place; the
handwritten TL;DR text (optional file) goes above the per-CPU summary table.
"""

import json
import sys
from pathlib import Path

rep, doc = json.load(open(sys.argv[1])), Path(sys.argv[2])
tldr = Path(sys.argv[3]).read_text().strip() + "\n\n" if len(sys.argv) > 3 else ""
s = doc.read_text()
fill = {
    "__TLDR__": tldr + rep["by_family"] + "\n" + rep["summary"].replace("### Summary", "Summary"),
    "__PARITY__": "Worst cosine between the Rust runner's and the Python card runtime's vectors for the same inputs, over every\n"
                  "benchmark case below (all three CPUs, all thread/batch configs, lengths and file types; FP32 both sides):\n\n"
                  + rep["parity"],
    "__TABLES__": rep["tables"],
    "__CTX__": rep["ctx"],
    "__MM__": rep["mm"],
    "__OPS__": rep["ops"],
    "__JOBS__": "<details><summary>Benchmark directories per CPU</summary>\n\n" + rep["jobs"] + "\n\n</details>",
}
missing = [k for k in fill if k not in s]
if missing:
    sys.exit(f"placeholders not found: {missing}")
for k, v in fill.items():
    s = s.replace(k, v.strip())
doc.write_text(s)
print(f"filled {len(fill)} sections into {doc}")
