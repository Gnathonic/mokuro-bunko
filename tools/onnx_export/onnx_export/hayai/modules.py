"""Export-friendly functional re-implementation of hayai-nova's graphs.

Same weights, same arithmetic as the repo's ``modeling_hayai.py`` at the pinned
revision driven by the 0.5.2 runner's ``nova_generate`` (vision tower +
DSCProjector, then the decoder with an explicit KV cache), but with every
data-dependent shape moved to host-computed inputs (position table resize,
projector gather indices, RoPE tables, masks) so the graphs export with
dynamic shapes. Re-verify (``check_parity``) on any pin bump: the attribute
names below are the repo's.
"""

import torch
import torch.nn.functional as F
from torch import nn

def rope(x, cos, sin):
    # x (b,s,h,64) interleaved pairs; cos/sin (b,s,32)
    x0, x1 = x[..., 0::2], x[..., 1::2]
    c, s = cos.unsqueeze(2), sin.unsqueeze(2)
    return torch.stack([x0 * c - x1 * s, x0 * s + x1 * c], -1).flatten(-2)

class NovaVision(nn.Module):
    def __init__(self, model):
        super().__init__()
        vm = model.vision_encoder.vision_model if hasattr(model.vision_encoder, "vision_model") else model.vision_encoder
        self.patch = vm.embeddings.patch_embedding
        self.layers = vm.encoder.layers
        self.post = vm.post_layernorm
        pj = model.decoder.projector
        self.pnorm, self.mlp, self.onorm = pj.norm, pj.mlp, pj.out_norm

    def forward(self, pixel_values, pixel_mask, pos, gather_idx, tok_valid):
        h = self.patch(pixel_values) + pos
        bias = torch.where(pixel_mask > 0.5, 0.0, -1e9)[:, None, None, :]
        for layer in self.layers:
            r = h
            h = layer.layer_norm1(h)
            a = layer.self_attn
            b, P, D = h.shape
            nh = a.num_heads
            q = a.q_proj(h).view(b, P, nh, -1).transpose(1, 2)
            k = a.k_proj(h).view(b, P, nh, -1).transpose(1, 2)
            v = a.v_proj(h).view(b, P, nh, -1).transpose(1, 2)
            o = F.scaled_dot_product_attention(q, k, v, attn_mask=bias.to(q.dtype), scale=a.scale)
            h = r + a.out_proj(o.transpose(1, 2).reshape(b, P, D))
            h = h + layer.mlp(layer.layer_norm2(h))
        h = self.post(h)
        b, M, _ = gather_idx.shape
        g = torch.gather(h, 1, gather_idx.reshape(b, M * 4, 1).expand(-1, -1, h.shape[-1]))  # (b, M*4, 768)
        g = g.view(b, M, 4, -1).transpose(2, 3).reshape(b, M, -1)  # channel-major like pixel_unshuffle
        g = g * tok_valid[..., None]
        return self.onorm(self.mlp(self.pnorm(g)))

class NovaDecoder(nn.Module):
    def __init__(self, model):
        super().__init__()
        dec = model.decoder
        self.layers, self.final_norm, self.head = dec.layers, dec.final_norm, dec.output_head

    def forward(self, embeds, mask, cos, sin, *past):
        x = embeds
        present = []
        for i, layer in enumerate(self.layers):
            at = layer.attn
            h = layer.attn_norm(x)
            b, s, _ = h.shape
            q = at.q_norm(at.w_q(h).view(b, s, at.h_q, at.d_head))
            k = at.k_norm(at.w_k(h).view(b, s, at.h_kv, at.d_head))
            v = at.w_v(h).view(b, s, at.h_kv, at.d_head)
            q, k = rope(q, cos, sin), rope(k, cos, sin)
            k, v = k.transpose(1, 2), v.transpose(1, 2)
            if past:
                k = torch.cat([past[2 * i], k], 2); v = torch.cat([past[2 * i + 1], v], 2)
            present += [k, v]
            rep = at.h_q // at.h_kv
            kr, vr = k.repeat_interleave(rep, 1), v.repeat_interleave(rep, 1)
            o = F.scaled_dot_product_attention(q.transpose(1, 2), kr, vr, attn_mask=mask)
            x = x + layer.attn_res_scale * at.w_o(o.transpose(1, 2).reshape(b, s, -1))
            x = x + layer.ffn_res_scale * layer.ffn(layer.ffn_norm(x))
        logits = self.head(self.final_norm(x[:, -1]))
        return (logits, *present)
