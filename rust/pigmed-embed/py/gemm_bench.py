"""Backend evidence: FP32 GEMM throughput of torch (MKL) and ONNX Runtime (MLAS) at the models' layer shapes.
Pair with `gemm-bench` (candle) run on the same node and thread count."""
import json, sys, time
import numpy as np, torch, onnx, onnxruntime as ort
from onnx import helper, TensorProto

t = int(sys.argv[1])
torch.set_num_threads(t)
shapes = [("gemma-text mlp up   512x512x2048", 512, 512, 2048), ("gemma-text mlp down 512x2048x512", 512, 2048, 512),
          ("gemma-vit  mlp up   2520x768x3072", 2520, 768, 3072), ("neomme     mlp up   2562x1024x3584", 2562, 1024, 3584),
          ("neomme     mlp down 2562x3584x1024", 2562, 3584, 1024)]
if len(sys.argv) > 2:  # e.g. 674x1024x3584,674x3584x1024
    shapes = [(f"{x}", *map(int, x.split("x"))) for x in sys.argv[2].split(",")]
so = ort.SessionOptions(); so.intra_op_num_threads = t; so.inter_op_num_threads = 1
for label, m, k, n in shapes:
    a = torch.randn(m, k); b = torch.randn(k, n)
    for _ in range(10): a @ b
    reps = 20; s = time.perf_counter()
    for _ in range(reps): a @ b
    dt = (time.perf_counter() - s) / reps
    print(json.dumps({"engine": "torch", "threads": t, "shape": label, "ms": round(dt * 1e3, 3), "gflops": round(2 * m * k * n / dt / 1e9, 1)}))
    # ORT: X[m,k] @ W[k,n] with W as initializer (like a Linear layer)
    w = np.random.randn(k, n).astype(np.float32)
    g = helper.make_graph([helper.make_node("MatMul", ["X", "W"], ["Y"])], "g",
                          [helper.make_tensor_value_info("X", TensorProto.FLOAT, [m, k])],
                          [helper.make_tensor_value_info("Y", TensorProto.FLOAT, [m, n])],
                          [onnx.numpy_helper.from_array(w, "W")])
    mdl = helper.make_model(g, opset_imports=[helper.make_opsetid("", 17)], ir_version=10).SerializeToString()
    x = a.numpy()
    for name, spin in (("ort-mlas", "1"), ("ort-mlas-nospin", "0")):  # the runner uses non-spinning pools
        so.add_session_config_entry("session.intra_op.allow_spinning", spin)
        sess = ort.InferenceSession(mdl, so)
        for _ in range(3): sess.run(None, {"X": x})
        s = time.perf_counter()
        for _ in range(reps): sess.run(None, {"X": x})
        dt = (time.perf_counter() - s) / reps
        print(json.dumps({"engine": name, "threads": t, "shape": label, "ms": round(dt * 1e3, 3), "gflops": round(2 * m * k * n / dt / 1e9, 1)}))
    # torch again, after ORT (load on shared nodes drifts)
    s = time.perf_counter()
    for _ in range(reps): a @ b
    dt = (time.perf_counter() - s) / reps
    print(json.dumps({"engine": "torch-again", "threads": t, "shape": label, "ms": round(dt * 1e3, 3), "gflops": round(2 * m * k * n / dt / 1e9, 1)}))
