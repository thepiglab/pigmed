"""Interleaved A/B timing of ONNX graph variants in one process (median of R rounds, one 512-token doc,
batch 1). Interleaving cancels most of the drift from other jobs on a shared burst node."""
import sys, time, statistics, numpy as np, onnxruntime as ort

threads, rounds, seq = int(sys.argv[1]), int(sys.argv[2]), int(sys.argv[3])
paths = sys.argv[4:]
so = ort.SessionOptions(); so.intra_op_num_threads = threads; so.inter_op_num_threads = 1
so.graph_optimization_level = ort.GraphOptimizationLevel.ORT_ENABLE_ALL
sess = [ort.InferenceSession(p, so, providers=["CPUExecutionProvider"]) for p in paths]
rng = np.random.default_rng(0)
def feeds(s):
    f = {}
    for i in s.get_inputs():
        if i.name == "input_ids": f[i.name] = rng.integers(10, 1000, (1, seq)).astype(np.int64)
        elif i.name == "attention_mask": f[i.name] = np.ones((1, seq), np.int64)
        elif i.name == "mm_embeds": f[i.name] = np.zeros((1, seq, 512), np.float32)
        elif i.name == "mm_mask": f[i.name] = np.zeros((1, seq), bool)
    return f
fs = [feeds(s) for s in sess]
for s, f in zip(sess, fs): s.run(None, f); s.run(None, f)
times = [[] for _ in sess]
for r in range(rounds):
    order = range(len(sess)) if r % 2 == 0 else reversed(range(len(sess)))
    for k in order:
        t = time.perf_counter(); sess[k].run(None, fs[k]); times[k].append(time.perf_counter() - t)
for p, t in zip(paths, times):
    print(f"{p}: median {1e3*statistics.median(t):.1f} ms  min {1e3*min(t):.1f}  max {1e3*max(t):.1f}")
