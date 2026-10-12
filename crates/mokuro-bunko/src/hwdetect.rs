//! Which OCR backend pack fits this machine (`install-ocr`, `doctor`), found without
//! loading any GPU library: NVIDIA from the kernel driver (`/proc/driver/nvidia/version`,
//! `nvidia-smi`), AMD from the ROCm kernel interface (`/dev/kfd` + KFD topology),
//! Windows NVIDIA from `nvcuda.dll`. The pack's own `bt_devices` is the authority once
//! a pack is installed; this only picks which pack to download.
//!
//! GPUs the runtimes are told to hide count as absent: `CUDA_VISIBLE_DEVICES` for
//! NVIDIA; `HIP_VISIBLE_DEVICES`, `ROCR_VISIBLE_DEVICES` and (as HIP honours it on AMD
//! too) `CUDA_VISIBLE_DEVICES` for AMD. Set empty or to `-1`, no GPU of that vendor is
//! visible to the process, so a GPU pack would only run on the CPU.
//!
//! In a container the kernel's view is not enough: the host's NVIDIA driver shows in
//! `/proc/driver/nvidia` and the AMD topology in `/sys/class/kfd` whether or not the
//! container was given the GPU. There a GPU counts only with its device nodes and,
//! for NVIDIA, the driver library the container toolkit mounts (`libcuda.so.1`); for
//! AMD, `/dev/kfd` and a `/dev/dri` render node this process may open ([`Access`]).
//!
//! [`preferred`] applies the owner's `ocr.backend` preference (`cpu`, `cuda`, `rocm`,
//! `auto`) on top of what was found.

use std::path::Path;

/// CUDA 13.0 needs this NVIDIA driver (Linux 580.65.06, Windows 580.88).
pub const MIN_NVIDIA_DRIVER: (u32, u32) = (580, 65);

/// ROCm architectures the rocm7.1 pack carries kernels for (packaging spec), and the
/// RDNA2 parts that run gfx1030 kernels with `HSA_OVERRIDE_GFX_VERSION=10.3.0` (which
/// the backend sets itself when the variable is unset: `bunko_torch::abi::prepare_rocm_env`).
pub const ROCM_ARCHS: &[&str] = &[
    "gfx1030", "gfx1100", "gfx1101", "gfx1102", "gfx1200", "gfx1201",
];
const ROCM_OVERRIDE_1030: &[&str] = &["gfx1031", "gfx1032", "gfx1034"];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Hardware {
    /// NVIDIA driver version (`595.58.03`) when the driver is loaded.
    pub nvidia_driver: Option<String>,
    /// NVIDIA GPU names (from nvidia-smi, when present).
    pub nvidia_gpus: Vec<String>,
    /// AMD GPUs visible to ROCm, as `gfx` targets (`gfx1201`).
    pub amd_gfx: Vec<String>,
    /// GPUs left out because the environment hides them (`HIP_VISIBLE_DEVICES=` ->
    /// `AMD gfx1201 (hidden by HIP_VISIBLE_DEVICES="")`), for the reason shown.
    pub hidden: Vec<String>,
}

/// A `*_VISIBLE_DEVICES` value that hides every device: empty, or a list starting with
/// a negative index (`-1`; the runtimes stop at the first invalid entry).
pub fn hides_all(value: &str) -> bool {
    let first = value.split(',').next().unwrap_or("").trim();
    first.is_empty() || first.starts_with('-')
}

/// Drops the GPUs whose vendor's visibility variable hides them all; `var` reads the
/// environment (a parameter for the tests).
pub fn apply_visibility(hw: &mut Hardware, var: impl Fn(&str) -> Option<String>) {
    // The variable, if it hides every device: `NAME=""`.
    let hiding = |n: &str| {
        var(n)
            .filter(|v| hides_all(v))
            .map(|v| format!("{n}={v:?}"))
    };
    if hw.nvidia_driver.is_some()
        && let Some(by) = hiding("CUDA_VISIBLE_DEVICES")
    {
        let what = if hw.nvidia_gpus.is_empty() {
            "NVIDIA GPU".to_string()
        } else {
            hw.nvidia_gpus.join(", ")
        };
        hw.hidden.push(format!("{what} (hidden by {by})"));
        hw.nvidia_driver = None;
        hw.nvidia_gpus.clear();
    }
    // AMD: ROCr's variable filters first; then HIP's, which falls back to CUDA's when
    // unset ("same effect as HIP_VISIBLE_DEVICES on the AMD platform").
    let hip = if var("HIP_VISIBLE_DEVICES").is_some() {
        "HIP_VISIBLE_DEVICES"
    } else {
        "CUDA_VISIBLE_DEVICES"
    };
    if !hw.amd_gfx.is_empty()
        && let Some(by) = hiding("ROCR_VISIBLE_DEVICES").or_else(|| hiding(hip))
    {
        hw.hidden
            .push(format!("AMD {} (hidden by {by})", hw.amd_gfx.join(", ")));
        hw.amd_gfx.clear();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub variant: &'static str,
    /// Why, for the user.
    pub reason: String,
    /// Something the user should know or do (old driver, HSA override, ...).
    pub hint: Option<String>,
}

pub fn detect() -> Hardware {
    let mut hw = Hardware::default();
    if cfg!(target_os = "linux") {
        if let Ok(text) = std::fs::read_to_string("/proc/driver/nvidia/version") {
            hw.nvidia_driver = parse_nvidia_proc(&text);
        }
        hw.amd_gfx = amd_gfx_targets(Path::new("/sys/class/kfd/kfd/topology/nodes"));
    }
    if cfg!(windows) {
        let sys = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
        if Path::new(&sys)
            .join("System32")
            .join("nvcuda.dll")
            .is_file()
        {
            hw.nvidia_driver = Some(String::new());
        }
    }
    if hw.nvidia_driver.is_some() || cfg!(target_os = "linux") {
        // nvidia-smi gives names and (on Windows) the version; absent: not fatal.
        if let Ok(out) = std::process::Command::new("nvidia-smi")
            .args(["--query-gpu=name,driver_version", "--format=csv,noheader"])
            .output()
            && out.status.success()
        {
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                let mut it = line.rsplitn(2, ',');
                let ver = it.next().unwrap_or("").trim();
                let name = it.next().unwrap_or("").trim();
                if !name.is_empty() {
                    hw.nvidia_gpus.push(name.to_string());
                }
                if hw.nvidia_driver.as_deref().is_none_or(str::is_empty) && !ver.is_empty() {
                    hw.nvidia_driver = Some(ver.to_string());
                }
            }
        }
    }
    if cfg!(target_os = "linux") {
        apply_access(&mut hw, &Access::probe());
    }
    apply_visibility(&mut hw, |n| std::env::var(n).ok());
    hw
}

/// What this process can reach of the GPUs the kernel has (Linux).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Access {
    /// Running in a container (Docker, Podman): the kernel's view includes the host's
    /// GPUs whether or not they were passed in.
    pub in_container: bool,
    /// `/dev/nvidiactl` and at least one `/dev/nvidia<N>`.
    pub nvidia_devices: bool,
    /// The NVIDIA driver library (`libcuda.so.1`) is installed or mounted here.
    pub libcuda: bool,
    /// `/dev/kfd` exists.
    pub kfd: bool,
    /// At least one `/dev/dri/renderD*` exists.
    pub render_node: bool,
    /// This process may open `/dev/kfd` and a render node for reading and writing.
    pub amd_permitted: bool,
    /// The host has the ROCm driver (`/sys/class/kfd/kfd/topology/nodes` lists nodes)
    /// but their properties cannot be read: a container without `/dev/kfd` (Docker
    /// refuses the read), so the GPU is not even named.
    pub amd_topology_blocked: bool,
}

impl Access {
    pub fn probe() -> Access {
        let render_nodes: Vec<std::path::PathBuf> = std::fs::read_dir("/dev/dri")
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.path())
                    .filter(|p| {
                        p.file_name()
                            .is_some_and(|n| n.to_string_lossy().starts_with("renderD"))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let kfd = Path::new("/dev/kfd");
        Access {
            in_container: in_container(),
            nvidia_devices: Path::new("/dev/nvidiactl").exists()
                && std::fs::read_dir("/dev").is_ok_and(|rd| {
                    rd.flatten().any(|e| {
                        let n = e.file_name().to_string_lossy().into_owned();
                        n.strip_prefix("nvidia")
                            .is_some_and(|i| !i.is_empty() && i.chars().all(|c| c.is_ascii_digit()))
                    })
                }),
            libcuda: library_present("libcuda.so.1"),
            kfd: kfd.exists(),
            render_node: !render_nodes.is_empty(),
            amd_permitted: read_write(kfd) && render_nodes.iter().any(|p| read_write(p)),
            amd_topology_blocked: std::fs::read_dir("/sys/class/kfd/kfd/topology/nodes").is_ok_and(
                |rd| {
                    rd.flatten().any(|e| {
                        std::fs::read_to_string(e.path().join("properties"))
                            .is_err_and(|err| err.kind() == std::io::ErrorKind::PermissionDenied)
                    })
                },
            ),
        }
    }
}

#[cfg(unix)]
fn read_write(p: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(p.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `c` is a valid NUL-terminated path for the duration of the call.
    unsafe { libc::access(c.as_ptr(), libc::R_OK | libc::W_OK) == 0 }
}

#[cfg(not(unix))]
fn read_write(_p: &Path) -> bool {
    false
}

/// A container: Docker's `/.dockerenv`, Podman's `/run/.containerenv`, or the images'
/// `MOKURO_INSTALL_KIND=docker`.
pub fn in_container() -> bool {
    Path::new("/.dockerenv").exists()
        || Path::new("/run/.containerenv").exists()
        || std::env::var("MOKURO_INSTALL_KIND").is_ok_and(|k| k == "docker")
}

/// Whether the dynamic loader can find `name` (`ldconfig -p`, then the usual library
/// directories). Linux only (elsewhere: true).
pub fn library_present(name: &str) -> bool {
    if !cfg!(target_os = "linux") {
        return true;
    }
    let cache = ["/sbin/ldconfig", "/usr/sbin/ldconfig", "ldconfig"]
        .iter()
        .find_map(|c| std::process::Command::new(c).arg("-p").output().ok())
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    library_in(&cache, name, |p| p.exists())
}

/// [`library_present`] over an `ldconfig -p` listing and a file check (for the tests).
pub fn library_in(ldconfig: &str, name: &str, exists: impl Fn(&Path) -> bool) -> bool {
    const DIRS: &[&str] = &[
        "/lib",
        "/lib64",
        "/usr/lib",
        "/usr/lib64",
        "/lib/x86_64-linux-gnu",
        "/usr/lib/x86_64-linux-gnu",
        "/usr/local/lib",
        "/usr/local/nvidia/lib64",
    ];
    ldconfig
        .lines()
        .any(|line| line.trim_start().starts_with(&format!("{name} ")))
        || DIRS.iter().any(|d| exists(&Path::new(d).join(name)))
}

/// Drops the GPUs this process cannot use although the kernel has them: in a
/// container, an NVIDIA GPU the container toolkit did not pass in, an AMD GPU without
/// `/dev/kfd`, a render node, or the permission to open them. Each is named in
/// [`Hardware::hidden`] with what to change.
pub fn apply_access(hw: &mut Hardware, a: &Access) {
    if a.in_container && hw.nvidia_driver.is_some() && !(a.nvidia_devices && a.libcuda) {
        let what = match hw.nvidia_driver.as_deref() {
            Some(d) if !d.is_empty() => format!("NVIDIA driver {d}"),
            _ => "NVIDIA driver".to_string(),
        };
        hw.hidden.push(format!(
            "{what} on the host, but this container has no access to the GPU{} (start it with --gpus all, or --runtime=nvidia with NVIDIA_VISIBLE_DEVICES=all and NVIDIA_DRIVER_CAPABILITIES=compute,utility)",
            if a.nvidia_devices {
                ": no libcuda.so.1"
            } else {
                ""
            }
        ));
        hw.nvidia_driver = None;
        hw.nvidia_gpus.clear();
    }
    if hw.amd_gfx.is_empty() {
        if a.in_container && !a.kfd && a.amd_topology_blocked {
            hw.hidden.push(
                "AMD GPU (the host has the ROCm driver, but this container has no /dev/kfd: start it with --device /dev/kfd --device /dev/dri)"
                    .into(),
            );
        }
        return;
    }
    let gpus = format!("AMD {}", hw.amd_gfx.join(", "));
    let why = if !a.kfd {
        // Outside a container a machine without /dev/kfd has no ROCm driver: nothing
        // to say. In one, the host's topology shows through.
        a.in_container.then(|| {
            "this container has no /dev/kfd (start it with --device /dev/kfd --device /dev/dri)"
                .to_string()
        })
    } else if a.in_container && !a.render_node {
        Some("this container has no /dev/dri render node (start it with --device /dev/dri)".into())
    } else if a.in_container && !a.amd_permitted {
        Some(
            "this user may not open /dev/kfd and /dev/dri/renderD* (give it their groups, e.g. --group-add for the render and video groups)"
                .into(),
        )
    } else {
        None
    };
    if !a.kfd || why.is_some() {
        if let Some(why) = why {
            hw.hidden.push(format!("{gpus} ({why})"));
        }
        hw.amd_gfx.clear();
    }
}

/// `NVRM version: NVIDIA UNIX Open Kernel Module for x86_64  595.58.03  Release Build ...`
pub fn parse_nvidia_proc(text: &str) -> Option<String> {
    let line = text.lines().find(|l| l.starts_with("NVRM version"))?;
    line.split_whitespace()
        .find(|w| {
            let mut parts = w.split('.');
            parts
                .next()
                .is_some_and(|p| p.len() >= 3 && p.chars().all(|c| c.is_ascii_digit()))
                && parts
                    .next()
                    .is_some_and(|p| p.chars().all(|c| c.is_ascii_digit()))
        })
        .map(str::to_string)
}

fn driver_ok(v: &str) -> bool {
    let mut it = v.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    let major = it.next().unwrap_or(0);
    let minor = it.next().unwrap_or(0);
    (major, minor) >= MIN_NVIDIA_DRIVER || (cfg!(windows) && major >= 580)
}

/// KFD topology: each GPU node's `properties` has `gfx_target_version 120001`
/// (major * 10000 + minor * 100 + stepping, minor/stepping printed in hex: gfx1201,
/// gfx90a). CPU nodes say 0.
pub fn amd_gfx_targets(nodes: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(nodes) else {
        return out;
    };
    let mut dirs: Vec<_> = rd.flatten().map(|e| e.path()).collect();
    dirs.sort();
    for d in dirs {
        let Ok(props) = std::fs::read_to_string(d.join("properties")) else {
            continue;
        };
        let v = props
            .lines()
            .find_map(|l| l.strip_prefix("gfx_target_version "))
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(0);
        if v > 0 {
            out.push(gfx_name(v));
        }
    }
    out
}

pub fn gfx_name(v: u32) -> String {
    format!("gfx{}{:x}{:x}", v / 10000, (v / 100) % 100, v % 100)
}

/// The pack variant for this machine and target triple.
pub fn choose(hw: &Hardware, target: &str) -> Choice {
    nvidia_choice(hw, target)
        .or_else(|| amd_choice(hw, target))
        .unwrap_or_else(|| Choice {
            variant: "cpu",
            reason: if hw.hidden.is_empty() {
                "no supported GPU found".into()
            } else {
                format!("no GPU visible: {}", hw.hidden.join("; "))
            },
            hint: None,
        })
}

/// The pack variant for the owner's preference `pref` (`ocr.backend`: `auto`, `cpu`,
/// `cuda`, `rocm`; anything else counts as `auto`) on this machine. A GPU preference
/// whose GPU is not usable here falls back to `cpu` with a hint, as 0.5.2 fell back
/// to CPU torch: a GPU pack would only run on the CPU, after a far larger download.
pub fn preferred(pref: &str, hw: &Hardware, target: &str) -> Choice {
    let pref = pref.trim().to_ascii_lowercase();
    let gpu = |vendor: &str, want: &str, found: Option<Choice>| match found {
        Some(c) if c.variant == want => Choice {
            reason: format!("ocr.backend: {pref}; {}", c.reason),
            ..c
        },
        other => {
            let (why, hint) = match other {
                Some(c) => (c.reason, c.hint),
                None if hw.hidden.is_empty() => ("none found".to_string(), None),
                None => (hw.hidden.join("; "), None),
            };
            Choice {
                variant: "cpu",
                reason: format!("ocr.backend: {pref}, but no usable {vendor} GPU here ({why})"),
                hint: Some(hint.unwrap_or_else(|| format!(
                    "OCR runs on the CPU until an {vendor} GPU is visible; then run install-ocr again (Docker: restart the container with the GPU passed in)."
                ))),
            }
        }
    };
    match pref.as_str() {
        "cpu" => Choice {
            variant: "cpu",
            reason: "ocr.backend: cpu".into(),
            hint: None,
        },
        "cuda" => gpu("NVIDIA", "cu130", nvidia_choice(hw, target)),
        "rocm" | "hip" => gpu("AMD", "rocm7.1", amd_choice(hw, target)),
        _ => choose(hw, target),
    }
}

/// NVIDIA's part of [`choose`]: `cu130`, or `cpu` for a driver too old for CUDA 13;
/// None without an NVIDIA driver (or on a target without cu130 packs).
fn nvidia_choice(hw: &Hardware, target: &str) -> Option<Choice> {
    let linux_x64 = target.starts_with("x86_64") && target.contains("-linux-");
    let windows_x64 = target.starts_with("x86_64") && target.contains("windows");
    if (linux_x64 || windows_x64)
        && let Some(driver) = &hw.nvidia_driver
    {
        let gpus = if hw.nvidia_gpus.is_empty() {
            "NVIDIA GPU".to_string()
        } else {
            hw.nvidia_gpus.join(", ")
        };
        if driver.is_empty() || driver_ok(driver) {
            return Some(Choice {
                variant: "cu130",
                reason: format!(
                    "{gpus}{}",
                    if driver.is_empty() {
                        String::new()
                    } else {
                        format!(" (driver {driver})")
                    }
                ),
                hint: None,
            });
        }
        return Some(Choice {
            variant: "cpu",
            reason: format!("{gpus} with driver {driver}: too old for CUDA 13"),
            hint: Some(format!(
                "Update the NVIDIA driver to {}.{} or newer to OCR on the GPU, then run install-ocr again.",
                MIN_NVIDIA_DRIVER.0, MIN_NVIDIA_DRIVER.1
            )),
        });
    }
    None
}

/// AMD's part of [`choose`]: `rocm7.1` for a supported GPU (Linux x86_64), `cpu` for
/// an unsupported one; None without an AMD GPU.
fn amd_choice(hw: &Hardware, target: &str) -> Option<Choice> {
    let linux_x64 = target.starts_with("x86_64") && target.contains("-linux-");
    if linux_x64 && !hw.amd_gfx.is_empty() {
        let usable: Vec<&String> = hw
            .amd_gfx
            .iter()
            .filter(|g| ROCM_ARCHS.contains(&g.as_str()))
            .collect();
        let overridable: Vec<&String> = hw
            .amd_gfx
            .iter()
            .filter(|g| ROCM_OVERRIDE_1030.contains(&g.as_str()))
            .collect();
        if !usable.is_empty() {
            return Some(Choice {
                variant: "rocm7.1",
                reason: format!("AMD {}", hw.amd_gfx.join(", ")),
                hint: None,
            });
        }
        if !overridable.is_empty() {
            let set = std::env::var("HSA_OVERRIDE_GFX_VERSION").ok();
            return Some(Choice {
                variant: "rocm7.1",
                reason: format!("AMD {}", hw.amd_gfx.join(", ")),
                hint: rdna2_hint(set.as_deref()),
            });
        }
        return Some(Choice {
            variant: "cpu",
            reason: format!(
                "AMD {} is not one of the supported ROCm GPUs ({})",
                hw.amd_gfx.join(", "),
                ROCM_ARCHS.join(", ")
            ),
            hint: None,
        });
    }
    None
}

/// What an RDNA2 card that runs the gfx1030 kernels (RX 6600: gfx1032) needs: nothing
/// from the user, since the backend sets `HSA_OVERRIDE_GFX_VERSION=10.3.0` when the
/// variable is unset; a value the user set is kept, so a different one is a warning.
pub fn rdna2_hint(set: Option<&str>) -> Option<String> {
    match set.map(str::trim).filter(|v| !v.is_empty()) {
        None => Some(
            "This RDNA2 GPU runs the gfx1030 kernels: the OCR backend sets \
             HSA_OVERRIDE_GFX_VERSION=10.3.0 for itself when it loads, nothing to configure."
                .to_string(),
        ),
        Some("10.3.0") => None,
        Some(v) => Some(format!(
            "This RDNA2 GPU runs the gfx1030 kernels, but HSA_OVERRIDE_GFX_VERSION={v} is set \
             here and the backend keeps a value you set: unset it (the backend then sets \
             10.3.0 itself) or set it to 10.3.0."
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nvidia_proc_versions() {
        let t = "NVRM version: NVIDIA UNIX Open Kernel Module for x86_64  595.58.03  Release Build  (dvs-builder@U22-I3-AE18-23-3)  Thu Mar  5 2026\nGCC version:  gcc version 15.2.1";
        assert_eq!(parse_nvidia_proc(t).as_deref(), Some("595.58.03"));
        let t =
            "NVRM version: NVIDIA UNIX x86_64 Kernel Module  550.120  Fri Sep 13 10:10:01 UTC 2024";
        assert_eq!(parse_nvidia_proc(t).as_deref(), Some("550.120"));
        assert!(driver_ok("580.65.06") && driver_ok("595.58.03") && !driver_ok("575.64"));
    }

    #[test]
    fn gfx_names_and_topology() {
        assert_eq!(gfx_name(120001), "gfx1201");
        assert_eq!(gfx_name(100300), "gfx1030");
        assert_eq!(gfx_name(100302), "gfx1032");
        assert_eq!(gfx_name(90010), "gfx90a");
        let tmp = tempfile::tempdir().unwrap();
        for (n, v) in [("0", 0), ("1", 120001)] {
            let d = tmp.path().join(n);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(
                d.join("properties"),
                format!("cpu_cores_count 16\ngfx_target_version {v}\n"),
            )
            .unwrap();
        }
        assert_eq!(amd_gfx_targets(tmp.path()), vec!["gfx1201"]);
    }

    #[test]
    fn choices() {
        let linux = "x86_64-unknown-linux-gnu";
        let nv = Hardware {
            nvidia_driver: Some("595.58.03".into()),
            nvidia_gpus: vec!["NVIDIA GeForce RTX 4090".into()],
            ..Hardware::default()
        };
        assert_eq!(choose(&nv, linux).variant, "cu130");
        assert_eq!(choose(&nv, "aarch64-apple-darwin").variant, "cpu");
        let old = Hardware {
            nvidia_driver: Some("550.120".into()),
            ..nv.clone()
        };
        let c = choose(&old, linux);
        assert_eq!(c.variant, "cpu");
        assert!(c.hint.is_some());
        let amd = Hardware {
            amd_gfx: vec!["gfx1201".into()],
            ..Hardware::default()
        };
        assert_eq!(choose(&amd, linux).variant, "rocm7.1");
        assert_eq!(choose(&amd, "x86_64-pc-windows-msvc").variant, "cpu");
        let rx6600 = Hardware {
            amd_gfx: vec!["gfx1032".into()],
            ..Hardware::default()
        };
        assert_eq!(choose(&rx6600, linux).variant, "rocm7.1");
        let vega = Hardware {
            amd_gfx: vec!["gfx906".into()],
            ..Hardware::default()
        };
        assert_eq!(choose(&vega, linux).variant, "cpu");
        assert_eq!(choose(&Hardware::default(), linux).variant, "cpu");
    }

    #[test]
    fn preference_picks_the_pack() {
        let linux = "x86_64-unknown-linux-gnu";
        let amd = Hardware {
            amd_gfx: vec!["gfx1201".into()],
            ..Hardware::default()
        };
        let nv = Hardware {
            nvidia_driver: Some("595.58.03".into()),
            nvidia_gpus: vec!["NVIDIA GeForce RTX 4090".into()],
            ..Hardware::default()
        };
        let both = Hardware {
            amd_gfx: amd.amd_gfx.clone(),
            ..nv.clone()
        };
        // auto (and the ONNX-era names) follow the hardware.
        for p in ["auto", "", "webgpu", "AUTO"] {
            assert_eq!(preferred(p, &amd, linux).variant, "rocm7.1", "{p}");
            assert_eq!(preferred(p, &nv, linux).variant, "cu130", "{p}");
        }
        // cpu stays on the CPU whatever the GPU.
        for hw in [&amd, &nv, &both] {
            let c = preferred("cpu", hw, linux);
            assert_eq!(c.variant, "cpu");
            assert_eq!(c.reason, "ocr.backend: cpu");
        }
        // A vendor preference picks that vendor, also when both are there.
        assert_eq!(preferred("rocm", &both, linux).variant, "rocm7.1");
        assert_eq!(preferred("hip", &both, linux).variant, "rocm7.1");
        assert_eq!(preferred("cuda", &both, linux).variant, "cu130");
        let c = preferred("rocm", &amd, linux);
        assert!(
            c.reason.starts_with("ocr.backend: rocm; AMD gfx1201"),
            "{}",
            c.reason
        );
        // ...and falls back to cpu, saying why, when that GPU is not usable here.
        let c = preferred("cuda", &amd, linux);
        assert_eq!(c.variant, "cpu");
        assert!(
            c.reason.contains("no usable NVIDIA GPU here (none found)"),
            "{}",
            c.reason
        );
        assert!(c.hint.unwrap().contains("until an NVIDIA GPU is visible"));
        let c = preferred("rocm", &Hardware::default(), linux);
        assert_eq!(c.variant, "cpu");
        // An old NVIDIA driver keeps its own hint (update the driver).
        let old = Hardware {
            nvidia_driver: Some("550.120".into()),
            ..nv.clone()
        };
        let c = preferred("cuda", &old, linux);
        assert_eq!(c.variant, "cpu");
        assert!(c.hint.unwrap().starts_with("Update the NVIDIA driver"));
        // No ROCm packs off Linux x86_64.
        assert_eq!(
            preferred("rocm", &amd, "x86_64-pc-windows-msvc").variant,
            "cpu"
        );
    }

    #[test]
    fn containers_see_only_the_gpus_passed_in() {
        let linux = "x86_64-unknown-linux-gnu";
        let nv = Hardware {
            nvidia_driver: Some("595.58.03".into()),
            ..Hardware::default()
        };
        let amd = Hardware {
            amd_gfx: vec!["gfx1201".into()],
            ..Hardware::default()
        };
        let with = |hw: &Hardware, a: Access| {
            let mut hw = hw.clone();
            apply_access(&mut hw, &a);
            hw
        };
        let passed_in = Access {
            in_container: true,
            nvidia_devices: true,
            libcuda: true,
            kfd: true,
            render_node: true,
            amd_permitted: true,
            amd_topology_blocked: false,
        };
        // --gpus all: the toolkit added the device nodes and libcuda.
        assert_eq!(with(&nv, passed_in.clone()), nv);
        // No --gpus: /proc/driver/nvidia shows the host's driver, nothing else is here.
        let hw = with(
            &nv,
            Access {
                nvidia_devices: false,
                libcuda: false,
                ..passed_in.clone()
            },
        );
        assert!(hw.nvidia_driver.is_none());
        assert!(
            hw.hidden[0].contains("this container has no access to the GPU"),
            "{:?}",
            hw.hidden
        );
        assert!(hw.hidden[0].contains("--gpus all"));
        assert_eq!(choose(&hw, linux).variant, "cpu");
        // Device nodes but no driver library (capabilities without `compute`).
        let hw = with(
            &nv,
            Access {
                libcuda: false,
                ..passed_in.clone()
            },
        );
        assert!(hw.nvidia_driver.is_none() && hw.hidden[0].contains("no libcuda.so.1"));
        // Outside a container the driver is enough (as before).
        let host = Access {
            in_container: false,
            ..Access::default()
        };
        assert_eq!(with(&nv, host.clone()).nvidia_driver, nv.nvidia_driver);

        // AMD: --device /dev/kfd --device /dev/dri, and permission to open them.
        assert_eq!(with(&amd, passed_in.clone()), amd);
        assert_eq!(
            choose(&with(&amd, passed_in.clone()), linux).variant,
            "rocm7.1"
        );
        for (a, says) in [
            (
                Access {
                    kfd: false,
                    ..passed_in.clone()
                },
                "no /dev/kfd",
            ),
            (
                Access {
                    render_node: false,
                    ..passed_in.clone()
                },
                "no /dev/dri render node",
            ),
            (
                Access {
                    amd_permitted: false,
                    ..passed_in.clone()
                },
                "may not open /dev/kfd",
            ),
        ] {
            let hw = with(&amd, a);
            assert!(hw.amd_gfx.is_empty());
            assert!(hw.hidden[0].contains(says), "{:?}", hw.hidden);
            let c = choose(&hw, linux);
            assert_eq!(c.variant, "cpu");
            assert!(c.reason.contains(says), "{}", c.reason);
        }
        // Docker without --device /dev/kfd refuses even the topology read: the GPU is
        // not named, but the reason still says what to pass.
        let hw = with(
            &Hardware::default(),
            Access {
                kfd: false,
                render_node: false,
                amd_topology_blocked: true,
                ..passed_in.clone()
            },
        );
        let c = choose(&hw, linux);
        assert_eq!(c.variant, "cpu");
        assert!(c.reason.contains("--device /dev/kfd"), "{}", c.reason);
        // Outside a container: no /dev/kfd means no ROCm driver, nothing to report;
        // the permission is the user's business (install-ocr still picks rocm7.1).
        let hw = with(&amd, host.clone());
        assert!(hw.amd_gfx.is_empty() && hw.hidden.is_empty());
        let hw = with(
            &amd,
            Access {
                kfd: true,
                render_node: true,
                ..host
            },
        );
        assert_eq!(hw, amd);
    }

    #[test]
    fn finds_libraries_like_the_loader() {
        let ldconfig = "\tlibcuda.so.1 (libc6,x86-64) => /usr/lib/x86_64-linux-gnu/libcuda.so.1\n";
        assert!(library_in(ldconfig, "libcuda.so.1", |_| false));
        assert!(!library_in(ldconfig, "libcuda.so", |_| false));
        assert!(library_in("", "libcuda.so.1", |p| p
            == Path::new("/usr/local/nvidia/lib64/libcuda.so.1")));
        assert!(!library_in("", "libcuda.so.1", |_| false));
    }

    #[test]
    fn rdna2_override_is_automatic() {
        let h = rdna2_hint(None).unwrap();
        assert!(
            h.contains("sets HSA_OVERRIDE_GFX_VERSION=10.3.0 for itself"),
            "{h}"
        );
        assert!(!h.contains("set HSA_OVERRIDE_GFX_VERSION=10.3.0 in the environment"));
        assert_eq!(rdna2_hint(Some("")), rdna2_hint(None));
        assert_eq!(rdna2_hint(Some("10.3.0")), None);
        let h = rdna2_hint(Some("11.0.0")).unwrap();
        assert!(h.contains("HSA_OVERRIDE_GFX_VERSION=11.0.0 is set"), "{h}");
    }

    #[test]
    fn visibility_variables_hide_gpus() {
        let linux = "x86_64-unknown-linux-gnu";
        let both = Hardware {
            nvidia_driver: Some("595.58.03".into()),
            nvidia_gpus: vec!["NVIDIA GeForce RTX 4090".into()],
            amd_gfx: vec!["gfx1201".into()],
            ..Hardware::default()
        };
        let amd = Hardware {
            amd_gfx: vec!["gfx1201".into()],
            ..Hardware::default()
        };
        let with = |hw: &Hardware, env: &[(&str, &str)]| {
            let mut hw = hw.clone();
            apply_visibility(&mut hw, |n| {
                env.iter()
                    .find(|(k, _)| *k == n)
                    .map(|(_, v)| v.to_string())
            });
            hw
        };
        assert!(hides_all("") && hides_all(" ") && hides_all("-1") && hides_all("-1,0"));
        assert!(!hides_all("0") && !hides_all("1,0") && !hides_all("GPU-8f2c"));

        // Unset or a device list: unchanged.
        assert_eq!(with(&amd, &[]), amd);
        assert_eq!(with(&amd, &[("HIP_VISIBLE_DEVICES", "0")]), amd);
        // Each AMD variable, empty or -1: no AMD GPU, cpu, and the reason says why.
        for var in [
            "HIP_VISIBLE_DEVICES",
            "ROCR_VISIBLE_DEVICES",
            "CUDA_VISIBLE_DEVICES",
        ] {
            for v in ["", "-1"] {
                let hw = with(&amd, &[(var, v)]);
                assert!(hw.amd_gfx.is_empty(), "{var}={v:?}");
                let c = choose(&hw, linux);
                assert_eq!(c.variant, "cpu", "{var}={v:?}");
                assert!(
                    c.reason.contains(var) && c.reason.contains("gfx1201"),
                    "{}",
                    c.reason
                );
            }
        }
        // HIP/ROCR hide only AMD; CUDA hides NVIDIA (and AMD, as HIP reads it too).
        let hw = with(&both, &[("HIP_VISIBLE_DEVICES", "")]);
        assert!(hw.amd_gfx.is_empty() && hw.nvidia_driver.is_some());
        assert_eq!(choose(&hw, linux).variant, "cu130");
        // HIP's own variable wins over CUDA's.
        let hw = with(
            &amd,
            &[("HIP_VISIBLE_DEVICES", "0"), ("CUDA_VISIBLE_DEVICES", "")],
        );
        assert_eq!(hw, amd);
        let hw = with(&both, &[("CUDA_VISIBLE_DEVICES", "-1")]);
        assert!(hw.amd_gfx.is_empty() && hw.nvidia_driver.is_none() && hw.nvidia_gpus.is_empty());
        assert_eq!(choose(&hw, linux).variant, "cpu");
        // The final-pass case: all three empty on an AMD host.
        let all = [
            ("HIP_VISIBLE_DEVICES", ""),
            ("ROCR_VISIBLE_DEVICES", ""),
            ("CUDA_VISIBLE_DEVICES", ""),
        ];
        assert_eq!(choose(&with(&amd, &all), linux).variant, "cpu");
    }
}
