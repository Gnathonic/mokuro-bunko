//! One package set: the graphs of an engine x precision x target, their graph I/O
//! contract (`bunko.io`) and, for GPU targets, the shared weights they bind to.
//! [`PackageSet`] is the recognizers' three graphs (vision, prefill, step; `bunko.io` a
//! number in [`IO_VERSIONS`]); [`GraphSet`] is any set of roles with a named I/O id
//! (the text-box engines' single `model`, `bunko.io = "textbox-1"`). Both bind the
//! same way ([`bind_all`]).
//!
//! `tools/torch_export` compiles GPU packages without weights: each package's
//! `bunko.weights` metadata maps every constant (FQN) to a key of the
//! `weights-<group>.safetensors` files next to the target directories
//! (`<engine>/<precision>/`). The runtime loads each weights file once onto the device
//! and binds every package to those tensors (user-managed, no copy: the model library's
//! `...UpdateUserManagedConstantBufferPairs`, see `shim/aoti_shim.cpp`),
//! so vision, prefill and step share one copy. CPU packages do the same (since
//! 2026-10-08; packages built with `TORCH_EXPORT_CPU_EMBED=1` carry their weights: an
//! empty map). Packages without the metadata (the shootout's) are I/O v1 with embedded
//! weights.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use tch::{Kind, Tensor};

use crate::TorchError;
use crate::aoti::{Aoti, Placement};

/// Graph I/O contracts this runtime drives.
///
/// * 1: prefill/step return `(logits, *present)` (the shootout's packages).
/// * 2: prefill returns `(next_ids i64 (b,), live bool (b,), *present)`; step takes
///   `live` after its four other inputs and returns the same triple.
pub const IO_VERSIONS: [u32; 2] = [1, 2];

/// Device tensors of the weights files, by key, plus the files' `bunko.alias.*` names.
pub struct Weights {
    tensors: HashMap<String, Tensor>,
    aliases: HashMap<String, String>,
}

fn kind_of(dtype: &str) -> Option<Kind> {
    Some(match dtype {
        "F32" => Kind::Float,
        "BF16" => Kind::BFloat16,
        "F16" => Kind::Half,
        "F64" => Kind::Double,
        "I64" => Kind::Int64,
        "I32" => Kind::Int,
        "I16" => Kind::Int16,
        "I8" => Kind::Int8,
        "U8" => Kind::Uint8,
        "BOOL" => Kind::Bool,
        _ => return None,
    })
}

impl Weights {
    /// Loads every tensor of `files` onto `dev`. On a GPU one tensor at a time (the host
    /// never holds more than one tensor's bytes). On the CPU each file's data is read
    /// into ONE buffer and its tensors are views of it -- the layout a package with its
    /// weights inside gets (AOTInductor's single constant blob), not hundreds of separate
    /// allocations.
    pub fn load(files: &[PathBuf], dev: tch::Device) -> Result<Self, TorchError> {
        let mut tensors = HashMap::new();
        let mut aliases = HashMap::new();
        for path in files {
            let err = |m: String| TorchError::Load(format!("{}: {m}", path.display()));
            let mut f = std::fs::File::open(path).map_err(|e| err(e.to_string()))?;
            let mut len = [0u8; 8];
            f.read_exact(&mut len).map_err(|e| err(e.to_string()))?;
            let n = u64::from_le_bytes(len) as usize;
            if n > 100 << 20 {
                return Err(err(format!("header of {n} bytes")));
            }
            let mut header = vec![0u8; n];
            f.read_exact(&mut header).map_err(|e| err(e.to_string()))?;
            let header: serde_json::Map<String, serde_json::Value> =
                serde_json::from_slice(&header).map_err(|e| err(e.to_string()))?;
            let base = 8 + n as u64;
            // CPU: the whole data section in one buffer (the safetensors data offsets are
            // relative to it); tensors whose offset suits their dtype are views of it.
            let blob = if dev == tch::Device::Cpu {
                let end = header
                    .iter()
                    .filter(|(k, _)| *k != "__metadata__")
                    .filter_map(|(_, v)| v["data_offsets"].get(1).and_then(|e| e.as_u64()))
                    .max()
                    .unwrap_or(0);
                let len = f.metadata().map_err(|e| err(e.to_string()))?.len();
                if base + end > len {
                    return Err(err(format!(
                        "data ends at {} of a {len}-byte file",
                        base + end
                    )));
                }
                let blob = Tensor::f_empty([end as i64], (Kind::Uint8, dev))
                    .map_err(|e| err(format!("{end} bytes: {e}")))?;
                if end > 0 {
                    f.seek(SeekFrom::Start(base))
                        .map_err(|e| err(e.to_string()))?;
                    // SAFETY: a fresh, contiguous uint8 CPU tensor of exactly `end` bytes,
                    // not shared with anything yet.
                    let bytes = unsafe {
                        std::slice::from_raw_parts_mut(blob.data_ptr().cast::<u8>(), end as usize)
                    };
                    f.read_exact(bytes).map_err(|e| err(e.to_string()))?;
                }
                Some(blob)
            } else {
                None
            };
            for (name, v) in &header {
                if name == "__metadata__" {
                    if let Some(m) = v.as_object() {
                        for (k, v) in m {
                            if let (Some(a), Some(v)) = (k.strip_prefix("bunko.alias."), v.as_str())
                            {
                                aliases.insert(a.to_string(), v.to_string());
                            }
                        }
                    }
                    continue;
                }
                let dtype = v["dtype"].as_str().unwrap_or_default();
                let kind = kind_of(dtype).ok_or_else(|| err(format!("{name}: dtype {dtype}")))?;
                let shape: Vec<i64> = v["shape"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|d| d.as_i64()).collect())
                    .unwrap_or_default();
                let off: Vec<u64> = v["data_offsets"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|d| d.as_u64()).collect())
                    .unwrap_or_default();
                let [a, b] = off[..] else {
                    return Err(err(format!("{name}: bad data_offsets")));
                };
                let want =
                    shape.iter().product::<i64>().max(0) as u64 * kind.elt_size_in_bytes() as u64;
                if b < a || b - a != want {
                    return Err(err(format!(
                        "{name}: {} bytes for shape {shape:?}",
                        b.saturating_sub(a)
                    )));
                }
                let elt = kind.elt_size_in_bytes() as u64;
                if let Some(blob) = blob.as_ref().filter(|_| a % elt == 0 && want > 0) {
                    let t = blob
                        .f_narrow(0, a as i64, (b - a) as i64)
                        .and_then(|t| t.f_view_dtype(kind))
                        .and_then(|t| t.f_view(shape.as_slice()))
                        .map_err(|e| err(format!("{name}: {e}")))?;
                    tensors.insert(name.clone(), t);
                    continue;
                }
                let mut bytes = vec![0u8; (b - a) as usize];
                f.seek(SeekFrom::Start(base + a))
                    .map_err(|e| err(e.to_string()))?;
                f.read_exact(&mut bytes).map_err(|e| err(e.to_string()))?;
                let t = Tensor::f_from_data_size(&bytes, &shape, kind)
                    .map_err(|e| err(format!("{name}: {e}")))?
                    .to_device(dev);
                tensors.insert(name.clone(), t);
            }
        }
        Ok(Self { tensors, aliases })
    }

    pub fn get(&self, key: &str) -> Option<&Tensor> {
        self.tensors.get(key)
    }

    /// The tensor a `bunko.alias.<alias>` names (e.g. `host.embed_tokens`).
    pub fn alias(&self, alias: &str) -> Option<&Tensor> {
        self.aliases.get(alias).and_then(|k| self.tensors.get(k))
    }

    pub fn len(&self) -> usize {
        self.tensors.len()
    }
}

/// Frees the weights' memory. Dropping the tensors is not enough: the packages they
/// were bound to (user-managed) leak a reference to each one, so without this every
/// recognizer load kept a full copy of its weights for the life of the process (a
/// processor filled a 24 GB GPU in about 80 OCR sessions, then fell back to the CPU).
/// Holders declare `Weights` after their packages, so the packages are gone first.
impl Drop for Weights {
    fn drop(&mut self) {
        for t in self.tensors.values() {
            crate::aoti::release_storage(t);
        }
    }
}

/// Loads the weights files once onto `dev` and binds every graph whose `bunko.weights`
/// map is not empty to them (each of its constants by its key). `None` when no graph
/// has a map (weights inside the packages, or the shootout's packages).
fn bind_all(
    graphs: &[&Aoti],
    weight_files: &[PathBuf],
    dev: tch::Device,
) -> Result<Option<Weights>, TorchError> {
    let maps: Vec<HashMap<String, String>> = graphs
        .iter()
        .map(|g| {
            g.metadata("bunko.weights").map_or(Ok(HashMap::new()), |v| {
                serde_json::from_str(&v)
                    .map_err(|e| TorchError::Load(format!("bad bunko.weights metadata: {e}")))
            })
        })
        .collect::<Result<_, _>>()?;
    if maps.iter().all(|m| m.is_empty()) {
        return Ok(None);
    }
    if weight_files.is_empty() {
        return Err(TorchError::Load(
            "the packages need their weights-*.safetensors files, none found".into(),
        ));
    }
    let w = Weights::load(weight_files, dev)?;
    for (g, map) in graphs.iter().zip(&maps) {
        if map.is_empty() {
            continue;
        }
        let mut bound = Vec::new();
        for fqn in g.constant_fqns()? {
            let key = map
                .get(&fqn)
                .ok_or_else(|| TorchError::Load(format!("constant {fqn} has no weights key")))?;
            let t = w.get(key).ok_or_else(|| {
                TorchError::Load(format!("the weights files lack {key} (for {fqn})"))
            })?;
            bound.push((fqn, t));
        }
        g.bind(&bound)?;
    }
    Ok(Some(w))
}

/// Package metadata keys this runtime does not act on yet: a graph that sets one would
/// be bound wrongly, so the set is refused (the fp16 cast and the ONNX weights reader
/// come with the routes that need them).
const NOT_YET: [&str; 2] = ["bunko.weights_dtype", "bunko.weights_format"];

/// The graphs of one package set by role (`model`; `vision`/`prefill`/`step`; ...),
/// bound and ready to run. Unlike [`PackageSet`] it assumes no roles, and its I/O id
/// (`bunko.io`) is a string the caller names (`textbox-1`, ...).
pub struct GraphSet {
    /// (role, graph), in the order given to [`GraphSet::load`].
    pub graphs: Vec<(String, Aoti)>,
    /// The shared weights. After the graphs: dropping `Weights` frees their memory, once
    /// the graphs bound to them are gone (see `impl Drop for Weights`).
    pub weights: Option<Weights>,
}

// SAFETY: the tensors are read-only after load; see `Aoti`.
unsafe impl Send for GraphSet {}
unsafe impl Sync for GraphSet {}

impl GraphSet {
    /// Loads each `(role, path)` graph onto `at`, refuses the set unless every graph says
    /// `bunko.io = expect_io` and the graphs agree on `bunko.weights_dtype` and
    /// `bunko.weights_format`, then binds them to `weight_files`.
    pub fn load(
        graphs: &[(&str, &Path)],
        weight_files: &[PathBuf],
        at: Placement,
        expect_io: &str,
    ) -> Result<Self, TorchError> {
        if graphs.is_empty() {
            return Err(TorchError::Arg(
                "a graph set needs at least one graph".into(),
            ));
        }
        for (i, (role, _)) in graphs.iter().enumerate() {
            if graphs[..i].iter().any(|(r, _)| r == role) {
                return Err(TorchError::Arg(format!("graph role {role} given twice")));
            }
        }
        let loaded: Vec<(String, Aoti)> = graphs
            .iter()
            .map(|(role, path)| Ok((role.to_string(), Aoti::load(path, at.index)?)))
            .collect::<Result<_, TorchError>>()?;
        let meta = |key: &str| -> Vec<Option<String>> {
            loaded.iter().map(|(_, g)| g.metadata(key)).collect()
        };
        let io = meta("bunko.io");
        let io: Vec<Option<&str>> = io.iter().map(Option::as_deref).collect();
        crate::graphmeta::check_io(&io, expect_io).map_err(TorchError::Load)?;
        for key in NOT_YET {
            let found = meta(key);
            let found: Vec<Option<&str>> = found.iter().map(Option::as_deref).collect();
            if let Some(v) = crate::graphmeta::agreed(key, &found).map_err(TorchError::Load)? {
                return Err(TorchError::Load(format!(
                    "{key} {v}: this runtime binds the weights as stored"
                )));
            }
        }
        let refs: Vec<&Aoti> = loaded.iter().map(|(_, g)| g).collect();
        let weights = bind_all(&refs, weight_files, at.dev)?;
        Ok(Self {
            graphs: loaded,
            weights,
        })
    }

    /// The graph of `role`.
    pub fn graph(&self, role: &str) -> Option<&Aoti> {
        self.graphs.iter().find(|(r, _)| r == role).map(|(_, g)| g)
    }

    /// A metadata value of the first graph (`bunko.max_batch`, ...).
    pub fn metadata(&self, key: &str) -> Option<String> {
        self.graphs.first().and_then(|(_, g)| g.metadata(key))
    }
}

/// The three graphs of one package set, bound and ready to run.
pub struct PackageSet {
    pub vision: Aoti,
    pub prefill: Aoti,
    pub step: Aoti,
    /// Graph I/O contract ([`IO_VERSIONS`]).
    pub io: u32,
    /// The shared weights (kept alive here: the packages use them in place). After the
    /// packages: dropping `Weights` frees their memory, once the packages are gone.
    pub weights: Option<Weights>,
}

// SAFETY: the tensors are read-only after load; see `Aoti`.
unsafe impl Send for PackageSet {}
unsafe impl Sync for PackageSet {}

/// `weights-*.safetensors` next to the target directory (`<engine>/<precision>/`).
pub fn default_weight_files(model_dir: &Path) -> Vec<PathBuf> {
    let Some(parent) = model_dir.parent() else {
        return Vec::new();
    };
    let mut v: Vec<PathBuf> = std::fs::read_dir(parent)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name().is_some_and(|n| {
                        let n = n.to_string_lossy();
                        n.starts_with("weights-") && n.ends_with(".safetensors")
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

impl PackageSet {
    pub fn load(
        graphs: [&Path; 3],
        weight_files: &[PathBuf],
        at: Placement,
    ) -> Result<Self, TorchError> {
        let [vision, prefill, step] = graphs.map(|p| Aoti::load(p, at.index));
        let (vision, prefill, step) = (vision?, prefill?, step?);
        let ios: Vec<u32> = [&vision, &prefill, &step]
            .iter()
            .map(|g| {
                g.metadata("bunko.io")
                    .map_or(Ok(1), |v| v.trim().parse::<u32>())
                    .map_err(|_| TorchError::Load("bad bunko.io metadata".into()))
            })
            .collect::<Result<_, _>>()?;
        let io = ios[0];
        if ios.iter().any(|&v| v != io) || !IO_VERSIONS.contains(&io) {
            return Err(TorchError::Load(format!(
                "package I/O versions {ios:?}: this runtime drives {IO_VERSIONS:?} and all three graphs must agree"
            )));
        }
        let weights = bind_all(&[&vision, &prefill, &step], weight_files, at.dev)?;
        Ok(Self {
            vision,
            prefill,
            step,
            io,
            weights,
        })
    }

    /// A metadata value of the vision package (`bunko.special`, `bunko.prompt`, ...).
    pub fn metadata(&self, key: &str) -> Option<String> {
        self.vision.metadata(key)
    }
}

/// A decoder graph's outputs as (next token ids, live rows, present KV): I/O v1 gives
/// `(logits, *present)` (ids = first argmax, like `torch.argmax`; no live), v2
/// `(next_ids, live, *present)`.
pub fn split_decoder_out(
    out: Vec<Tensor>,
    io: u32,
) -> Result<(Tensor, Option<Tensor>, Vec<Tensor>), TorchError> {
    let mut it = out.into_iter();
    let first = it
        .next()
        .ok_or_else(|| TorchError::Run("the decoder returned nothing".into()))?;
    if io >= 2 {
        let live = it
            .next()
            .ok_or_else(|| TorchError::Run("the decoder returned no live mask".into()))?;
        Ok((first, Some(live), it.collect()))
    } else {
        Ok((first.argmax(-1, false), None, it.collect()))
    }
}
