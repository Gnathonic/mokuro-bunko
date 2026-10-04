//! The C ABI between `mokuro-bunko` (which never links libtorch) and `libbunko_torch`
//! (built per backend pack against exactly that pack's libtorch), plus the `pack.json`
//! that describes a pack. Always built (no libtorch needed): the loader in
//! `bunko-engines` uses these types, the cdylib implements them.
//!
//! ```c
//! uint32_t bt_abi_version(void);
//! int32_t  bt_init(const char *config_json, char **err);          // once, before anything else
//! int32_t  bt_devices(char **json_out, char **err);               // DevicesReport
//! void    *bt_load(const char *engine, const char *model_dir, const char *precision,
//!                  const char *device, const char *opts_json, char **err);   // NULL on error
//! int32_t  bt_info(void *handle, char **json_out, char **err);    // LoadInfo
//! int32_t  bt_read(void *handle, const BtCrop *crops, size_t n, const uint32_t *caps,
//!                  char **json_out, char **err);                  // ["text", ...], one per crop
//! void     bt_free(void *handle);
//! void     bt_free_str(char *s);
//! ```
//!
//! Rules: no tensor crosses the boundary, only RGB8 crops in and UTF-8 JSON out. Every
//! string the library hands out (`json_out`, `err`) is freed with `bt_free_str`. A
//! handle may be used by any number of threads at once (`bt_read` serialises the device
//! work inside); `bt_free` must not race a `bt_read` on the same handle. A panic never
//! crosses the boundary (it becomes [`BT_ERR_PANIC`]).

use std::path::Path;

use serde::{Deserialize, Serialize};

/// The ABI this crate implements. Bumped on any change to the functions above or to
/// the meaning of their JSON.
pub const BT_ABI_VERSION: u32 = 1;

/// Success.
pub const BT_OK: i32 = 0;
/// Bad arguments (null pointers, unknown engine/precision/device, malformed JSON).
pub const BT_ERR_ARG: i32 = 1;
/// A model or library could not be loaded.
pub const BT_ERR_LOAD: i32 = 2;
/// Inference failed.
pub const BT_ERR_RUN: i32 = 3;
/// The library panicked (a bug); the handle should be dropped.
pub const BT_ERR_PANIC: i32 = 4;

/// Exported symbol names, as the loader looks them up.
pub mod symbols {
    pub const ABI_VERSION: &[u8] = b"bt_abi_version\0";
    pub const INIT: &[u8] = b"bt_init\0";
    pub const DEVICES: &[u8] = b"bt_devices\0";
    pub const LOAD: &[u8] = b"bt_load\0";
    pub const INFO: &[u8] = b"bt_info\0";
    pub const READ: &[u8] = b"bt_read\0";
    pub const FREE: &[u8] = b"bt_free\0";
    pub const FREE_STR: &[u8] = b"bt_free_str\0";
}

/// One crop: tightly packed RGB8 rows (`width * 3` bytes each), `height` rows.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct BtCrop {
    pub data: *const u8,
    pub width: u32,
    pub height: u32,
}

/// `bt_init` configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitConfig {
    /// The pack's `lib/` directory: the GPU half of libtorch (`libtorch_cuda` /
    /// `libtorch_hip`) and the vendor runtime are loaded from here. `None`: by name
    /// through the library's own search path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lib_dir: Option<String>,
    /// Skip the GPU half even when the pack has one (CPU-only use of a GPU pack).
    #[serde(default)]
    pub cpu_only: bool,
    /// The GPU architectures the pack's ROCm kernels were built for
    /// (`pack.json` `requires.gpu_archs`); decides `HSA_OVERRIDE_GFX_VERSION`
    /// ([`prepare_rocm_env`]). Empty: [`ROCM_DEFAULT_ARCHS`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gpu_archs: Vec<String>,
}

/// What `bt_devices` reports.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DevicesReport {
    pub abi: u32,
    /// The libtorch the library was built against, e.g. `2.13.0+rocm7.1`.
    pub torch: String,
    /// `cuda`, `rocm` or none: the GPU half that `bt_init` loaded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_backend: Option<String>,
    /// The CPU first, then every GPU in libtorch's order (`gpu:<n>` = torch device n).
    pub devices: Vec<DeviceEntry>,
    /// Why the GPU half or a probe failed, for the log.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// One device.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DeviceEntry {
    /// `cpu` or `gpu:<n>` (n = the libtorch device index).
    pub id: String,
    /// `cpu`, `cuda` or `rocm`.
    pub kind: String,
    /// Marketing name (`NVIDIA GeForce RTX 4090`, `AMD Radeon RX 9070 XT`) or CPU model.
    pub name: String,
    /// `sm_89`, `gfx1201` (HSA_OVERRIDE_GFX_VERSION applied), `x86_64` / `aarch64`.
    pub arch: String,
    /// CPU features that decide which compiled package runs (`avx2`, `fma`, `avx512f`,
    /// `avx512_bf16`, `amx_bf16`, `neon`, ...).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub isa: Vec<String>,
    /// Device memory in MiB (GPUs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vram_mb: Option<u64>,
    /// PCI bus id (GPUs), e.g. `0000:03:00.0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pci: Option<String>,
    /// The precisions this device computes in: `fp32` always; GPUs `fp16` and `bf16`
    /// (0.5.2's torch probe said yes on every CUDA/ROCm card); the CPU `bf16` only on
    /// x86-64-v4 hosts with AVX512_BF16 (what the bf16 CPU packages are compiled for).
    pub formats: Vec<String>,
}

/// `bt_load` options.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoadOptions {
    /// The model's `tokenizer.json`.
    pub tokenizer: String,
    /// hayai-nova: f32 SigLIP2 position table (`hayai-nova/pos-table`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pos_table: Option<String>,
    /// hayai-nova: f32 decoder token embeddings; paddle-manga: the f32 (or f16) input
    /// embedding table, cast to the model precision on load (not needed when the
    /// packages' weights carry it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embeddings: Option<String>,
    /// hayai-nova `max_num_patches` (the packages are compiled for one budget, 512).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch_budget: Option<u32>,
    /// CPU threads for the AOTInductor CPU kernels and ATen (OpenMP), set on every
    /// `bt_read` thread. 0 = libtorch's default.
    #[serde(default)]
    pub threads: u32,
    /// Graph files when they are not `<model_dir>/{vision,prefill,step}.pt2`. A `.pt2`
    /// may be the zip AOTInductor writes or that zip unpacked into a directory (loaded
    /// in place, no extraction to the temp directory).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefill: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    /// Where `.pt2` zips are unpacked (once, then loaded in place). Default:
    /// `<model_dir>/.unpacked`. Unpacked package directories need none of this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_dir: Option<String>,
    /// The shared weights files of weightless (GPU) packages. Default: every
    /// `weights-*.safetensors` in the parent of `model_dir` (`<engine>/<precision>/`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weights: Option<Vec<String>>,
}

/// `bt_info`: what a loaded handle is.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LoadInfo {
    pub engine: String,
    pub precision: String,
    pub device: String,
    /// Crops per generation batch.
    pub batch: u32,
    pub load_seconds: f64,
    pub torch: String,
    /// The packages' graph I/O contract (`bunko.io`; 1 = the shootout's).
    #[serde(default)]
    pub io: u32,
    /// Weight tensors bound from the weights files (0: the packages carry their own).
    #[serde(default)]
    pub weights: u32,
}

/// `pack.json` format this build reads.
pub const PACK_FORMAT: u32 = 1;
/// The file name of a pack's description.
pub const PACK_JSON: &str = "pack.json";

/// A backend pack: `<storage>/backends/torch-<variant>-<torch version>/pack.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackManifest {
    /// [`PACK_FORMAT`].
    pub format: u32,
    /// `cpu`, `cu130` or `rocm7.1`.
    pub variant: String,
    /// The libtorch version, e.g. `2.13.0`.
    pub torch: String,
    /// The [`BT_ABI_VERSION`] the library implements.
    pub abi: u32,
    /// `std::env::consts::OS` / `ARCH` of the build: `linux`, `windows`, `macos`;
    /// `x86_64`, `aarch64`.
    pub os: String,
    pub arch: String,
    /// The cdylib, relative to the pack (`libbunko_torch.so`, `bunko_torch.dll`, ...).
    pub library: String,
    /// The libtorch runtime directory, relative to the pack.
    #[serde(default = "default_lib_dir")]
    pub lib_dir: String,
    /// Every file of the pack (for install-time verification).
    #[serde(default)]
    pub files: Vec<PackFile>,
    /// What the host must provide (only the fields the loader reads).
    #[serde(default)]
    pub requires: PackRequires,
}

/// `pack.json` `requires` (the loader's part of it).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackRequires {
    /// GPU architectures the ROCm runtime's kernels were built for.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gpu_archs: Vec<String>,
}

fn default_lib_dir() -> String {
    "lib".into()
}

/// One file of a pack.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackFile {
    /// Relative to the pack, `/`-separated.
    pub path: String,
    pub size: u64,
    /// Lowercase hex sha256.
    pub sha256: String,
}

/// Why a pack cannot be used here.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PackError {
    #[error("pack.json is not valid: {0}")]
    Malformed(String),
    #[error("pack.json format {0} is not supported (this build reads format {PACK_FORMAT})")]
    Format(u32),
    #[error("the pack implements backend ABI {found}, this mokuro-bunko needs ABI {wanted}")]
    Abi { found: u32, wanted: u32 },
    #[error("the pack is for {found}, this machine is {wanted}")]
    Platform { found: String, wanted: String },
    #[error("unsafe path in pack.json: {0}")]
    Path(String),
}

/// A pack-relative path with no way out of the pack (no `..`, not absolute).
pub fn safe_relative(p: &str) -> bool {
    !p.is_empty()
        && !p.starts_with('/')
        && !p.starts_with('\\')
        && !p.contains(':')
        && p.split(['/', '\\']).all(|c| !c.is_empty() && c != "..")
}

impl PackManifest {
    pub fn parse(text: &str) -> Result<PackManifest, PackError> {
        serde_json::from_str(text).map_err(|e| PackError::Malformed(e.to_string()))
    }

    /// Whether this pack can be loaded by this build on this machine (ABI, format, OS,
    /// architecture, paths). It does not touch the files.
    pub fn check(&self) -> Result<(), PackError> {
        if self.format != PACK_FORMAT {
            return Err(PackError::Format(self.format));
        }
        if self.abi != BT_ABI_VERSION {
            return Err(PackError::Abi {
                found: self.abi,
                wanted: BT_ABI_VERSION,
            });
        }
        let here = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
        let pack = format!("{}-{}", self.os, self.arch);
        if pack != here {
            return Err(PackError::Platform {
                found: pack,
                wanted: here,
            });
        }
        for p in std::iter::once(&self.library)
            .chain(std::iter::once(&self.lib_dir))
            .chain(self.files.iter().map(|f| &f.path))
        {
            if !safe_relative(p) {
                return Err(PackError::Path(p.clone()));
            }
        }
        Ok(())
    }

    /// The directory name packs are installed under: `torch-<variant>-<torch>`.
    pub fn dir_name(&self) -> String {
        format!("torch-{}-{}", self.variant, self.torch)
    }

    /// Whether this is a GPU variant (CUDA or ROCm), which also runs on the CPU.
    pub fn is_gpu(&self) -> bool {
        self.variant != "cpu"
    }
}

/// The platform file name of the cdylib (`libbunko_torch.so`, `bunko_torch.dll`,
/// `libbunko_torch.dylib`).
pub fn library_file_name() -> String {
    format!(
        "{}bunko_torch{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    )
}

// ---------------------------------------------------------------------------
// Loading a pack's libtorch (used by the cdylib's `bt_init` and by the loader)

/// The CPU half, in dependency order, loaded from the pack before anything else: the
/// AOTInductor model libraries name these (and `libtorch` itself) as plain sonames and
/// carry no run path, so whatever is loaded under that name is what they bind to. If
/// the pack's copy were not loaded first, a system libtorch (`/usr/lib/libtorch.so`)
/// would be picked up.
pub fn core_libraries() -> &'static [&'static str] {
    if cfg!(target_os = "windows") {
        &["c10.dll", "torch_cpu.dll"]
    } else if cfg!(target_os = "macos") {
        &["libomp.dylib", "libc10.dylib", "libtorch_cpu.dylib"]
    } else {
        &["libgomp.so.1", "libc10.so", "libtorch_cpu.so"]
    }
}

/// The umbrella library the model libraries also need (it links the GPU half, so it is
/// loaded after it).
pub fn umbrella_library() -> &'static str {
    if cfg!(target_os = "windows") {
        "torch.dll"
    } else if cfg!(target_os = "macos") {
        "libtorch.dylib"
    } else {
        "libtorch.so"
    }
}

/// libtorch / c10 libraries mapped into this process from anywhere but `lib_dir`
/// (Linux: `/proc/self/maps`; elsewhere nothing is checked).
pub fn foreign_libraries(lib_dir: Option<&Path>) -> Vec<String> {
    let Some(dir) = lib_dir.and_then(|d| std::fs::canonicalize(d).ok()) else {
        return Vec::new();
    };
    let Ok(maps) = std::fs::read_to_string("/proc/self/maps") else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    // address perms offset dev inode [path, which may contain spaces]
    let path_of = |l: &str| {
        let mut rest = l;
        for _ in 0..5 {
            rest = rest.trim_start().split_once(char::is_whitespace)?.1;
        }
        Some(rest.trim().to_string()).filter(|p| p.starts_with('/'))
    };
    for path in maps.lines().filter_map(path_of) {
        let p = Path::new(&path);
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default();
        if !(name.starts_with("libtorch") || name.starts_with("libc10")) {
            continue;
        }
        let parent = p
            .parent()
            .and_then(|d| std::fs::canonicalize(d).ok())
            .unwrap_or_default();
        if parent != dir && !out.contains(&path) {
            out.push(path);
        }
    }
    out
}

/// An error with its sources (`libloading` puts the dynamic loader's message in the
/// source: its own text is only "dlopen failed").
pub fn error_chain(e: &dyn std::error::Error) -> String {
    let mut s = e.to_string();
    let mut cur = e.source();
    while let Some(src) = cur {
        let t = src.to_string();
        if !s.contains(&t) {
            s.push_str(": ");
            s.push_str(&t);
        }
        cur = src.source();
    }
    s
}

/// A plain-language reason for a shared-library load failure that means "built for a
/// newer or different system", or `None` for anything else.
pub fn explain_load_error(msg: &str) -> Option<String> {
    let version_after = |tag: &str| {
        msg.find(tag).map(|i| {
            msg[i..]
                .split(|c: char| c == '\'' || c == '`' || c.is_whitespace() || c == ')')
                .next()
                .unwrap_or(tag)
                .to_string()
        })
    };
    if msg.contains("not found") {
        if let Some(v) = version_after("GLIBCXX_").or_else(|| version_after("CXXABI_")) {
            return Some(format!(
                "built for a newer system: needs the C++ runtime's {v}, which this system's libstdc++ lacks ({msg})"
            ));
        }
        if let Some(v) = version_after("GLIBC_") {
            return Some(format!(
                "built for a newer system: needs {v}, newer than this system's glibc ({msg})"
            ));
        }
    }
    if let Some(i) = msg.find(": cannot open shared object file") {
        let lib = msg[..i].rsplit([' ', '/']).next().unwrap_or("a library");
        return Some(format!(
            "{lib} is missing: neither the pack nor this system provides it (reinstall the pack with 'mokuro-bunko install-ocr'; 'mokuro-bunko doctor' lists missing host libraries) ({msg})"
        ));
    }
    if msg.contains("cannot enable executable stack") {
        return Some(format!(
            "the library asks for an executable stack, which this system refuses (glibc 2.41+, SELinux or a hardened kernel); it must be rebuilt with -z noexecstack ({msg})"
        ));
    }
    None
}

// ---------------------------------------------------------------------------
// The ROCm runtime's environment (set before it starts: before the pack's GPU half,
// and before `libtorch_cpu` of a ROCm build, which links the HIP runtime)

/// The variable that makes the ROCm runtime treat a card as another target.
pub const HSA_OVERRIDE_VAR: &str = "HSA_OVERRIDE_GFX_VERSION";
/// Where libdrm_amdgpu looks for `amdgpu.ids` (`:`-separated directories).
pub const ASIC_ID_PATHS_VAR: &str = "AMDGPU_ASIC_ID_TABLE_PATHS";
/// The ROCm targets the packs' kernels are built for when `pack.json` does not say.
pub const ROCM_DEFAULT_ARCHS: &[&str] = &[
    "gfx1030", "gfx1100", "gfx1101", "gfx1102", "gfx1200", "gfx1201",
];

/// KFD's `gfx_target_version` (`100302`) → `gfx1032`.
pub fn gfx_from_target_version(v: u32) -> Option<String> {
    (v > 0).then(|| format!("gfx{}{}{:x}", v / 10000, (v / 100) % 100, v % 100))
}

/// `HSA_OVERRIDE_GFX_VERSION=10.3.0` → `gfx1030` (the code objects the runtime picks).
pub fn gfx_from_override(v: &str) -> Option<String> {
    let parts: Vec<u32> = v
        .trim()
        .split('.')
        .map(|p| p.parse().ok())
        .collect::<Option<_>>()?;
    match parts[..] {
        [ma, mi, st] => Some(format!("gfx{ma}{mi}{st:x}")),
        _ => None,
    }
}

/// The GPU targets the kernel's ROCm driver sees (`/sys/class/kfd/kfd/topology`),
/// CPU nodes left out.
pub fn kfd_gfx_targets(sysfs: &Path) -> Vec<String> {
    let nodes = sysfs.join("class/kfd/kfd/topology/nodes");
    let Ok(rd) = std::fs::read_dir(nodes) else {
        return Vec::new();
    };
    let mut dirs: Vec<_> = rd.flatten().map(|e| e.path()).collect();
    dirs.sort();
    dirs.iter()
        .filter_map(|d| std::fs::read_to_string(d.join("properties")).ok())
        .filter_map(|props| {
            props.lines().find_map(|l| {
                let (k, v) = l.split_once(' ')?;
                (k == "gfx_target_version").then(|| v.trim().parse::<u32>().ok())?
            })
        })
        .filter_map(gfx_from_target_version)
        .collect()
}

/// 0.5.2's `rocm_gfx.override_for`: a card whose own target is not in the build but
/// whose family's `…0` target is (an RX 6600 is gfx1032, run as gfx1030) gets
/// `HSA_OVERRIDE_GFX_VERSION` = that family (`10.3.0`).
pub fn rocm_override_for(targets: &[String], built: &[String]) -> Option<String> {
    for t in targets {
        if built.contains(t) {
            continue;
        }
        let digits = t.strip_prefix("gfx")?;
        if digits.len() < 3 {
            continue;
        }
        let (major, minor) = (
            &digits[..digits.len() - 2],
            &digits[digits.len() - 2..digits.len() - 1],
        );
        if built.iter().any(|b| *b == format!("gfx{major}{minor}0")) {
            let (Ok(ma), Ok(mi)) = (major.parse::<u32>(), u32::from_str_radix(minor, 16)) else {
                continue;
            };
            return Some(format!("{ma}.{mi}.0"));
        }
    }
    None
}

/// Prepares the environment of a pack's ROCm runtime; returns what was set (for the
/// log and `doctor`). Never overrides a value the user set.
///
/// * `AMDGPU_ASIC_ID_TABLE_PATHS`: the pack's `share/libdrm`, then `/usr/share/libdrm`.
///   The libdrm_amdgpu that libtorch's ROCm build bundles otherwise looks for
///   `amdgpu.ids` under its build prefix only, walks the executable's directory tree
///   when that is missing (seconds), prints "(null): No such file or directory" and
///   names every card "AMD Radeon Graphics".
/// * `HSA_OVERRIDE_GFX_VERSION`: [`rocm_override_for`] the cards in the kfd topology
///   and the pack's built targets (`gpu_archs`, or [`ROCM_DEFAULT_ARCHS`]).
///
/// Must run before the ROCm runtime starts. It changes the process environment, so
/// it is called while the process loads its OCR backend, before any OCR thread runs.
pub fn prepare_rocm_env(pack_dir: &Path, gpu_archs: &[String]) -> Vec<(String, String)> {
    let mut set = Vec::new();
    let mut dirs: Vec<String> = Vec::new();
    for d in [
        pack_dir.join("share/libdrm"),
        Path::new("/usr/share/libdrm").to_path_buf(),
    ] {
        if d.join("amdgpu.ids").is_file() {
            dirs.push(d.to_string_lossy().into_owned());
        }
    }
    let current = std::env::var(ASIC_ID_PATHS_VAR).unwrap_or_default();
    let have: Vec<&str> = current.split(':').filter(|p| !p.is_empty()).collect();
    let add: Vec<String> = dirs
        .into_iter()
        .filter(|d| !have.contains(&d.as_str()))
        .collect();
    if !add.is_empty() {
        let mut v = add;
        v.extend(have.iter().map(|s| s.to_string()));
        let value = v.join(":");
        // SAFETY: called while the backend loads, before the OCR threads that could read
        // the environment concurrently exist (see the function docs).
        unsafe { std::env::set_var(ASIC_ID_PATHS_VAR, &value) };
        set.push((ASIC_ID_PATHS_VAR.to_string(), value));
    }
    if std::env::var_os(HSA_OVERRIDE_VAR).is_none_or(|v| v.is_empty()) {
        let built: Vec<String> = if gpu_archs.is_empty() {
            ROCM_DEFAULT_ARCHS.iter().map(|s| s.to_string()).collect()
        } else {
            gpu_archs.to_vec()
        };
        if let Some(v) = rocm_override_for(&kfd_gfx_targets(Path::new("/sys")), &built) {
            // SAFETY: as above.
            unsafe { std::env::set_var(HSA_OVERRIDE_VAR, &v) };
            set.push((HSA_OVERRIDE_VAR.to_string(), v));
        }
    }
    set
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> PackManifest {
        PackManifest {
            format: PACK_FORMAT,
            variant: "rocm7.1".into(),
            torch: "2.13.0".into(),
            abi: BT_ABI_VERSION,
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
            library: library_file_name(),
            lib_dir: "lib".into(),
            files: vec![PackFile {
                path: "lib/libc10.so".into(),
                size: 1,
                sha256: "00".into(),
            }],
            requires: PackRequires::default(),
        }
    }

    #[test]
    fn manifest_round_trips_and_checks() {
        let m = manifest();
        let text = serde_json::to_string(&m).unwrap();
        let back = PackManifest::parse(&text).unwrap();
        assert_eq!(back, m);
        back.check().unwrap();
        assert_eq!(back.dir_name(), "torch-rocm7.1-2.13.0");
        assert!(back.is_gpu());
    }

    #[test]
    fn bad_manifests_are_refused() {
        assert!(matches!(
            PackManifest::parse("{not json"),
            Err(PackError::Malformed(_))
        ));
        assert!(matches!(
            PackManifest::parse(r#"{"format": 1}"#),
            Err(PackError::Malformed(_))
        ));
        let mut m = manifest();
        m.abi = BT_ABI_VERSION + 1;
        assert!(matches!(m.check(), Err(PackError::Abi { .. })));
        let mut m = manifest();
        m.format = 9;
        assert_eq!(m.check(), Err(PackError::Format(9)));
        let mut m = manifest();
        m.os = "plan9".into();
        assert!(matches!(m.check(), Err(PackError::Platform { .. })));
        let mut m = manifest();
        m.library = "../../evil.so".into();
        assert!(matches!(m.check(), Err(PackError::Path(_))));
        let mut m = manifest();
        m.files[0].path = "/etc/passwd".into();
        assert!(matches!(m.check(), Err(PackError::Path(_))));
    }

    #[test]
    fn load_errors_are_explained() {
        let e = explain_load_error(
            "Error in dlopen: /x/model.so: /lib64/libc.so.6: version `GLIBC_2.38' not found (required by /x/model.so)",
        )
        .unwrap();
        assert!(
            e.starts_with("built for a newer system: needs GLIBC_2.38"),
            "{e}"
        );
        let e = explain_load_error("libstdc++.so.6: version `GLIBCXX_3.4.32' not found").unwrap();
        assert!(e.contains("GLIBCXX_3.4.32"), "{e}");
        let e = explain_load_error("libtorch_cpu.so: cannot enable executable stack as shared object requires: Invalid argument").unwrap();
        assert!(e.contains("noexecstack"), "{e}");
        let e = explain_load_error(
            "/p/lib/libtorch_cpu.so: libamd_comgr.so.3: cannot open shared object file: No such file or directory",
        )
        .unwrap();
        assert!(e.starts_with("libamd_comgr.so.3 is missing"), "{e}");
        assert!(explain_load_error("undefined symbol: foo").is_none());
    }

    #[test]
    fn rocm_overrides_follow_0_5_2() {
        let v = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let built = v(ROCM_DEFAULT_ARCHS);
        assert_eq!(
            rocm_override_for(&v(&["gfx1032"]), &built).as_deref(),
            Some("10.3.0")
        );
        assert_eq!(
            rocm_override_for(&v(&["gfx1031"]), &built).as_deref(),
            Some("10.3.0")
        );
        assert_eq!(rocm_override_for(&v(&["gfx1030"]), &built), None);
        assert_eq!(rocm_override_for(&v(&["gfx1201"]), &built), None);
        assert_eq!(
            rocm_override_for(&v(&["gfx1103"]), &built).as_deref(),
            Some("11.0.0")
        );
        // a family the build lacks entirely: nothing to do
        assert_eq!(rocm_override_for(&v(&["gfx906"]), &built), None);
        assert_eq!(
            rocm_override_for(&v(&["gfx90c"]), &v(&["gfx900"])).as_deref(),
            Some("9.0.0")
        );
        assert_eq!(gfx_from_target_version(100302).as_deref(), Some("gfx1032"));
        assert_eq!(gfx_from_override("10.3.0").as_deref(), Some("gfx1030"));
        // a fake kfd topology
        let tmp = std::env::temp_dir().join(format!("bt-kfd-{}", std::process::id()));
        for (n, gv) in [("0", 0), ("1", 100302)] {
            let d = tmp.join("class/kfd/kfd/topology/nodes").join(n);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(
                d.join("properties"),
                format!("simd_count 0\ngfx_target_version {gv}\n"),
            )
            .unwrap();
        }
        assert_eq!(kfd_gfx_targets(&tmp), vec!["gfx1032"]);
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn relative_paths() {
        assert!(safe_relative("lib/libtorch_cpu.so"));
        assert!(safe_relative("bunko_torch.dll"));
        for bad in ["", "/abs", "\\abs", "a/../b", "..", "C:/x", "a//b"] {
            assert!(!safe_relative(bad), "{bad}");
        }
    }

    #[test]
    fn json_shapes() {
        let o: LoadOptions = serde_json::from_str(r#"{"tokenizer": "t.json"}"#).unwrap();
        assert_eq!(o.threads, 0);
        assert!(o.pos_table.is_none());
        let r = DevicesReport {
            abi: 1,
            torch: "2.13.0+cpu".into(),
            gpu_backend: None,
            devices: vec![DeviceEntry {
                id: "cpu".into(),
                kind: "cpu".into(),
                name: "CPU".into(),
                arch: "x86_64".into(),
                isa: vec!["avx2".into()],
                vram_mb: None,
                pci: None,
                formats: vec!["fp32".into()],
            }],
            warnings: vec![],
        };
        let v: serde_json::Value = serde_json::to_value(&r).unwrap();
        assert!(v.get("gpu_backend").is_none());
        assert_eq!(v["devices"][0]["isa"][0], "avx2");
    }
}
