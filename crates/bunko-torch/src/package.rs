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
//! weights. A [`GraphSet`] whose graphs say `bunko.weights_dtype = "fp16"` (nar-small's
//! fp16 packages, compiled over the authors' fp32 file) has its floating weights cast to
//! fp16 at load; no fp16 copy of the weights exists on disk. A set whose graphs say
//! `bunko.weights_format = "onnx"` (AnimeText) binds the raw initializers of one ONNX
//! file instead ([`Weights::load_onnx`]).

use std::collections::{HashMap, HashSet};
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

/// The kinds a weights cast converts (integer and bool tensors keep their kind).
fn is_floating(kind: Kind) -> bool {
    matches!(
        kind,
        Kind::Float | Kind::Double | Kind::BFloat16 | Kind::Half
    )
}

impl Weights {
    /// Loads every tensor of `files` onto `dev`. On a GPU one tensor at a time (the host
    /// never holds more than one tensor's bytes). On the CPU each file's data is read
    /// into ONE buffer and its tensors are views of it -- the layout a package with its
    /// weights inside gets (AOTInductor's single constant blob), not hundreds of separate
    /// allocations.
    ///
    /// With `cast` (a package set marked `bunko.weights_dtype`, e.g. nar-small's fp16
    /// packages over the authors' fp32 file) every floating tensor (F32, F64, BF16, F16)
    /// becomes that kind once on `dev`; integer and bool tensors stay as stored. A cast
    /// tensor is a new allocation, so the CPU blob views are skipped then.
    pub fn load(
        files: &[PathBuf],
        dev: tch::Device,
        cast: Option<Kind>,
    ) -> Result<Self, TorchError> {
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
            let blob = if dev == tch::Device::Cpu && cast.is_none() {
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
                let mut t = Tensor::f_from_data_size(&bytes, &shape, kind)
                    .map_err(|e| err(format!("{name}: {e}")))?
                    .to_device(dev);
                if let Some(to) = cast.filter(|&to| to != kind && is_floating(kind)) {
                    t = t
                        .f_to_kind(to)
                        .map_err(|e| err(format!("{name} to {to:?}: {e}")))?;
                }
                tensors.insert(name.clone(), t);
            }
        }
        Ok(Self { tensors, aliases })
    }

    /// Loads the `wanted` initializers of the ONNX file at `path` onto `dev`, one tensor
    /// at a time, keyed by initializer name (`bunko.weights_format = "onnx"`). The file
    /// must be `size` bytes (`bunko.weights_size`); [`crate::onnx_weights::scan`] refuses
    /// a missing or repeated name, a dtype other than FLOAT, an external tensor and a byte
    /// count that does not match the dims. The host checked the file's sha256 when it
    /// downloaded it, so it is not hashed again here.
    pub fn load_onnx(
        path: &Path,
        dev: tch::Device,
        wanted: &HashSet<String>,
        size: u64,
    ) -> Result<Self, TorchError> {
        let err = |m: String| TorchError::Load(format!("{}: {m}", path.display()));
        let f = std::fs::File::open(path).map_err(|e| err(e.to_string()))?;
        let len = f.metadata().map_err(|e| err(e.to_string()))?.len();
        if len != size {
            return Err(err(format!(
                "{len} bytes, the packages were built against a {size}-byte file"
            )));
        }
        let mut r = std::io::BufReader::new(f);
        let found = crate::onnx_weights::scan(&mut r, size, wanted).map_err(err)?;
        let mut f = r.into_inner();
        let mut tensors = HashMap::with_capacity(found.len());
        for (name, t) in found {
            let mut bytes = vec![0u8; t.len as usize];
            f.seek(SeekFrom::Start(t.offset))
                .map_err(|e| err(e.to_string()))?;
            f.read_exact(&mut bytes)
                .map_err(|e| err(format!("{name}: {e}")))?;
            // ONNX raw_data is little-endian, as tch reads host bytes on every target we build.
            let t = Tensor::f_from_data_size(&bytes, &t.dims, Kind::Float)
                .map_err(|e| err(format!("{name}: {e}")))?
                .to_device(dev);
            tensors.insert(name, t);
        }
        Ok(Self {
            tensors,
            aliases: HashMap::new(),
        })
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

/// Where the weights of a set of graphs are read from (`bunko.weights_format`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WeightsFormat {
    /// `weights-*.safetensors` files, or the authors' safetensors (no metadata).
    Safetensors,
    /// One ONNX file's raw initializers (`onnx`), `size` bytes (`bunko.weights_size`).
    Onnx { size: u64 },
}

impl WeightsFormat {
    /// From the graphs' agreed `bunko.weights_format` and `bunko.weights_size`.
    fn from_meta(format: Option<&str>, size: Option<&str>) -> Result<Self, TorchError> {
        match format.map(str::trim) {
            None => Ok(Self::Safetensors),
            Some("onnx") => {
                let size = size.ok_or_else(|| {
                    TorchError::Load("bunko.weights_format onnx without bunko.weights_size".into())
                })?;
                let size = size
                    .trim()
                    .parse::<u64>()
                    .map_err(|_| TorchError::Load(format!("bad bunko.weights_size {size:?}")))?;
                Ok(Self::Onnx { size })
            }
            Some(v) => Err(TorchError::Load(format!(
                "bunko.weights_format {v}: this runtime reads safetensors or onnx"
            ))),
        }
    }
}

/// Loads the weights files once onto `dev` and binds every graph whose `bunko.weights`
/// map is not empty to them (each of its constants by its key), cast to `cast` when the
/// set asks for it ([`Weights::load`]). `None` when no graph has a map (weights inside
/// the packages, or the shootout's packages).
fn bind_all(
    graphs: &[&Aoti],
    weight_files: &[PathBuf],
    dev: tch::Device,
    format: WeightsFormat,
    cast: Option<Kind>,
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
    let w = match format {
        WeightsFormat::Safetensors => {
            if weight_files.is_empty() {
                return Err(TorchError::Load(
                    "the packages need their weights-*.safetensors files, none found".into(),
                ));
            }
            Weights::load(weight_files, dev, cast)?
        }
        WeightsFormat::Onnx { size } => {
            if cast.is_some() {
                // The ONNX reader binds FLOAT initializers as stored; no package casts them.
                return Err(TorchError::Load(
                    "bunko.weights_dtype with bunko.weights_format onnx: this runtime casts safetensors weights only".into(),
                ));
            }
            let [file] = weight_files else {
                return Err(TorchError::Load(format!(
                    "the packages bind one ONNX file's weights, {} files given",
                    weight_files.len()
                )));
            };
            // Only the initializers the graphs name (the file also holds shape constants).
            let wanted: HashSet<String> = maps.iter().flat_map(|m| m.values().cloned()).collect();
            Weights::load_onnx(file, dev, &wanted, size)?
        }
    };
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
    /// `bunko.weights_format`, then binds them to `weight_files` (safetensors files, cast
    /// to fp16 when `bunko.weights_dtype = "fp16"` ([`crate::graphmeta::weights_cast`]),
    /// or with `bunko.weights_format = "onnx"` the one ONNX file).
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
        let dtype = meta("bunko.weights_dtype");
        let dtype: Vec<Option<&str>> = dtype.iter().map(Option::as_deref).collect();
        let cast = match crate::graphmeta::weights_cast(&dtype).map_err(TorchError::Load)? {
            Some("fp16") => Some(Kind::Half),
            Some(v) => return Err(TorchError::Load(format!("bunko.weights_dtype {v}"))),
            None => None,
        };
        let agreed = |key: &str| -> Result<Option<String>, TorchError> {
            let found = meta(key);
            let found: Vec<Option<&str>> = found.iter().map(Option::as_deref).collect();
            Ok(crate::graphmeta::agreed(key, &found)
                .map_err(TorchError::Load)?
                .map(str::to_string))
        };
        let format_meta = agreed("bunko.weights_format")?;
        // The size matters only to the ONNX reader (safetensors files carry their own).
        let size_meta = match format_meta {
            Some(_) => agreed("bunko.weights_size")?,
            None => None,
        };
        let format = WeightsFormat::from_meta(format_meta.as_deref(), size_meta.as_deref())?;
        let refs: Vec<&Aoti> = loaded.iter().map(|(_, g)| g).collect();
        let weights = bind_all(&refs, weight_files, at.dev, format, cast)?;
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
        let weights = bind_all(
            &[&vision, &prefill, &step],
            weight_files,
            at.dev,
            WeightsFormat::Safetensors,
            None,
        )?;
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

#[cfg(all(test, feature = "libtorch"))]
mod tests {
    use super::*;

    /// Writes a safetensors file of `(name, dtype, shape, bytes)` tensors: the 8-byte
    /// little-endian header length, the JSON header, then the data.
    fn write_safetensors(path: &Path, tensors: &[(&str, &str, &[i64], &[u8])]) {
        let mut header = serde_json::Map::new();
        let mut data = Vec::new();
        for (name, dtype, shape, bytes) in tensors {
            let a = data.len();
            data.extend_from_slice(bytes);
            header.insert(
                name.to_string(),
                serde_json::json!({"dtype": dtype, "shape": shape, "data_offsets": [a, data.len()]}),
            );
        }
        let header = serde_json::to_vec(&header).unwrap();
        let mut out = (header.len() as u64).to_le_bytes().to_vec();
        out.extend_from_slice(&header);
        out.extend_from_slice(&data);
        std::fs::write(path, out).unwrap();
    }

    #[test]
    fn weights_cast_to_half() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("w.safetensors");
        write_safetensors(
            &f,
            &[
                (
                    "a",
                    "F32",
                    &[2],
                    &[1.0f32.to_le_bytes(), 0.1f32.to_le_bytes()].concat(),
                ),
                ("b", "BF16", &[1], &0x4040u16.to_le_bytes()), // bf16 3.0
                ("c", "I64", &[1], &7i64.to_le_bytes()),
            ],
        );
        let w = Weights::load(&[f], tch::Device::Cpu, Some(tch::Kind::Half)).unwrap();
        assert_eq!(w.get("a").unwrap().kind(), tch::Kind::Half);
        assert_eq!(w.get("b").unwrap().kind(), tch::Kind::Half);
        assert_eq!(w.get("c").unwrap().kind(), tch::Kind::Int64);
        let a = Tensor::from_slice(&[1.0f32, 0.1]).to_kind(tch::Kind::Half);
        assert!(w.get("a").unwrap().equal(&a));
    }

    /// Loads the package `pkg` on gpu:0, after the GPU half of the libtorch at
    /// `$LIBTORCH` (the one the libtorch-feature tests link).
    fn load_gpu_package(pkg: &std::ffi::OsStr) -> Aoti {
        crate::runtime::init(&crate::abi::InitConfig {
            lib_dir: std::env::var("LIBTORCH").ok().map(|d| format!("{d}/lib")),
            ..Default::default()
        })
        .unwrap();
        assert!(crate::gpu::loaded(), "no GPU half of libtorch loaded");
        Aoti::load(Path::new(pkg), 0).unwrap()
    }

    /// Binds the package `g` (on gpu:0) to the weights file `w` loaded with `cast`, by
    /// its `bunko.weights` map, as [`bind_all`] does: the bind's error.
    fn bind_with(g: &Aoti, w: &std::ffi::OsStr, cast: Option<Kind>) -> String {
        let weights = Weights::load(&[PathBuf::from(w)], tch::Device::Cuda(0), cast).unwrap();
        let map: HashMap<String, String> =
            serde_json::from_str(&g.metadata("bunko.weights").unwrap()).unwrap();
        let bound: Vec<(String, &Tensor)> = g
            .constant_fqns()
            .unwrap()
            .into_iter()
            .map(|fqn| {
                let t = weights.get(&map[&fqn]).unwrap();
                (fqn, t)
            })
            .collect();
        g.bind(&bound).unwrap_err().to_string()
    }

    /// MOKURO_NAR_FP16_PACKAGE=<…/nar-small/fp16/linux-rocm-gfx1201/model.pt2> \
    /// MOKURO_NAR_WEIGHTS=<model.safetensors> LIBTORCH=<dir> \
    ///   cargo test -p bunko-torch --features libtorch --release --lib -- --ignored a_wrong_dtype_bind_is_refused
    #[test]
    #[ignore = "needs an fp16 nar package, the weights and a GPU (env vars)"]
    fn a_wrong_dtype_bind_is_refused() {
        let (Some(pkg), Some(w)) = (
            std::env::var_os("MOKURO_NAR_FP16_PACKAGE"),
            std::env::var_os("MOKURO_NAR_WEIGHTS"),
        ) else {
            eprintln!("skipping: MOKURO_NAR_FP16_PACKAGE or MOKURO_NAR_WEIGHTS is not set");
            return;
        };
        let g = load_gpu_package(&pkg);
        assert_eq!(g.metadata("bunko.weights_dtype").as_deref(), Some("fp16"));
        // The authors' F32 tensors, NOT cast: what a pack without the cast would bind.
        let err = bind_with(&g, &w, None);
        assert!(err.contains("is F32, the package wants F16"), "{err}");
    }

    /// The other way round, with the fp32 package: weights cast to fp16 for it.
    ///
    /// MOKURO_NAR_PACKAGE=<…/nar-small/fp32/linux-rocm-gfx1201/model.pt2> \
    /// MOKURO_NAR_WEIGHTS=<model.safetensors> LIBTORCH=<dir> \
    ///   cargo test -p bunko-torch --features libtorch --release --lib -- --ignored an_fp16_bind_into_an_fp32_package_is_refused
    #[test]
    #[ignore = "needs an fp32 nar package, the weights and a GPU (env vars)"]
    fn an_fp16_bind_into_an_fp32_package_is_refused() {
        let (Some(pkg), Some(w)) = (
            std::env::var_os("MOKURO_NAR_PACKAGE"),
            std::env::var_os("MOKURO_NAR_WEIGHTS"),
        ) else {
            eprintln!("skipping: MOKURO_NAR_PACKAGE or MOKURO_NAR_WEIGHTS is not set");
            return;
        };
        let g = load_gpu_package(&pkg);
        assert_eq!(g.metadata("bunko.weights_dtype"), None);
        let err = bind_with(&g, &w, Some(Kind::Half));
        assert!(err.contains("is F16, the package wants F32"), "{err}");
        assert!(err.contains("constant net."), "{err}");
    }
}
