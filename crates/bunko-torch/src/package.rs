//! One package set: the three graphs of an engine x precision x target, their graph
//! I/O contract (`bunko.io`) and, for GPU targets, the shared weights they bind to.
//!
//! `tools/torch_export` compiles GPU packages without weights: each package's
//! `bunko.weights` metadata maps every constant (FQN) to a key of the
//! `weights-<group>.safetensors` files next to the target directories
//! (`<engine>/<precision>/`). The runtime loads each weights file once onto the device
//! and binds every package to those tensors (user-managed, no copy: the model library's
//! `...UpdateUserManagedConstantBufferPairs`, see `shim/aoti_shim.cpp`),
//! so vision, prefill and step share one copy. CPU packages are frozen and carry their
//! weights (an empty map). Packages without the metadata (the shootout's) are I/O v1
//! with embedded weights.

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
    /// Loads every tensor of `files` onto `dev`, one tensor at a time (the host never
    /// holds more than one tensor's bytes).
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

/// The three graphs of one package set, bound and ready to run.
pub struct PackageSet {
    pub vision: Aoti,
    pub prefill: Aoti,
    pub step: Aoti,
    /// Graph I/O contract ([`IO_VERSIONS`]).
    pub io: u32,
    /// The shared weights (kept alive here: the packages use them in place).
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
        let maps: Vec<HashMap<String, String>> = [&vision, &prefill, &step]
            .iter()
            .map(|g| {
                g.metadata("bunko.weights").map_or(Ok(HashMap::new()), |v| {
                    serde_json::from_str(&v)
                        .map_err(|e| TorchError::Load(format!("bad bunko.weights metadata: {e}")))
                })
            })
            .collect::<Result<_, _>>()?;
        let mut weights = None;
        if maps.iter().any(|m| !m.is_empty()) {
            if weight_files.is_empty() {
                return Err(TorchError::Load(
                    "the packages need their weights-*.safetensors files, none found".into(),
                ));
            }
            let w = Weights::load(weight_files, at.dev)?;
            for (g, map) in [&vision, &prefill, &step].iter().zip(&maps) {
                if map.is_empty() {
                    continue;
                }
                let mut bound = Vec::new();
                for fqn in g.constant_fqns()? {
                    let key = map.get(&fqn).ok_or_else(|| {
                        TorchError::Load(format!("constant {fqn} has no weights key"))
                    })?;
                    let t = w.get(key).ok_or_else(|| {
                        TorchError::Load(format!("the weights files lack {key} (for {fqn})"))
                    })?;
                    bound.push((fqn, t));
                }
                g.bind(&bound)?;
            }
            weights = Some(w);
        }
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
