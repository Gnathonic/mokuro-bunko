//! The exported C ABI (see [`crate::abi`] for the contract). Every entry point catches
//! panics; strings go out as `CString::into_raw` and come back through `bt_free_str`.

use std::ffi::{CStr, CString, c_char, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;

use bunko_vlm::Rgb;

use crate::TorchError;
use crate::abi::{BT_ABI_VERSION, BT_ERR_PANIC, BT_OK, BtCrop, InitConfig, LoadOptions};
use crate::runtime::Handle;

fn put(dst: *mut *mut c_char, s: String) {
    if dst.is_null() {
        return;
    }
    // Interior NULs cannot be represented; replace them rather than lose the message.
    let c = CString::new(s.replace('\0', " ")).unwrap_or_default();
    // SAFETY: the caller passed a writable out pointer.
    unsafe { *dst = c.into_raw() };
}

/// Runs `f`, turning an error or a panic into a status code and an `err` string.
fn guarded<T>(err: *mut *mut c_char, f: impl FnOnce() -> Result<T, TorchError>) -> Result<T, i32> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => {
            put(err, e.to_string());
            Err(e.code())
        }
        Err(p) => {
            let msg = p
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| p.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "panic".into());
            put(err, format!("bunko-torch panicked: {msg}"));
            Err(BT_ERR_PANIC)
        }
    }
}

fn arg_str<'a>(p: *const c_char, what: &str) -> Result<&'a str, TorchError> {
    if p.is_null() {
        return Err(TorchError::Arg(format!("{what} is null")));
    }
    // SAFETY: the caller passes a NUL-terminated string that outlives the call.
    unsafe { CStr::from_ptr(p) }
        .to_str()
        .map_err(|_| TorchError::Arg(format!("{what} is not UTF-8")))
}

fn json_arg<T: serde::de::DeserializeOwned + Default>(
    p: *const c_char,
    what: &str,
) -> Result<T, TorchError> {
    if p.is_null() {
        return Ok(T::default());
    }
    let s = arg_str(p, what)?;
    if s.trim().is_empty() {
        return Ok(T::default());
    }
    serde_json::from_str(s).map_err(|e| TorchError::Arg(format!("{what}: {e}")))
}

fn to_json<T: serde::Serialize>(v: &T) -> Result<String, TorchError> {
    serde_json::to_string(v).map_err(|e| TorchError::Run(e.to_string()))
}

#[unsafe(no_mangle)]
pub extern "C" fn bt_abi_version() -> u32 {
    BT_ABI_VERSION
}

/// # Safety
/// `config_json` is null or a NUL-terminated string; `err` is null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bt_init(config_json: *const c_char, err: *mut *mut c_char) -> i32 {
    match guarded(err, || {
        let cfg: InitConfig = json_arg(config_json, "init config")?;
        crate::runtime::init(&cfg)
    }) {
        Ok(()) => BT_OK,
        Err(code) => code,
    }
}

/// # Safety
/// `json_out` and `err` are null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bt_devices(json_out: *mut *mut c_char, err: *mut *mut c_char) -> i32 {
    match guarded(err, || to_json(&crate::runtime::devices())) {
        Ok(s) => {
            put(json_out, s);
            BT_OK
        }
        Err(code) => code,
    }
}

/// # Safety
/// String arguments are NUL-terminated (`opts_json` may be null); `err` is null or
/// writable. Returns null on error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bt_load(
    engine: *const c_char,
    model_dir: *const c_char,
    precision: *const c_char,
    device: *const c_char,
    opts_json: *const c_char,
    err: *mut *mut c_char,
) -> *mut c_void {
    let r = guarded(err, || {
        let engine = arg_str(engine, "engine")?;
        let dir = arg_str(model_dir, "model_dir")?;
        let precision = arg_str(precision, "precision")?;
        let device = arg_str(device, "device")?;
        let opts: LoadOptions = json_arg(opts_json, "load options")?;
        crate::runtime::load(engine, Path::new(dir), precision, device, &opts)
    });
    match r {
        Ok(h) => Box::into_raw(Box::new(h)).cast(),
        Err(_) => std::ptr::null_mut(),
    }
}

fn handle<'a>(h: *mut c_void) -> Result<&'a Handle, TorchError> {
    if h.is_null() {
        return Err(TorchError::Arg("null handle".into()));
    }
    // SAFETY: a pointer `bt_load` returned and `bt_free` has not freed (caller's duty).
    Ok(unsafe { &*h.cast::<Handle>() })
}

/// # Safety
/// `h` is a live handle from `bt_load`; out pointers are null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bt_info(
    h: *mut c_void,
    json_out: *mut *mut c_char,
    err: *mut *mut c_char,
) -> i32 {
    match guarded(err, || to_json(handle(h)?.info())) {
        Ok(s) => {
            put(json_out, s);
            BT_OK
        }
        Err(code) => code,
    }
}

/// # Safety
/// `h` is a live handle; `crops` points at `n` crops whose `data` holds
/// `width * height * 3` bytes each; `caps` is null or points at `n` values; out
/// pointers are null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bt_read(
    h: *mut c_void,
    crops: *const BtCrop,
    n: usize,
    caps: *const u32,
    json_out: *mut *mut c_char,
    err: *mut *mut c_char,
) -> i32 {
    let r = guarded(err, || {
        let h = handle(h)?;
        if n > 0 && crops.is_null() {
            return Err(TorchError::Arg("crops is null".into()));
        }
        let raw: &[BtCrop] = if n == 0 {
            &[]
        } else {
            // SAFETY: the caller passes `n` crops.
            unsafe { std::slice::from_raw_parts(crops, n) }
        };
        let mut imgs = Vec::with_capacity(n);
        for (i, c) in raw.iter().enumerate() {
            let (w, hgt) = (c.width as usize, c.height as usize);
            if w == 0 || hgt == 0 || c.data.is_null() {
                return Err(TorchError::Arg(format!("crop {i} is empty")));
            }
            // SAFETY: the caller guarantees width*height*3 readable bytes.
            let bytes = unsafe { std::slice::from_raw_parts(c.data, w * hgt * 3) };
            imgs.push(
                Rgb::from_raw(w, hgt, bytes.to_vec())
                    .ok_or_else(|| TorchError::Arg(format!("crop {i} has a bad size")))?,
            );
        }
        let caps = if caps.is_null() {
            None
        } else {
            // SAFETY: the caller passes `n` caps.
            Some(unsafe { std::slice::from_raw_parts(caps, n) })
        };
        let refs: Vec<&Rgb> = imgs.iter().collect();
        to_json(&h.read(&refs, caps)?)
    });
    match r {
        Ok(s) => {
            put(json_out, s);
            BT_OK
        }
        Err(code) => code,
    }
}

/// # Safety
/// `h` is null or a handle from `bt_load` that no other thread is using.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bt_free(h: *mut c_void) {
    if !h.is_null() {
        // SAFETY: `h` came from `Box::into_raw` in `bt_load`.
        let h = unsafe { Box::from_raw(h.cast::<Handle>()) };
        let _ = catch_unwind(AssertUnwindSafe(|| crate::runtime::free_handle(h)));
    }
}

/// # Safety
/// `s` is null or a string this library handed out.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bt_free_str(s: *mut c_char) {
    if !s.is_null() {
        // SAFETY: `s` came from `CString::into_raw` in `put`.
        drop(unsafe { CString::from_raw(s) });
    }
}
