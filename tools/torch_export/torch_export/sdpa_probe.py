"""Which fused SDPA kernel THIS machine's torch picks for the call shapes our graphs use
(run on a real card of each target arch; sdpa.py's rules must agree):

    python -m torch_export.sdpa_probe
"""
import torch, json
d = torch.device("cuda")
names = {0: "math", 1: "flash", 2: "efficient", 3: "cudnn", 4: "overrideable", -1: "error"}
out = {"device": torch.cuda.get_device_name(), "arch": getattr(torch.cuda.get_device_properties(0), "gcnArchName", "") or "sm_%d%d" % torch.cuda.get_device_capability()}
for dt in ("float32", "bfloat16", "float16"):
    for hd, nq, nk in ((64, 9, 9), (64, 1, 40), (72, 80, 80), (128, 30, 30), (128, 1, 31)):
        for mask in (False, True):
            q = torch.randn(3, 8, nq, hd, device=d, dtype=getattr(torch, dt)); k = torch.randn(3, 8, nk, hd, device=d, dtype=q.dtype)
            m = torch.zeros(3, 1, nq, nk, device=d, dtype=q.dtype) if mask else None
            out[f"{dt}/hd{hd}/q{nq}k{nk}/mask{int(mask)}"] = names[torch._fused_sdp_choice(q, k, k, m)]
print(json.dumps(out, indent=0))
