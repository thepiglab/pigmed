"""torch SDPA (what the Python baseline runs) at the same shapes as `flash-check`, one call each, ms."""
import sys, time, torch
torch.set_num_threads(int(sys.argv[1]) if len(sys.argv) > 1 else 16)
F = torch.nn.functional
cases = [("gemma text sliding  b1 S1400 4/2 d256 w512", 1, 4, 2, 1400, 256, 512, 0),
         ("gemma text global   b1 S1400 4/1 d512 full", 1, 4, 1, 1400, 512, -1, 0),
         ("neomme sliding      b2 S700 16/4 d64 w256 pad", 2, 16, 4, 700, 64, 256, 300),
         ("neomme global       b2 S700 16/4 d64 full pad", 2, 16, 4, 700, 64, -1, 300),
         ("gemma vision        b1 S2520 12/12 d64 full pad", 1, 12, 12, 2520, 64, -1, 63)]
for label, b, hq, hk, s, d, w, pad in cases:
    q, k, v = torch.randn(b, hq, s, d), torch.randn(b, hk, s, d), torch.randn(b, hk, s, d)
    m = torch.ones(b, s, dtype=torch.bool)
    if pad:
        m[-1, s - pad:] = False
    allowed = m[:, None, None, :]
    if w >= 0:
        i = torch.arange(s)
        allowed = allowed & ((i[:, None] - i[None, :]).abs() <= w)
    kk, vv = k.repeat_interleave(hq // hk, 1), v.repeat_interleave(hq // hk, 1)
    for _ in range(2):
        F.scaled_dot_product_attention(q, kk, vv, attn_mask=allowed)
    t = time.perf_counter()
    for _ in range(5):
        F.scaled_dot_product_attention(q, kk, vv, attn_mask=allowed)
    print(f"torch sdpa {label}: {(time.perf_counter() - t) / 5 * 1e3:8.2f} ms")
