"""Profile an exported graph in ONNX Runtime: total time per op type for one 512-token document."""
import json, sys, glob, os, collections
import numpy as np, onnx, onnxruntime as ort

path, threads = sys.argv[1], int(sys.argv[2])
seq = int(sys.argv[3]) if len(sys.argv) > 3 else 512
m = onnx.load(path, load_external_data=False)
ops = collections.Counter(n.op_type for n in m.graph.node)
print("graph ops:", dict(ops.most_common(25)), "opset:", [(o.domain, o.version) for o in m.opset_import])
so = ort.SessionOptions(); so.intra_op_num_threads = threads; so.inter_op_num_threads = 1
so.graph_optimization_level = ort.GraphOptimizationLevel.ORT_ENABLE_ALL
import tempfile
opt_path = os.path.join(tempfile.mkdtemp(prefix="ortopt-"), "opt.onnx")
so.optimized_model_filepath = opt_path
so.enable_profiling = True; so.profile_file_prefix = "/tmp/ortprof"
s = ort.InferenceSession(path, so, providers=["CPUExecutionProvider"])
feeds = {}
for i in s.get_inputs():
    shp = [1 if d in ("batch",) or (isinstance(d, str) and "batch" in d) else seq for d in i.shape[:2]] + [d for d in i.shape[2:]]
    if i.name == "input_ids": feeds[i.name] = np.random.randint(10, 1000, (1, seq)).astype(np.int64)
    elif i.name == "attention_mask": feeds[i.name] = np.ones((1, seq), np.int64)
    elif i.name == "mm_embeds": feeds[i.name] = np.zeros((1, seq, 512), np.float32)
    elif i.name == "mm_mask": feeds[i.name] = np.zeros((1, seq), bool)
for _ in range(4): s.run(None, feeds)
prof = s.end_profiling()
ev = json.load(open(prof))
by = collections.Counter(); tot = 0
for e in ev:
    if e.get("cat") == "Node" and e["name"].endswith("_kernel_time"):
        by[e["args"]["op_name"]] += e["dur"]; tot += e["dur"]
print("total node time ms (4 runs):", tot / 1e3)
for k, v in by.most_common(15): print(f"  {k:28s} {v/1e3:9.1f} ms  {100*v/tot:5.1f}%")
om = onnx.load(opt_path, load_external_data=False)
print("optimized graph ops:", dict(collections.Counter(n.op_type for n in om.graph.node).most_common(25)))
os.remove(prof)
import shutil; shutil.rmtree(os.path.dirname(opt_path), ignore_errors=True)
