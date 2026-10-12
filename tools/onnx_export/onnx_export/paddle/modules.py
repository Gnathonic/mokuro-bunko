"""Export-friendly wrappers around the merged PaddleOCR-VL-1.6 + manga LoRA model.

``VisionWrap``: SigLIP-style tower + projector for ONE image, with the
position-embedding interpolation, 2D RoPE tables and 2x2 merge order given as
host-computed inputs (spec §6.4/§6.5). ``DecoderWrap``: the ERNIE-4.5 text
model with an explicit KV cache, M-RoPE cos/sin and an additive bias as inputs,
last-position logits only (§6.9). Attribute names are transformers 5.17's
``modeling_paddleocr_vl``; re-run ``check_parity`` on any version or pin bump.
"""

import torch, torch.nn as nn, torch.nn.functional as F

def rot_half(x):
    h = x.shape[-1] // 2
    return torch.cat((-x[..., h:], x[..., :h]), dim=-1)

class VisionWrap(nn.Module):
    """pixel_values (N,3,14,14), pos_idx (N,4), pos_w (N,4), cos/sin (N,72), merge_idx (N,) -> (N/4, 1024)."""
    def __init__(self, model):
        super().__init__()
        vm = model.model.visual.vision_model
        self.patch = vm.embeddings.patch_embedding  # PatchLinear
        self.pos = vm.embeddings.position_embedding
        self.layers = vm.encoder.layers
        self.post = vm.post_layernorm
        self.proj = model.model.projector
        self.heads = self.layers[0].self_attn.num_heads
    def forward(self, pixel_values, pos_idx, pos_w, cos, sin, merge_idx):
        n = pixel_values.shape[0]
        w = self.patch.weight
        x = F.linear(pixel_values.flatten(1).to(w.dtype), w.flatten(1), self.patch.bias)
        x = x + (self.pos(pos_idx) * pos_w[:, :, None].to(w.dtype)).sum(1)
        cos = cos.float().unsqueeze(1); sin = sin.float().unsqueeze(1)
        for L in self.layers:
            a = L.self_attn
            h = L.layer_norm1(x)
            q = a.q_proj(h).view(n, self.heads, -1); k = a.k_proj(h).view(n, self.heads, -1); v = a.v_proj(h).view(n, self.heads, -1)
            qf, kf = q.float(), k.float()
            q = (qf * cos + rot_half(qf) * sin).to(x.dtype); k = (kf * cos + rot_half(kf) * sin).to(x.dtype)
            q, k, v = (t.transpose(0, 1).unsqueeze(0) for t in (q, k, v))
            o = F.scaled_dot_product_attention(q, k, v, scale=a.scaling)
            o = o.squeeze(0).transpose(0, 1).reshape(n, -1)
            x = x + a.out_proj(o)
            x = x + L.mlp(L.layer_norm2(x))
        x = self.post(x)
        x = self.proj.pre_norm(x)
        x = x[merge_idx].reshape(-1, 4 * x.shape[-1])
        return self.proj.linear_2(self.proj.act(self.proj.linear_1(x)))

class DecoderWrap(nn.Module):
    """inputs_embeds (B,S,H), cos/sin (B,S,128), bias (B,1,S,T), past_k/v x L (B,2,P,128) -> last logits (B,V), present k/v x L."""
    def __init__(self, model):
        super().__init__()
        lm = model.model.language_model
        self.layers = lm.layers; self.norm = lm.norm; self.lm_head = model.lm_head
        a = self.layers[0].self_attn
        self.nh, self.nkv, self.hd, self.scaling = a.num_heads, a.num_key_value_heads, a.head_dim, a.scaling
    def forward(self, inputs_embeds, cos, sin, bias, *past):
        b, s, _ = inputs_embeds.shape
        x = inputs_embeds
        cos = cos.unsqueeze(1).to(x.dtype); sin = sin.unsqueeze(1).to(x.dtype)
        rep = self.nh // self.nkv
        presents = []
        for i, L in enumerate(self.layers):
            a = L.self_attn
            h = L.input_layernorm(x)
            q = a.q_proj(h).view(b, s, self.nh, self.hd).transpose(1, 2)
            k = a.k_proj(h).view(b, s, self.nkv, self.hd).transpose(1, 2)
            v = a.v_proj(h).view(b, s, self.nkv, self.hd).transpose(1, 2)
            q = q * cos + rot_half(q) * sin; k = k * cos + rot_half(k) * sin
            k = torch.cat([past[2 * i], k], dim=2); v = torch.cat([past[2 * i + 1], v], dim=2)
            presents += [k, v]
            t = k.shape[2]
            kk = k[:, :, None].expand(b, self.nkv, rep, t, self.hd).reshape(b, self.nh, t, self.hd)
            vv = v[:, :, None].expand(b, self.nkv, rep, t, self.hd).reshape(b, self.nh, t, self.hd)
            o = F.scaled_dot_product_attention(q, kk, vv, attn_mask=bias.to(x.dtype), scale=self.scaling)
            x = x + a.o_proj(o.transpose(1, 2).reshape(b, s, -1))
            x = x + L.mlp(L.post_attention_layernorm(x))
        x = self.norm(x[:, -1:, :])
        return (self.lm_head(x)[:, 0, :].float(), *presents)
