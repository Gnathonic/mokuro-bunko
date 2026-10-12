//! hayai-nova and paddle-manga on **ncnn Vulkan** (`docs/rust-port/VULKAN.md`).
//!
//! The recognizers for GPUs no compiled libtorch package exists for (gfx803 Polaris and
//! the rest of the Vulkan long tail). The graphs are written straight from the
//! checkpoints by `tools/ncnn_export` (`vision`, `prefill`, `step`, fp32); a C++ shim
//! (`shim/bunko_ncnn.cpp`) runs vision per crop, one batched prefill and the decode loop
//! with the KV cache on the GPU, on ncnn plus three self-checked custom Vulkan layers.
//! Host preprocessing and detokenizing are bunko-vlm's, exactly as on libtorch.

// The stub build (no BUNKO_NCNN_DIR) keeps the API but never calls the shim.
#![cfg_attr(
    not(bunko_ncnn),
    allow(unused_mut, unused_variables, dead_code, unused_imports)
)]

use std::ffi::{CStr, CString, c_char, c_int};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use bunko_vlm::crop::{LINE_MARGIN_EM, Quad, SECOND_MARGIN_EM, hayai_line_crops, paddle_quad_crop};
use bunko_vlm::detok::Detokenizer;
use bunko_vlm::npy::Npy;
use bunko_vlm::pyfmt::py_strip;
use bunko_vlm::resample::{Filter, aa_weights, resize};
use bunko_vlm::{
    Bgr, CropSet, Recognizer, RecognizerInfo, Rgb, VlmError, hayai, paddle, read_flat,
};

#[cfg(bunko_ncnn)]
mod ffi {
    use std::ffi::{c_char, c_int};

    #[repr(C)]
    pub struct Opaque {
        _p: [u8; 0],
    }

    #[repr(C)]
    pub struct GpuInfo {
        pub name: [c_char; 256],
        pub kind: i32,
        pub vendor_id: u32,
        pub device_id: u32,
        pub api_version: u32,
        pub driver_version: u32,
        pub subgroup_size: u32,
        pub fp16_storage: i32,
        pub heap_budget_mib: u32,
    }

    unsafe extern "C" {
        pub fn bn_gpu_count() -> c_int;
        pub fn bn_shutdown();
        pub fn bn_gpu_info(index: c_int, out: *mut GpuInfo) -> c_int;
        pub fn bn_self_check(
            gpu: c_int,
            flags: u32,
            passed: *mut u32,
            log: *mut c_char,
            loglen: usize,
        ) -> c_int;
        pub fn bn_hayai_load(
            dir: *const c_char,
            gpu: c_int,
            flags: u32,
            err: *mut c_char,
            errlen: usize,
        ) -> *mut Opaque;
        pub fn bn_hayai_free(h: *mut Opaque);
        #[allow(clippy::too_many_arguments)]
        pub fn bn_hayai_generate(
            h: *mut Opaque,
            n: c_int,
            hp: *const i32,
            wp: *const i32,
            pv: *const *const f32,
            pos: *const *const f32,
            max_new: i32,
            out_ids: *mut i32,
            out_len: *mut i32,
            err: *mut c_char,
            errlen: usize,
        ) -> c_int;
        pub fn bn_hayai_stats(h: *const Opaque, secs: *mut f64, steps: *mut i64);
        #[allow(clippy::too_many_arguments)]
        pub fn bn_paddle_load(
            dir: *const c_char,
            gpu: c_int,
            flags: u32,
            prefix: *const i32,
            n_prefix: i32,
            suffix: *const i32,
            n_suffix: i32,
            vocab: i32,
            err: *mut c_char,
            errlen: usize,
        ) -> *mut Opaque;
        pub fn bn_paddle_free(h: *mut Opaque);
        #[allow(clippy::too_many_arguments)]
        pub fn bn_paddle_generate(
            h: *mut Opaque,
            n: c_int,
            gh: *const i32,
            gw: *const i32,
            pixels: *const *const f32,
            pos_idx: *const *const i32,
            pos_w: *const *const f32,
            vcos: *const *const f32,
            vsin: *const *const f32,
            caps: *const i32,
            max_cap: i32,
            out_ids: *mut i32,
            out_len: *mut i32,
            err: *mut c_char,
            errlen: usize,
        ) -> c_int;
        pub fn bn_paddle_stats(h: *const Opaque, secs: *mut f64, steps: *mut i64);
    }
}

/// fp16 blobs and weights with fp32 math: faster, but not 0.5.2's text (opt-in only).
pub const FLAG_FP16_STORAGE: u32 = 2;
/// The custom layers that passed their check on a device ([`self_check`]).
pub const CHECK_GEMM: u32 = 1;
pub const CHECK_ATTN: u32 = 2;
pub const CHECK_ARGMAX: u32 = 4;
/// hayai-nova's patch budget the graphs are written for.
pub const HAYAI_BUDGET: usize = 512;

const ERR_VRAM: c_int = 4;

#[derive(Debug, thiserror::Error)]
pub enum NcnnError {
    #[error("ncnn: {0}")]
    Load(String),
    /// The model does not fit the device's free memory.
    #[error("ncnn: {0}")]
    Vram(String),
    #[error("ncnn: {0}")]
    Run(String),
}

/// A Vulkan device ncnn can run on.
#[derive(Debug, Clone, Default)]
pub struct VkDevice {
    /// ncnn's device index.
    pub index: usize,
    pub name: String,
    /// 0 discrete, 1 integrated, 2 virtual.
    pub kind: i32,
    pub vendor_id: u32,
    pub device_id: u32,
    /// Vulkan API version (`VK_MAKE_API_VERSION`).
    pub api_version: u32,
    pub driver_version: u32,
    pub subgroup_size: u32,
    pub fp16_storage: bool,
    /// Free device-local memory at enumeration, MiB.
    pub heap_budget_mib: u32,
}

/// The hardware Vulkan devices (software rasterizers such as llvmpipe left out).
/// Destroy ncnn's Vulkan instance (see `bn_shutdown`): from an exit handler that runs
/// before the Vulkan driver unloads. Recognizers still alive must not be used after.
pub fn shutdown() {
    #[cfg(bunko_ncnn)]
    // SAFETY: no arguments; ncnn locks its own instance state.
    unsafe {
        ffi::bn_shutdown()
    };
}

pub fn devices() -> Vec<VkDevice> {
    #[cfg(bunko_ncnn)]
    unsafe {
        let n = ffi::bn_gpu_count();
        (0..n)
            .filter_map(|i| {
                let mut g: ffi::GpuInfo = std::mem::zeroed();
                if ffi::bn_gpu_info(i, &mut g) != 0 || !(0..=2).contains(&g.kind) {
                    return None;
                }
                Some(VkDevice {
                    index: i as usize,
                    name: CStr::from_ptr(g.name.as_ptr())
                        .to_string_lossy()
                        .into_owned(),
                    kind: g.kind,
                    vendor_id: g.vendor_id,
                    device_id: g.device_id,
                    api_version: g.api_version,
                    driver_version: g.driver_version,
                    subgroup_size: g.subgroup_size,
                    fp16_storage: g.fp16_storage != 0,
                    heap_budget_mib: g.heap_budget_mib,
                })
            })
            .collect()
    }
    #[cfg(not(bunko_ncnn))]
    Vec::new()
}

/// Whether this build carries ncnn at all.
pub fn available() -> bool {
    cfg!(bunko_ncnn)
}

/// Runs (once per process and device) the custom layers' checks against their CPU
/// paths: the [`CHECK_GEMM`]/[`CHECK_ATTN`]/[`CHECK_ARGMAX`] bits that passed, and the
/// failure log. A failed layer runs ncnn's stock path instead (masked attention has
/// none: paddle-manga then cannot load on that device).
pub fn self_check(device: usize, flags: u32) -> Result<(u32, String), NcnnError> {
    #[cfg(bunko_ncnn)]
    {
        let mut passed = 0u32;
        let mut log = vec![0 as c_char; 4096];
        let rc = unsafe {
            ffi::bn_self_check(
                device as c_int,
                flags,
                &mut passed,
                log.as_mut_ptr(),
                log.len(),
            )
        };
        if rc != 0 {
            return Err(NcnnError::Load(format!(
                "self-check could not run on device {device}"
            )));
        }
        Ok((passed, cstr(&log)))
    }
    #[cfg(not(bunko_ncnn))]
    Err(NcnnError::Load("this build has no ncnn".into()))
}

fn cstr(buf: &[c_char]) -> String {
    unsafe { CStr::from_ptr(buf.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

fn load_err(rc: c_int, msg: String) -> NcnnError {
    if rc == ERR_VRAM || msg.contains("of GPU memory") {
        NcnnError::Vram(msg)
    } else {
        NcnnError::Load(msg)
    }
}

/// Where the converted graphs of `engine` live: `$MOKURO_NCNN_MODELS_DIR/<engine>/fp32`.
pub fn graphs_dir(engine: &str) -> Option<PathBuf> {
    let d = PathBuf::from(std::env::var_os("MOKURO_NCNN_MODELS_DIR")?)
        .join(engine)
        .join("fp32");
    (d.join("step.param").is_file() && d.join("decoder.bin").is_file()).then_some(d)
}

/// The shim's handle: only used under the owner's mutex (the shim locks too).
struct Handle(*mut std::ffi::c_void);
// SAFETY: the handle is moved between threads but used by one at a time (Mutex).
unsafe impl Send for Handle {}

fn dir_cstring(dir: &Path) -> Result<CString, NcnnError> {
    CString::new(dir.to_string_lossy().as_bytes()).map_err(|e| NcnnError::Load(e.to_string()))
}

// ---------------------------------------------------------------------------------------
// hayai-nova

/// hayai-nova's files: the converted graphs and the host tables.
pub struct HayaiFiles<'a> {
    pub graphs: &'a Path,
    pub pos_table: &'a Path,
    pub tokenizer: &'a Path,
}

pub struct NcnnHayai {
    handle: Mutex<Handle>,
    pos_grid: Vec<f32>,
    grid: usize,
    pos_dim: usize,
    detok: Detokenizer,
    info: RecognizerInfo,
}

impl Drop for NcnnHayai {
    fn drop(&mut self) {
        #[cfg(bunko_ncnn)]
        unsafe {
            let h = self.handle.get_mut().unwrap_or_else(|p| p.into_inner());
            if !h.0.is_null() {
                ffi::bn_hayai_free(h.0.cast());
            }
        }
    }
}

impl NcnnHayai {
    /// On ncnn Vulkan device `device` (an index from [`devices`]).
    pub fn load(
        files: &HayaiFiles<'_>,
        device: usize,
        flags: u32,
        info: RecognizerInfo,
    ) -> Result<Self, NcnnError> {
        let pos = Npy::load(files.pos_table).map_err(|e| NcnnError::Load(e.to_string()))?;
        let (np, pos_dim) = match pos.shape[..] {
            [a, b] => (a, b),
            _ => return Err(NcnnError::Load("pos_table is not 2-D".into())),
        };
        let grid = (np as f64).sqrt() as usize;
        if grid * grid != np {
            return Err(NcnnError::Load(format!(
                "pos_table has {np} rows, not a square"
            )));
        }
        let pos_grid = pos.into_f32().map_err(|e| NcnnError::Load(e.to_string()))?;
        let detok =
            Detokenizer::from_file(files.tokenizer).map_err(|e| NcnnError::Load(e.to_string()))?;
        let dir = dir_cstring(files.graphs)?;
        #[cfg(bunko_ncnn)]
        {
            let mut err = [0 as c_char; 512];
            let h = unsafe {
                ffi::bn_hayai_load(
                    dir.as_ptr(),
                    device as c_int,
                    flags,
                    err.as_mut_ptr(),
                    err.len(),
                )
            };
            if h.is_null() {
                return Err(load_err(0, cstr(&err)));
            }
            Ok(Self {
                handle: Mutex::new(Handle(h.cast())),
                pos_grid,
                grid,
                pos_dim,
                detok,
                info,
            })
        }
        #[cfg(not(bunko_ncnn))]
        Err(NcnnError::Load(
            "this build has no ncnn (BUNKO_NCNN_DIR unset)".into(),
        ))
    }

    /// The resized position table of an (hp, wp) grid (bunko-torch's `pos_for`).
    fn pos_for(&self, hp: usize, wp: usize) -> Vec<f32> {
        let (g, d) = (self.grid, self.pos_dim);
        let wy = aa_weights(g, hp);
        let wx = aa_weights(g, wp);
        let mut r = vec![0f32; hp * wp * d];
        for h in 0..hp {
            for w in 0..wp {
                let out = &mut r[(h * wp + w) * d..(h * wp + w + 1) * d];
                for y in 0..g {
                    let a = wy[h * g + y];
                    if a == 0.0 {
                        continue;
                    }
                    for x in 0..g {
                        let b = wx[w * g + x];
                        if b == 0.0 {
                            continue;
                        }
                        let row = &self.pos_grid[(y * g + x) * d..(y * g + x + 1) * d];
                        for (o, &gv) in out.iter_mut().zip(row) {
                            *o += a * gv * b;
                        }
                    }
                }
            }
        }
        r
    }

    /// Generated ids per crop (EOS/PAD removed), one batch.
    pub fn generate_ids(&self, crops: &[&Rgb]) -> Result<Vec<Vec<u32>>, NcnnError> {
        let inv255 = (1.0f64 / 255.0) as f32;
        let (mut hp, mut wp, mut pvs, mut poss) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for crop in crops {
            let (th, tw) = hayai::size_for_budget(crop.height, crop.width, HAYAI_BUDGET);
            let img = resize(crop, tw, th, Filter::Bilinear);
            let (h, w) = (th / hayai::PATCH, tw / hayai::PATCH);
            let mut pv = vec![0f32; h * w * hayai::PATCH_DIM];
            for py in 0..h {
                for px in 0..w {
                    let base = (py * w + px) * hayai::PATCH_DIM;
                    for iy in 0..hayai::PATCH {
                        for ix in 0..hayai::PATCH {
                            let s = ((py * hayai::PATCH + iy) * tw + px * hayai::PATCH + ix) * 3;
                            for c in 0..3 {
                                let v = f32::from(img.data[s + c]);
                                pv[base + (iy * hayai::PATCH + ix) * 3 + c] =
                                    (v * inv255 - 0.5) / 0.5;
                            }
                        }
                    }
                }
            }
            poss.push(self.pos_for(h, w));
            pvs.push(pv);
            hp.push(h as i32);
            wp.push(w as i32);
        }
        let n = crops.len();
        let max_new = hayai::MAX_NEW_TOKENS;
        let mut ids = vec![0i32; n * max_new];
        let mut len = vec![0i32; n];
        #[cfg(bunko_ncnn)]
        {
            let pvp: Vec<*const f32> = pvs.iter().map(|v| v.as_ptr()).collect();
            let posp: Vec<*const f32> = poss.iter().map(|v| v.as_ptr()).collect();
            let mut err = [0 as c_char; 512];
            let h = self.handle.lock().unwrap_or_else(|p| p.into_inner());
            let rc = unsafe {
                ffi::bn_hayai_generate(
                    h.0.cast(),
                    n as c_int,
                    hp.as_ptr(),
                    wp.as_ptr(),
                    pvp.as_ptr(),
                    posp.as_ptr(),
                    max_new as i32,
                    ids.as_mut_ptr(),
                    len.as_mut_ptr(),
                    err.as_mut_ptr(),
                    err.len(),
                )
            };
            if rc != 0 {
                return Err(NcnnError::Run(cstr(&err)));
            }
        }
        Ok((0..n)
            .map(|i| {
                ids[i * max_new..i * max_new + len[i] as usize]
                    .iter()
                    .map(|&v| v as u32)
                    .collect()
            })
            .collect())
    }

    /// One text per crop.
    pub fn read_crops(&self, crops: &[&Rgb]) -> Result<Vec<String>, NcnnError> {
        let mut out = Vec::with_capacity(crops.len());
        for chunk in crops.chunks(hayai::BATCH) {
            for ids in self.generate_ids(chunk)? {
                out.push(py_strip(&self.detok.decode(&ids)).to_owned());
            }
        }
        Ok(out)
    }

    /// Seconds per stage (vision, prefill, -, decode) and decode steps so far.
    pub fn stats(&self) -> ([f64; 4], i64) {
        let mut s = [0f64; 4];
        let mut steps = 0i64;
        #[cfg(bunko_ncnn)]
        unsafe {
            let h = self.handle.lock().unwrap_or_else(|p| p.into_inner());
            ffi::bn_hayai_stats(h.0.cast(), s.as_mut_ptr(), &mut steps);
        }
        (s, steps)
    }
}

impl Recognizer for NcnnHayai {
    fn info(&self) -> &RecognizerInfo {
        &self.info
    }

    fn crop(&self, page: &Bgr, quad: &Quad, vertical: bool) -> CropSet {
        CropSet {
            crops: hayai_line_crops(page, quad, vertical),
        }
    }

    fn second_crop(&self, _page: &Bgr, _quad: &Quad, _vertical: bool) -> Option<CropSet> {
        None
    }

    fn read(&self, lines: &[CropSet], caps: Option<&[u32]>) -> Result<Vec<String>, VlmError> {
        read_flat(lines, caps, self.info.default_max_tokens, |crops, _caps| {
            self.read_crops(crops)
                .map_err(|e| VlmError::Runtime(e.to_string()))
        })
    }
}

// ---------------------------------------------------------------------------------------
// paddle-manga

/// paddle-manga's files: the converted graphs (with `embeddings.f32`, `pos_table.f32`
/// and `ncnn.json`) and the tokenizer.
pub struct PaddleFiles<'a> {
    pub graphs: &'a Path,
    pub tokenizer: &'a Path,
}

/// The vision graph's host inputs for one crop (bunko-torch's `VisionPrep`).
struct VisionPrep {
    gh: usize,
    gw: usize,
    pixels: Vec<f32>,
    pos_idx: Vec<i32>,
    pos_w: Vec<f32>,
    cos: Vec<f32>,
    sin: Vec<f32>,
}

pub struct NcnnPaddle {
    handle: Mutex<Handle>,
    detok: Detokenizer,
    info: RecognizerInfo,
}

impl Drop for NcnnPaddle {
    fn drop(&mut self) {
        #[cfg(bunko_ncnn)]
        unsafe {
            let h = self.handle.get_mut().unwrap_or_else(|p| p.into_inner());
            if !h.0.is_null() {
                ffi::bn_paddle_free(h.0.cast());
            }
        }
    }
}

impl NcnnPaddle {
    pub fn load(
        files: &PaddleFiles<'_>,
        device: usize,
        flags: u32,
        info: RecognizerInfo,
    ) -> Result<Self, NcnnError> {
        let detok =
            Detokenizer::from_file(files.tokenizer).map_err(|e| NcnnError::Load(e.to_string()))?;
        let meta: serde_json::Value = std::fs::read(files.graphs.join("ncnn.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .ok_or_else(|| {
                NcnnError::Load(format!(
                    "{}/ncnn.json is missing or broken",
                    files.graphs.display()
                ))
            })?;
        let ids = |k: &str| -> Vec<i64> {
            meta["prompt"][k]
                .as_array()
                .map(|a| a.iter().filter_map(|x| x.as_i64()).collect())
                .unwrap_or_default()
        };
        if ids("prefix") != paddle::PREFIX || ids("suffix") != paddle::SUFFIX {
            return Err(NcnnError::Load(
                "the graphs' prompt is not paddle-manga's".into(),
            ));
        }
        let vocab = meta["vocab"]
            .as_i64()
            .filter(|v| *v > 0)
            .ok_or_else(|| NcnnError::Load("ncnn.json: no vocab".into()))?;
        let dir = dir_cstring(files.graphs)?;
        #[cfg(bunko_ncnn)]
        {
            let pre: Vec<i32> = paddle::PREFIX.iter().map(|&v| v as i32).collect();
            let suf: Vec<i32> = paddle::SUFFIX.iter().map(|&v| v as i32).collect();
            let mut err = [0 as c_char; 512];
            let h = unsafe {
                ffi::bn_paddle_load(
                    dir.as_ptr(),
                    device as c_int,
                    flags,
                    pre.as_ptr(),
                    pre.len() as i32,
                    suf.as_ptr(),
                    suf.len() as i32,
                    vocab as i32,
                    err.as_mut_ptr(),
                    err.len(),
                )
            };
            if h.is_null() {
                return Err(load_err(0, cstr(&err)));
            }
            Ok(Self {
                handle: Mutex::new(Handle(h.cast())),
                detok,
                info,
            })
        }
        #[cfg(not(bunko_ncnn))]
        Err(NcnnError::Load(
            "this build has no ncnn (BUNKO_NCNN_DIR unset)".into(),
        ))
    }

    /// smart_resize + Pillow bicubic, patchify, position taps, 2D RoPE (as bunko-torch).
    fn vision_prep(crop: &Rgb) -> Result<VisionPrep, NcnnError> {
        use paddle::{PATCH, SIDE, V_HALF};
        let (hb, wb) = paddle::smart_resize(crop.height, crop.width)
            .map_err(|e| NcnnError::Run(e.to_string()))?;
        let img = resize(crop, wb, hb, Filter::Bicubic);
        let (gh, gw) = (hb / PATCH, wb / PATCH);
        let n = gh * gw;
        let mut pixels = vec![0f32; n * 3 * PATCH * PATCH];
        for r in 0..gh {
            for c in 0..gw {
                let base = (r * gw + c) * 3 * PATCH * PATCH;
                for iy in 0..PATCH {
                    for ix in 0..PATCH {
                        let s = ((r * PATCH + iy) * wb + c * PATCH + ix) * 3;
                        for ch in 0..3 {
                            let v = f32::from(img.data[s + ch]);
                            pixels[base + (ch * PATCH + iy) * PATCH + ix] = (v / 255.0 - 0.5) / 0.5;
                        }
                    }
                }
            }
        }
        let taps = |pos: usize, len: usize| -> (usize, usize, f64) {
            let src = if len > 1 {
                pos as f64 * (SIDE - 1) as f64 / (len - 1) as f64
            } else {
                0.0
            };
            let fl = src.floor();
            let i0 = (fl.max(0.0) as usize).min(SIDE - 1);
            (i0, (i0 + 1).min(SIDE - 1), src - fl)
        };
        let inv: [f32; V_HALF / 2] =
            std::array::from_fn(|i| 1.0 / 10000f32.powf((2 * i) as f32 / V_HALF as f32));
        let mut pos_idx = Vec::with_capacity(n * 4);
        let mut pos_w = Vec::with_capacity(n * 4);
        let mut cos = Vec::with_capacity(n * V_HALF);
        let mut sin = Vec::with_capacity(n * V_HALF);
        for r in 0..gh {
            let (r0, r1, fr) = taps(r, gh);
            for c in 0..gw {
                let (c0, c1, fc) = taps(c, gw);
                pos_idx.extend(
                    [
                        r0 * SIDE + c0,
                        r0 * SIDE + c1,
                        r1 * SIDE + c0,
                        r1 * SIDE + c1,
                    ]
                    .map(|v| v as i32),
                );
                let (rw0, rw1, cw0, cw1) = (1.0 - fr, fr, 1.0 - fc, fc);
                pos_w.extend([rw0 * cw0, rw0 * cw1, rw1 * cw0, rw1 * cw1].map(|v| v as f32));
                // one half of the cat(f, f) table: the graph's RotaryEmbed takes halves
                for k in 0..V_HALF {
                    let a = if k < V_HALF / 2 {
                        r as f32 * inv[k]
                    } else {
                        c as f32 * inv[k - V_HALF / 2]
                    };
                    cos.push(a.cos());
                    sin.push(a.sin());
                }
            }
        }
        Ok(VisionPrep {
            gh,
            gw,
            pixels,
            pos_idx,
            pos_w,
            cos,
            sin,
        })
    }

    /// Generated ids per crop (EOS removed, cut at the cap), one left-padded batch.
    pub fn generate_ids(&self, crops: &[&Rgb], caps: &[u32]) -> Result<Vec<Vec<u32>>, NcnnError> {
        let preps = crops
            .iter()
            .map(|c| Self::vision_prep(c))
            .collect::<Result<Vec<_>, _>>()?;
        let n = crops.len();
        let max_cap = caps
            .iter()
            .copied()
            .max()
            .unwrap_or(paddle::DEFAULT_MAX_NEW_TOKENS)
            .max(1) as usize;
        let mut ids = vec![0i32; n * max_cap];
        let mut len = vec![0i32; n];
        #[cfg(bunko_ncnn)]
        {
            let gh: Vec<i32> = preps.iter().map(|p| p.gh as i32).collect();
            let gw: Vec<i32> = preps.iter().map(|p| p.gw as i32).collect();
            let px: Vec<*const f32> = preps.iter().map(|p| p.pixels.as_ptr()).collect();
            let pi: Vec<*const i32> = preps.iter().map(|p| p.pos_idx.as_ptr()).collect();
            let pw: Vec<*const f32> = preps.iter().map(|p| p.pos_w.as_ptr()).collect();
            let vc: Vec<*const f32> = preps.iter().map(|p| p.cos.as_ptr()).collect();
            let vs: Vec<*const f32> = preps.iter().map(|p| p.sin.as_ptr()).collect();
            let cp: Vec<i32> = caps.iter().map(|&c| c as i32).collect();
            let mut err = [0 as c_char; 512];
            let h = self.handle.lock().unwrap_or_else(|p| p.into_inner());
            let rc = unsafe {
                ffi::bn_paddle_generate(
                    h.0.cast(),
                    n as c_int,
                    gh.as_ptr(),
                    gw.as_ptr(),
                    px.as_ptr(),
                    pi.as_ptr(),
                    pw.as_ptr(),
                    vc.as_ptr(),
                    vs.as_ptr(),
                    cp.as_ptr(),
                    max_cap as i32,
                    ids.as_mut_ptr(),
                    len.as_mut_ptr(),
                    err.as_mut_ptr(),
                    err.len(),
                )
            };
            if rc != 0 {
                return Err(NcnnError::Run(cstr(&err)));
            }
        }
        Ok((0..n)
            .map(|i| {
                ids[i * max_cap..i * max_cap + len[i] as usize]
                    .iter()
                    .map(|&v| v as u32)
                    .collect()
            })
            .collect())
    }

    /// One text per crop, batches grouped by (cap, area) as 0.5.2.
    pub fn read_crops(&self, crops: &[&Rgb], caps: &[u32]) -> Result<Vec<String>, NcnnError> {
        let mut order: Vec<usize> = (0..crops.len()).collect();
        let area = |i: usize| (crops[i].width * crops[i].height) as f64;
        order.sort_by(|&a, &b| {
            caps[a]
                .cmp(&caps[b])
                .then(area(a).total_cmp(&area(b)))
                .then(a.cmp(&b))
        });
        let mut texts = vec![String::new(); crops.len()];
        for group in order.chunks(paddle::BATCH) {
            let gc: Vec<&Rgb> = group.iter().map(|&i| crops[i]).collect();
            let gcaps: Vec<u32> = group.iter().map(|&i| caps[i]).collect();
            for (&i, ids) in group.iter().zip(self.generate_ids(&gc, &gcaps)?) {
                texts[i] = py_strip(&self.detok.decode(&ids)).to_owned();
            }
        }
        Ok(texts)
    }

    pub fn stats(&self) -> ([f64; 4], i64) {
        let mut s = [0f64; 4];
        let mut steps = 0i64;
        #[cfg(bunko_ncnn)]
        unsafe {
            let h = self.handle.lock().unwrap_or_else(|p| p.into_inner());
            ffi::bn_paddle_stats(h.0.cast(), s.as_mut_ptr(), &mut steps);
        }
        (s, steps)
    }
}

impl Recognizer for NcnnPaddle {
    fn info(&self) -> &RecognizerInfo {
        &self.info
    }

    fn crop(&self, page: &Bgr, quad: &Quad, _vertical: bool) -> CropSet {
        CropSet::one(paddle_quad_crop(page, quad, LINE_MARGIN_EM))
    }

    fn second_crop(&self, page: &Bgr, quad: &Quad, _vertical: bool) -> Option<CropSet> {
        Some(CropSet::one(paddle_quad_crop(page, quad, SECOND_MARGIN_EM)))
    }

    fn read(&self, lines: &[CropSet], caps: Option<&[u32]>) -> Result<Vec<String>, VlmError> {
        read_flat(lines, caps, self.info.default_max_tokens, |crops, caps| {
            self.read_crops(crops, caps)
                .map_err(|e| VlmError::Runtime(e.to_string()))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graphs_dir_needs_the_decoder() {
        let t = std::env::temp_dir().join(format!("bn-graphs-{}", std::process::id()));
        let d = t.join("hayai-nova/fp32");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("step.param"), "").unwrap();
        // SAFETY: tests in this module do not read the variable concurrently
        unsafe { std::env::set_var("MOKURO_NCNN_MODELS_DIR", &t) };
        assert!(graphs_dir("hayai-nova").is_none());
        std::fs::write(d.join("decoder.bin"), "").unwrap();
        assert_eq!(graphs_dir("hayai-nova"), Some(d));
        assert!(graphs_dir("paddle-manga").is_none());
        std::fs::remove_dir_all(&t).unwrap();
    }

    #[test]
    fn paddle_prep_matches_the_torch_layout() {
        // a 64 x 700 crop: smart_resize gives (h, w) multiples of 28; RoPE halves per patch
        let crop = Rgb::from_raw(700, 64, vec![128u8; 700 * 64 * 3]).unwrap();
        let p = NcnnPaddle::vision_prep(&crop).unwrap();
        let n = p.gh * p.gw;
        assert!(p.gh.is_multiple_of(2) && p.gw.is_multiple_of(2));
        assert_eq!(p.pixels.len(), n * 588);
        assert_eq!((p.pos_idx.len(), p.pos_w.len()), (n * 4, n * 4));
        assert_eq!((p.cos.len(), p.sin.len()), (n * 36, n * 36));
        // patch (0, 0): angle 0 everywhere; weights of one patch sum to 1
        assert!(p.cos[..36].iter().all(|&c| c == 1.0));
        let s: f32 = p.pos_w[..4].iter().sum();
        assert!((s - 1.0).abs() < 1e-6);
        // grey pixels normalise to (128/255 - 0.5) / 0.5
        assert!((p.pixels[0] - ((128.0 / 255.0 - 0.5) / 0.5)).abs() < 1e-6);
    }
}
