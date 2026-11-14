//! Opening a backend pack: `pack.json`, the cdylib (`libloading`), the ABI version
//! check, `bt_init` and `bt_devices`. The library is never unloaded (libtorch does not
//! survive `dlclose`), so a [`Pack`] lives as long as the process once opened.

use std::ffi::{CStr, CString, c_char, c_void};
use std::mem::ManuallyDrop;
use std::path::{Path, PathBuf};

use bunko_torch::abi::{
    self, BT_ABI_VERSION, BT_OK, BtCrop, CharsRead, DetectBlocksFn, DetectOut, DevicesReport,
    InitConfig, LoadInfo, LoadOptions, PACK_JSON, PackManifest, ReadCharsFn,
};

/// Why a pack could not be used.
#[derive(Debug, thiserror::Error)]
pub enum PackLoadError {
    #[error("no backend pack at {0}")]
    NotFound(PathBuf),
    #[error("{path}: {msg}")]
    Manifest { path: PathBuf, msg: String },
    #[error("cannot load {path}: {msg}")]
    Library { path: PathBuf, msg: String },
    #[error("{path} does not export {symbol} (not a bunko-torch library?)")]
    Symbol { path: PathBuf, symbol: String },
    #[error(
        "{path} implements backend ABI {found}, this mokuro-bunko needs ABI {wanted}: install the pack of this release"
    )]
    Abi {
        path: PathBuf,
        found: u32,
        wanted: u32,
    },
    #[error("the backend did not start: {0}")]
    Init(String),
}

type AbiVersionFn = unsafe extern "C" fn() -> u32;
type InitFn = unsafe extern "C" fn(*const c_char, *mut *mut c_char) -> i32;
type DevicesFn = unsafe extern "C" fn(*mut *mut c_char, *mut *mut c_char) -> i32;
type LoadFn = unsafe extern "C" fn(
    *const c_char,
    *const c_char,
    *const c_char,
    *const c_char,
    *const c_char,
    *mut *mut c_char,
) -> *mut c_void;
type InfoFn = unsafe extern "C" fn(*mut c_void, *mut *mut c_char, *mut *mut c_char) -> i32;
type ReadFn = unsafe extern "C" fn(
    *mut c_void,
    *const BtCrop,
    usize,
    *const u32,
    *mut *mut c_char,
    *mut *mut c_char,
) -> i32;
type FreeFn = unsafe extern "C" fn(*mut c_void);
type FreeStrFn = unsafe extern "C" fn(*mut c_char);

/// The library's entry points (valid while the library is loaded, i.e. forever).
#[derive(Clone, Copy)]
struct Api {
    init: InitFn,
    devices: DevicesFn,
    load: LoadFn,
    info: InfoFn,
    read: ReadFn,
    free: FreeFn,
    free_str: FreeStrFn,
    /// `bt_read_chars`, optional: packs before 0.8 lack it.
    read_chars: Option<ReadCharsFn>,
    /// `bt_detect_blocks`, optional: packs before 0.8 lack it.
    detect_blocks: Option<DetectBlocksFn>,
}

/// An opened backend pack.
pub struct Pack {
    pub dir: PathBuf,
    pub manifest: PackManifest,
    /// Environment the loader set for the pack's runtime (ROCm: `amdgpu.ids` paths,
    /// `HSA_OVERRIDE_GFX_VERSION`, a usable `TMPDIR`), for the log and `doctor`.
    pub env: Vec<(String, String)>,
    /// What the loader worked around or turned off while preparing that environment
    /// (ROCm: an unusable temporary folder); [`Pack::devices`] reports them.
    pub warnings: Vec<String>,
    api: Api,
    // Never dlclose'd: see the module docs.
    _lib: ManuallyDrop<libloading::Library>,
}

impl std::fmt::Debug for Pack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pack")
            .field("dir", &self.dir)
            .field("variant", &self.manifest.variant)
            .finish_non_exhaustive()
    }
}

/// The mokuro-bunko release this build is (the workspace version): it opens the backend
/// packs of [`abi::OLDEST_PACK_RELEASE`] up to this release (a development pack, with no
/// release, passes too).
pub const RELEASE: &str = env!("CARGO_PKG_VERSION");

/// Reads and checks `<dir>/pack.json`.
pub fn read_manifest(dir: &Path) -> Result<PackManifest, PackLoadError> {
    let path = dir.join(PACK_JSON);
    let text = std::fs::read_to_string(&path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            PackLoadError::NotFound(dir.to_path_buf())
        } else {
            PackLoadError::Manifest {
                path: path.clone(),
                msg: e.to_string(),
            }
        }
    })?;
    let m = PackManifest::parse(&text).map_err(|e| PackLoadError::Manifest {
        path: path.clone(),
        msg: e.to_string(),
    })?;
    // A pack of a release this build opens; then the ABI and the platform.
    m.check_release(RELEASE)
        .and_then(|()| m.check())
        .map_err(|e| PackLoadError::Manifest {
            path,
            msg: e.to_string(),
        })?;
    Ok(m)
}

/// Loads the pack's own libtorch CPU half (global, never unloaded) before the cdylib,
/// so that neither the cdylib's dependencies nor the compiled models' (which name
/// libtorch by soname, with no run path) can bind to another libtorch on the system
/// search path or `LD_LIBRARY_PATH`.
fn preload_core(lib_dir: &Path) -> Result<(), String> {
    for f in abi::core_libraries() {
        let p = lib_dir.join(f);
        if !p.exists() {
            continue;
        }
        #[cfg(unix)]
        {
            use libloading::os::unix::{Library, RTLD_GLOBAL, RTLD_NOW};
            // SAFETY: the pack's libtorch; its initialisers are libtorch's.
            let lib = unsafe { Library::open(Some(&p), RTLD_NOW | RTLD_GLOBAL) }.map_err(|e| {
                let m = abi::error_chain(&e);
                format!(
                    "{}: {}",
                    p.display(),
                    abi::explain_load_error(&m).unwrap_or(m)
                )
            })?;
            std::mem::forget(lib);
        }
        #[cfg(windows)]
        {
            use libloading::os::windows::{LOAD_WITH_ALTERED_SEARCH_PATH, Library};
            // SAFETY: as on unix.
            let lib = unsafe { Library::load_with_flags(&p, LOAD_WITH_ALTERED_SEARCH_PATH) }
                // libloading's message alone is "LoadLibraryExW failed": the OS error
                // is in its source (explained by `Pack::open`).
                .map_err(|e| format!("{}: {}", p.display(), abi::error_chain(&e)))?;
            std::mem::forget(lib);
        }
    }
    Ok(())
}

#[cfg(unix)]
fn open_library(path: &Path, lib_dir: &Path) -> Result<libloading::Library, String> {
    use libloading::os::unix::{Library, RTLD_LOCAL, RTLD_NOW};
    preload_core(lib_dir)?;
    // SAFETY: a bunko-torch cdylib (checked by its ABI symbol right after); its
    // initialisers are libtorch's static constructors.
    unsafe { Library::open(Some(path), RTLD_NOW | RTLD_LOCAL) }
        .map(Into::into)
        .map_err(|e| abi::error_chain(&e))
}

#[cfg(windows)]
fn open_library(path: &Path, lib_dir: &Path) -> Result<libloading::Library, String> {
    use libloading::os::windows::{
        LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, Library,
    };
    use std::os::windows::ffi::OsStrExt;
    unsafe extern "system" {
        fn AddDllDirectory(new_directory: *const u16) -> *mut c_void;
    }
    // The pack's lib/ joins the search path of every later load (libtorch's own
    // dependencies, the CUDA libraries, the AOTInductor model DLLs' imports).
    let wide: Vec<u16> = lib_dir.as_os_str().encode_wide().chain([0]).collect();
    // SAFETY: a NUL-terminated wide string that outlives the call.
    unsafe { AddDllDirectory(wide.as_ptr()) };
    preload_core(lib_dir)?;
    // SAFETY: as on unix.
    unsafe {
        Library::load_with_flags(
            path,
            LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
        )
    }
    .map(Into::into)
    .map_err(|e| abi::error_chain(&e))
}

fn take(api_free: FreeStrFn, p: *mut c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    // SAFETY: a NUL-terminated string the library handed out; freed by its own call.
    let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
    unsafe { api_free(p) };
    Some(s)
}

/// The crops as the C ABI takes them. The pack reads width*height*3 bytes behind each
/// pointer, so a crop whose slice holds another count is refused here (an empty crop,
/// of no width or no height, reads none).
fn bt_crops(crops: &[(&[u8], u32, u32)]) -> Result<Vec<BtCrop>, String> {
    crops
        .iter()
        .enumerate()
        .map(|(i, &(data, w, h))| {
            let want = (w as usize)
                .checked_mul(h as usize)
                .and_then(|n| n.checked_mul(3));
            if w > 0 && h > 0 && want != Some(data.len()) {
                return Err(format!("crop {i} has {} bytes for {w}x{h}", data.len()));
            }
            Ok(BtCrop {
                data: data.as_ptr(),
                width: w,
                height: h,
            })
        })
        .collect()
}

fn cstring(s: &str, what: &str) -> Result<CString, String> {
    CString::new(s).map_err(|_| format!("{what} contains a NUL byte"))
}

impl Pack {
    /// Opens the pack in `dir`: manifest, library, ABI check, `bt_init`.
    pub fn open(dir: &Path, cpu_only: bool) -> Result<Pack, PackLoadError> {
        let manifest = read_manifest(dir)?;
        let path = dir.join(&manifest.library);
        if !path.is_file() {
            return Err(PackLoadError::Library {
                path,
                msg: "file missing".into(),
            });
        }
        let lib_dir = dir.join(&manifest.lib_dir);
        // Before anything of the ROCm runtime is loaded (a ROCm libtorch_cpu links it).
        let rocm = if manifest.variant.starts_with("rocm") && !cpu_only {
            abi::prepare_rocm_env(dir, &manifest.requires.gpu_archs)
        } else {
            abi::RocmEnv::default()
        };
        let mut warnings = rocm.warnings;
        // The ROCm runtime would crash the process at its first GPU copy: CPU only.
        let cpu_only = cpu_only || rocm.gpu_off.is_some();
        if let Some(why) = rocm.gpu_off {
            warnings.push(format!("no GPU: {why}"));
        }
        let lib = open_library(&path, &lib_dir).map_err(|msg| PackLoadError::Library {
            path: path.clone(),
            msg: abi::explain_load_error(&msg).unwrap_or(msg),
        })?;
        let sym = |name: &[u8]| -> Result<*const c_void, PackLoadError> {
            // SAFETY: looking up a symbol; its type is fixed by the ABI version checked
            // below before any other call.
            unsafe { lib.get::<*const c_void>(name) }
                .map(|s| *s)
                .map_err(|_| PackLoadError::Symbol {
                    path: path.clone(),
                    symbol: String::from_utf8_lossy(&name[..name.len() - 1]).into_owned(),
                })
        };
        let version_ptr = sym(abi::symbols::ABI_VERSION)?;
        // SAFETY: `bt_abi_version` has had this signature in every ABI version.
        let found = unsafe { std::mem::transmute::<*const c_void, AbiVersionFn>(version_ptr)() };
        if found != BT_ABI_VERSION {
            return Err(PackLoadError::Abi {
                path,
                found,
                wanted: BT_ABI_VERSION,
            });
        }
        // SAFETY: ABI version 1 entry points with the signatures in `bunko_torch::abi`.
        let api = unsafe {
            Api {
                init: std::mem::transmute::<*const c_void, InitFn>(sym(abi::symbols::INIT)?),
                devices: std::mem::transmute::<*const c_void, DevicesFn>(sym(
                    abi::symbols::DEVICES,
                )?),
                load: std::mem::transmute::<*const c_void, LoadFn>(sym(abi::symbols::LOAD)?),
                info: std::mem::transmute::<*const c_void, InfoFn>(sym(abi::symbols::INFO)?),
                read: std::mem::transmute::<*const c_void, ReadFn>(sym(abi::symbols::READ)?),
                free: std::mem::transmute::<*const c_void, FreeFn>(sym(abi::symbols::FREE)?),
                free_str: std::mem::transmute::<*const c_void, FreeStrFn>(sym(
                    abi::symbols::FREE_STR,
                )?),
                // Optional: absent, the pack reads no text boxes on the GPU.
                read_chars: sym(abi::symbols::READ_CHARS)
                    .ok()
                    .map(|p| std::mem::transmute::<*const c_void, ReadCharsFn>(p)),
                // Optional: absent, the pack detects no blocks on the GPU.
                detect_blocks: sym(abi::symbols::DETECT_BLOCKS)
                    .ok()
                    .map(|p| std::mem::transmute::<*const c_void, DetectBlocksFn>(p)),
            }
        };
        let gpu_archs = manifest.requires.gpu_archs.clone();
        let pack = Pack {
            dir: dir.to_path_buf(),
            manifest,
            env: rocm.set,
            warnings,
            api,
            _lib: ManuallyDrop::new(lib),
        };
        let cfg = InitConfig {
            lib_dir: Some(lib_dir.to_string_lossy().into_owned()),
            cpu_only,
            gpu_archs,
        };
        let cfg = serde_json::to_string(&cfg).map_err(|e| PackLoadError::Init(e.to_string()))?;
        let cfg = cstring(&cfg, "init config").map_err(PackLoadError::Init)?;
        let mut err = std::ptr::null_mut();
        // SAFETY: valid strings and out pointer.
        let status = unsafe { (pack.api.init)(cfg.as_ptr(), &mut err) };
        if status != BT_OK {
            let msg = take(pack.api.free_str, err).unwrap_or_else(|| format!("status {status}"));
            return Err(PackLoadError::Init(msg));
        }
        Ok(pack)
    }

    /// Whether the pack's library exports `bt_read_chars` (a 0.8 pack): the text-box
    /// engines run on it only then.
    pub fn reads_chars(&self) -> bool {
        self.api.read_chars.is_some()
    }

    /// Whether the pack's library exports `bt_detect_blocks` (a 0.8 pack): the block
    /// detector runs on it only then.
    pub fn detects_blocks(&self) -> bool {
        self.api.detect_blocks.is_some()
    }

    /// An optional entry point of the pack's library (beyond the ABI's).
    pub fn optional_symbol(&self, name: &[u8]) -> Option<*const c_void> {
        // SAFETY: looking up a symbol; the caller knows its type.
        unsafe { self._lib.get::<*const c_void>(name) }
            .ok()
            .map(|s| *s)
    }

    /// libtorch libraries mapped into this process from outside the pack (Linux; empty
    /// elsewhere). Anything here means a system libtorch got in: results are undefined.
    pub fn foreign_libraries(&self) -> Vec<String> {
        abi::foreign_libraries(Some(&self.dir.join(&self.manifest.lib_dir)))
    }

    /// `bt_devices`, with the loader's own [`Pack::warnings`] first.
    pub fn devices(&self) -> Result<DevicesReport, String> {
        let (mut out, mut err) = (std::ptr::null_mut(), std::ptr::null_mut());
        // SAFETY: out pointers.
        let status = unsafe { (self.api.devices)(&mut out, &mut err) };
        let out = take(self.api.free_str, out);
        let err = take(self.api.free_str, err);
        if status != BT_OK {
            return Err(err.unwrap_or_else(|| format!("bt_devices: status {status}")));
        }
        let mut report: DevicesReport = serde_json::from_str(&out.unwrap_or_default())
            .map_err(|e| format!("bt_devices: {e}"))?;
        report.warnings.splice(0..0, self.warnings.iter().cloned());
        Ok(report)
    }

    /// `bt_load`: a handle owned by the returned [`Loaded`].
    pub fn load(
        &self,
        engine: &str,
        model_dir: &Path,
        precision: &str,
        device: &str,
        opts: &LoadOptions,
    ) -> Result<Loaded, String> {
        let engine_c = cstring(engine, "engine")?;
        let dir_c = cstring(&model_dir.to_string_lossy(), "model_dir")?;
        let prec_c = cstring(precision, "precision")?;
        let dev_c = cstring(device, "device")?;
        let opts = serde_json::to_string(opts).map_err(|e| e.to_string())?;
        let opts_c = cstring(&opts, "options")?;
        let mut err = std::ptr::null_mut();
        // SAFETY: valid NUL-terminated strings that outlive the call.
        let h = unsafe {
            (self.api.load)(
                engine_c.as_ptr(),
                dir_c.as_ptr(),
                prec_c.as_ptr(),
                dev_c.as_ptr(),
                opts_c.as_ptr(),
                &mut err,
            )
        };
        let err = take(self.api.free_str, err);
        if h.is_null() {
            return Err(err.unwrap_or_else(|| "bt_load failed".into()));
        }
        let loaded = Loaded {
            api: self.api,
            handle: h,
        };
        Ok(loaded)
    }
}

/// A loaded engine handle: a recognizer, a text-box engine or the block detector (freed
/// on drop).
pub struct Loaded {
    api: Api,
    handle: *mut c_void,
}

// SAFETY: the ABI makes a handle usable from any thread, concurrently for `bt_read`;
// `bt_free` runs only in `drop`, when no other reference exists.
unsafe impl Send for Loaded {}
unsafe impl Sync for Loaded {}

impl Drop for Loaded {
    fn drop(&mut self) {
        // SAFETY: the handle came from `bt_load` and is freed once.
        unsafe { (self.api.free)(self.handle) }
    }
}

impl Loaded {
    pub fn info(&self) -> Result<LoadInfo, String> {
        let (mut out, mut err) = (std::ptr::null_mut(), std::ptr::null_mut());
        // SAFETY: live handle, out pointers.
        let status = unsafe { (self.api.info)(self.handle, &mut out, &mut err) };
        let out = take(self.api.free_str, out);
        let err = take(self.api.free_str, err);
        if status != BT_OK {
            return Err(err.unwrap_or_else(|| format!("bt_info: status {status}")));
        }
        serde_json::from_str(&out.unwrap_or_default()).map_err(|e| format!("bt_info: {e}"))
    }

    /// `bt_read`: one text per crop. `crops` are `(rgb bytes, width, height)`.
    pub fn read(
        &self,
        crops: &[(&[u8], u32, u32)],
        caps: Option<&[u32]>,
    ) -> Result<Vec<String>, String> {
        if let Some(c) = caps
            && c.len() != crops.len()
        {
            return Err(format!("{} caps for {} crops", c.len(), crops.len()));
        }
        let raw = bt_crops(crops)?;
        let (mut out, mut err) = (std::ptr::null_mut(), std::ptr::null_mut());
        // SAFETY: `raw` and `caps` hold `crops.len()` entries and outlive the call; each
        // crop's bytes are width*height*3 (checked by `bt_crops`).
        let status = unsafe {
            (self.api.read)(
                self.handle,
                raw.as_ptr(),
                raw.len(),
                caps.map_or(std::ptr::null(), |c| c.as_ptr()),
                &mut out,
                &mut err,
            )
        };
        let out = take(self.api.free_str, out);
        let err = take(self.api.free_str, err);
        if status != BT_OK {
            return Err(err.unwrap_or_else(|| format!("bt_read: status {status}")));
        }
        let texts: Vec<String> =
            serde_json::from_str(&out.unwrap_or_default()).map_err(|e| format!("bt_read: {e}"))?;
        if texts.len() != crops.len() {
            return Err(format!(
                "bt_read returned {} texts for {} crops",
                texts.len(),
                crops.len()
            ));
        }
        Ok(texts)
    }

    /// `bt_read_chars`: each crop's text (and, for an engine that has them, its
    /// characters' boxes). `crops` are `(rgb bytes, width, height)`; an empty crop reads
    /// `""` in its place.
    pub fn read_chars(&self, crops: &[(&[u8], u32, u32)]) -> Result<Vec<CharsRead>, String> {
        let Some(read_chars) = self.api.read_chars else {
            return Err("the OCR backend pack predates GPU text-box reading".into());
        };
        let raw = bt_crops(crops)?;
        let (mut out, mut err) = (std::ptr::null_mut(), std::ptr::null_mut());
        // SAFETY: `raw` holds `crops.len()` entries and outlives the call; each crop's
        // bytes are width*height*3 (checked by `bt_crops`).
        let status =
            unsafe { read_chars(self.handle, raw.as_ptr(), raw.len(), &mut out, &mut err) };
        let out = take(self.api.free_str, out);
        let err = take(self.api.free_str, err);
        if status != BT_OK {
            return Err(err.unwrap_or_else(|| format!("bt_read_chars: status {status}")));
        }
        let read: Vec<CharsRead> = serde_json::from_str(&out.unwrap_or_default())
            .map_err(|e| format!("bt_read_chars: {e}"))?;
        // A pack that answers another count would put texts against the wrong blocks.
        if read.len() != crops.len() {
            return Err(format!(
                "bt_read_chars returned {} results for {} crops",
                read.len(),
                crops.len()
            ));
        }
        Ok(read)
    }

    /// `bt_detect_blocks`: each canvas's raw candidates scored `min_score` or more.
    /// `canvases` are `(rgb bytes, width, height)`, letterboxed pages whose sides are
    /// positive multiples of 32, at most 640 (the library refuses any other).
    pub fn detect_blocks(
        &self,
        canvases: &[(&[u8], u32, u32)],
        min_score: f32,
    ) -> Result<Vec<DetectOut>, String> {
        let Some(detect_blocks) = self.api.detect_blocks else {
            return Err("the OCR backend pack predates GPU block detection".into());
        };
        // The pack reads width*height*3 bytes of each canvas, as of a crop.
        let raw = bt_crops(canvases).map_err(|e| e.replacen("crop", "canvas", 1))?;
        let (mut out, mut err) = (std::ptr::null_mut(), std::ptr::null_mut());
        // SAFETY: `raw` holds `canvases.len()` entries and outlives the call; each
        // canvas's bytes are width*height*3 (checked by `bt_crops`).
        let status = unsafe {
            detect_blocks(
                self.handle,
                raw.as_ptr(),
                raw.len(),
                min_score,
                &mut out,
                &mut err,
            )
        };
        let out = take(self.api.free_str, out);
        let err = take(self.api.free_str, err);
        if status != BT_OK {
            return Err(err.unwrap_or_else(|| format!("bt_detect_blocks: status {status}")));
        }
        let found: Vec<DetectOut> = serde_json::from_str(&out.unwrap_or_default())
            .map_err(|e| format!("bt_detect_blocks: {e}"))?;
        // A pack that answers another count would put blocks on the wrong pages.
        if found.len() != canvases.len() {
            return Err(format!(
                "bt_detect_blocks returned {} results for {} canvases",
                found.len(),
                canvases.len()
            ));
        }
        Ok(found)
    }
}
