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
        if !Path::new("/dev/kfd").exists() {
            hw.amd_gfx.clear();
        }
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
    apply_visibility(&mut hw, |n| std::env::var(n).ok());
    hw
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
            return Choice {
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
            };
        }
        return Choice {
            variant: "cpu",
            reason: format!("{gpus} with driver {driver}: too old for CUDA 13"),
            hint: Some(format!(
                "Update the NVIDIA driver to {}.{} or newer to OCR on the GPU, then run install-ocr again.",
                MIN_NVIDIA_DRIVER.0, MIN_NVIDIA_DRIVER.1
            )),
        };
    }
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
            return Choice {
                variant: "rocm7.1",
                reason: format!("AMD {}", hw.amd_gfx.join(", ")),
                hint: None,
            };
        }
        if !overridable.is_empty() {
            let set = std::env::var("HSA_OVERRIDE_GFX_VERSION").ok();
            return Choice {
                variant: "rocm7.1",
                reason: format!("AMD {}", hw.amd_gfx.join(", ")),
                hint: rdna2_hint(set.as_deref()),
            };
        }
        return Choice {
            variant: "cpu",
            reason: format!(
                "AMD {} is not one of the supported ROCm GPUs ({})",
                hw.amd_gfx.join(", "),
                ROCM_ARCHS.join(", ")
            ),
            hint: None,
        };
    }
    Choice {
        variant: "cpu",
        reason: if hw.hidden.is_empty() {
            "no supported GPU found".into()
        } else {
            format!("no GPU visible: {}", hw.hidden.join("; "))
        },
        hint: None,
    }
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
