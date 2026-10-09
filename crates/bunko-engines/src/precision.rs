//! Precision modes → the format a session runs in (spec ocr-generations-bench §3,
//! ocr-recognizers §7.1/§7.2).
//!
//! A "format" is a choice between pre-compiled graph files; nothing is re-cast at run
//! time. What a device supports is decided by the caller: with the libtorch backend, the
//! device's formats (GPUs fp32/fp16/bf16 as 0.5.2's torch probe said, the CPU fp32, plus
//! bf16 on AVX512_BF16/AMX hosts) that also have a compiled package here, of which the
//! auto modes only take bf16 where it is native ([`auto_formats`]); with the ONNX
//! recognizers ([`supported`]), fp32 and on a GPU fp16 (no bf16 ONNX export). An
//! unsupported format makes auto modes fall to their next candidate, and a forced one
//! refuses with the 0.5.2 [`PRECISION_REFUSAL`] marker, which makes the library give
//! the volume back unrecorded and back the row off on this machine.

use bunko_vlm::Precision;

/// The marker the library looks for (`engine_runner.PRECISION_REFUSAL`).
pub const PRECISION_REFUSAL: &str = "precision not available here";

pub const MODE_ACCURACY: &str = "auto-accuracy";
pub const MODE_BALANCED: &str = "auto-balanced";
pub const MODE_SPEED: &str = "auto-speed";
const FORCED: [&str; 3] = ["fp32", "bf16", "fp16"];

/// `normalize_precision_mode`: blank or legacy `auto` → `auto-accuracy`; lower-cased.
/// An unknown spelling also reads as the default (the library validates rows).
pub fn normalize_mode(mode: &str) -> String {
    let m = mode.trim().to_ascii_lowercase();
    match m.as_str() {
        "" | "auto" => MODE_ACCURACY.to_string(),
        MODE_ACCURACY | MODE_BALANCED | MODE_SPEED => m,
        f if FORCED.contains(&f) => m,
        _ => MODE_ACCURACY.to_string(),
    }
}

/// `PRECISION_POLICY`: candidate formats per engine and auto mode, preferred first.
/// Empty for engines whose precision is fixed (ppocr-manga).
pub fn candidates(engine: &str, mode: &str) -> &'static [&'static str] {
    match (engine, mode) {
        ("hayai-nova", MODE_ACCURACY | MODE_BALANCED) => &["bf16", "fp32"],
        ("hayai-nova", MODE_SPEED) => &["bf16", "fp16", "fp32"],
        ("paddle-manga", MODE_ACCURACY) => &["fp32"],
        ("paddle-manga", MODE_BALANCED) => &["bf16", "fp32"],
        ("paddle-manga", MODE_SPEED) => &["bf16", "fp16", "fp32"],
        _ => &[],
    }
}

/// Whether precision modes apply to the engine at all.
pub fn is_precision_engine(engine: &str) -> bool {
    matches!(engine, "hayai-nova" | "paddle-manga")
}

/// What a device runs on the ONNX recognizers: the CPU fp32 only, a GPU fp32 and fp16
/// (never bf16, see the module docs).
pub fn supported(gpu: bool) -> &'static [&'static str] {
    if gpu { &["fp32", "fp16"] } else { &["fp32"] }
}

/// THE bf16 rule, shared with the library (which judges what each machine runs with
/// it): [`bf16_native`] says where bf16 is native, [`auto_formats`] keeps bf16 for the
/// auto modes only there. One copy, in `bunko-sched`.
pub use bunko_sched::precision::{auto_formats, bf16_native};

/// A resolved precision and why (`[runner] <engine> precision: <fmt> (<why>)`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub precision: Precision,
    pub why: String,
}

fn as_precision(fmt: &str) -> Option<Precision> {
    match fmt {
        "fp32" => Some(Precision::Fp32),
        "bf16" => Some(Precision::Bf16),
        "fp16" => Some(Precision::Fp16),
        _ => None,
    }
}

/// `resolve_precision(engine, requested, supported, pick, pick_why)`. `Ok(None)` for
/// an engine with a fixed precision; `Err` is the refusal (it contains
/// [`PRECISION_REFUSAL`]).
pub fn resolve(
    engine: &str,
    requested: &str,
    supported: &[&str],
    pick: Option<&str>,
    pick_why: &str,
) -> Result<Option<Resolved>, String> {
    if !is_precision_engine(engine) {
        return Ok(None);
    }
    let mode = normalize_mode(requested);
    let refuse = |why: String| {
        format!(
            "{PRECISION_REFUSAL}: {engine} is asked for {mode}, and this device cannot run it ({why})"
        )
    };
    let finish = |fmt: &str, reason: &str| {
        let precision = as_precision(fmt).ok_or_else(|| refuse(format!("{fmt} not supported")))?;
        let why = if reason == mode {
            mode.clone()
        } else {
            format!("{mode}; {reason}")
        };
        Ok(Some(Resolved { precision, why }))
    };
    if FORCED.contains(&mode.as_str()) {
        if supported.contains(&mode.as_str()) && as_precision(&mode).is_some() {
            return finish(&mode, &mode);
        }
        return Err(refuse(format!("{mode} not supported")));
    }
    let usable: Vec<&str> = candidates(engine, &mode)
        .iter()
        .copied()
        .filter(|c| *c == "fp32" || supported.contains(c))
        .filter(|c| as_precision(c).is_some())
        .collect();
    let Some(first) = usable.first().copied() else {
        return Err(refuse("no candidate format runs here".into()));
    };
    if mode == MODE_ACCURACY || usable.len() == 1 {
        return finish(first, &mode);
    }
    if let Some(p) = pick.filter(|p| usable.contains(p)) {
        let why = if pick_why.is_empty() {
            "benchmark"
        } else {
            pick_why
        };
        return finish(p, why);
    }
    finish(first, "not benchmarked yet: first supported candidate")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(r: Result<Option<Resolved>, String>) -> (Precision, String) {
        let r = r.expect("resolved").expect("a precision engine");
        (r.precision, r.why)
    }

    #[test]
    fn auto_modes_skip_bf16() {
        let gpu = supported(true);
        let cpu = supported(false);
        assert_eq!(
            ok(resolve("hayai-nova", "auto-accuracy", gpu, None, "")),
            (Precision::Fp32, "auto-accuracy".into())
        );
        assert_eq!(
            ok(resolve("hayai-nova", "auto", cpu, None, "")),
            (Precision::Fp32, "auto-accuracy".into())
        );
        assert_eq!(
            ok(resolve("hayai-nova", "auto-speed", gpu, None, "")),
            (
                Precision::Fp16,
                "auto-speed; not benchmarked yet: first supported candidate".into()
            )
        );
        assert_eq!(
            ok(resolve(
                "paddle-manga",
                "auto-speed",
                gpu,
                Some("fp32"),
                "benchmark: fp32 3.00 p/s"
            )),
            (
                Precision::Fp32,
                "auto-speed; benchmark: fp32 3.00 p/s".into()
            )
        );
        // One usable candidate (a card with fp32 packages only): no benchmark needed.
        assert_eq!(
            ok(resolve("paddle-manga", "auto-speed", &["fp32"], None, "")),
            (Precision::Fp32, "auto-speed".into())
        );
        assert_eq!(resolve("ppocr-manga", "fp16", cpu, None, ""), Ok(None));
    }

    #[test]
    fn libtorch_formats_include_bf16() {
        let gpu = ["fp32", "fp16", "bf16"];
        // 0.5.2's policy: hayai-nova's default is bf16 where the device runs it.
        assert_eq!(
            ok(resolve("hayai-nova", "auto-accuracy", &gpu, None, "")),
            (Precision::Bf16, "auto-accuracy".into())
        );
        assert_eq!(
            ok(resolve("paddle-manga", "auto-accuracy", &gpu, None, "")),
            (Precision::Fp32, "auto-accuracy".into())
        );
        assert_eq!(
            ok(resolve("paddle-manga", "bf16", &gpu, None, "")),
            (Precision::Bf16, "bf16".into())
        );
        assert_eq!(
            ok(resolve(
                "hayai-nova",
                "auto-speed",
                &gpu,
                Some("fp16"),
                "benchmark"
            )),
            (Precision::Fp16, "auto-speed; benchmark".into())
        );
        // a CPU without bf16 packages / AVX512_BF16
        assert_eq!(
            ok(resolve("hayai-nova", "auto-accuracy", &["fp32"], None, "")),
            (Precision::Fp32, "auto-accuracy".into())
        );
        assert!(resolve("hayai-nova", "bf16", &["fp32"], None, "").is_err());
    }

    #[test]
    fn cpu_auto_modes_stay_fp32_like_0_5_2() {
        // An AVX512_BF16 host reports bf16 packages; auto modes still run fp32 there.
        let cpu = || vec!["fp32", "bf16"];
        for mode in ["auto-accuracy", "auto-balanced", "auto-speed", "", "auto"] {
            let formats = auto_formats(mode, cpu(), bf16_native("cpu", "x86_64"));
            assert_eq!(formats, ["fp32"], "{mode}");
            assert_eq!(
                ok(resolve(
                    "hayai-nova",
                    mode,
                    &formats,
                    Some("bf16"),
                    "benchmark"
                ))
                .0,
                Precision::Fp32,
                "{mode}"
            );
        }
        // Forcing bf16 still gets it.
        let formats = auto_formats("BF16", cpu(), false);
        assert_eq!(
            ok(resolve("hayai-nova", "bf16", &formats, None, "")),
            (Precision::Bf16, "bf16".into())
        );
    }

    #[test]
    fn auto_modes_pick_bf16_only_where_it_is_native() {
        assert!(bf16_native("cuda", "sm_80") && bf16_native("cuda", "sm_89"));
        assert!(bf16_native("cuda", "sm_120"));
        assert!(!bf16_native("cuda", "sm_75") && !bf16_native("cuda", ""));
        assert!(bf16_native("rocm", "gfx1100") && bf16_native("rocm", "gfx1201"));
        assert!(!bf16_native("rocm", "gfx1030") && !bf16_native("rocm", "gfx1032"));
        assert!(!bf16_native("cpu", "x86_64"));
        let gpu = || vec!["fp32", "bf16", "fp16"];
        // RDNA2: auto-accuracy lands on fp32, auto-speed on fp16 (not bf16)
        let rdna2 = |mode| auto_formats(mode, gpu(), bf16_native("rocm", "gfx1030"));
        assert_eq!(
            ok(resolve(
                "hayai-nova",
                "auto-accuracy",
                &rdna2("auto-accuracy"),
                None,
                ""
            ))
            .0,
            Precision::Fp32
        );
        assert_eq!(
            ok(resolve(
                "hayai-nova",
                "auto-speed",
                &rdna2("auto-speed"),
                None,
                ""
            ))
            .0,
            Precision::Fp16
        );
        assert_eq!(
            ok(resolve(
                "paddle-manga",
                "auto-balanced",
                &rdna2("auto-balanced"),
                None,
                ""
            ))
            .0,
            Precision::Fp32
        );
        // forced bf16 still runs there
        assert_eq!(
            ok(resolve("hayai-nova", "bf16", &rdna2("bf16"), None, "")).0,
            Precision::Bf16
        );
        // RDNA4 / Ampere+: bf16 as 0.5.2's policy says
        let rdna4 = auto_formats("auto-accuracy", gpu(), bf16_native("rocm", "gfx1201"));
        assert_eq!(
            ok(resolve("hayai-nova", "auto-accuracy", &rdna4, None, "")).0,
            Precision::Bf16
        );
    }

    #[test]
    fn forced_formats_refuse_when_unsupported() {
        let err = resolve("hayai-nova", "bf16", supported(true), None, "").unwrap_err();
        assert!(err.starts_with(PRECISION_REFUSAL), "{err}");
        assert!(err.contains("hayai-nova is asked for bf16"), "{err}");
        // a card with fp32 packages only
        let err = resolve("paddle-manga", "fp16", &["fp32"], None, "").unwrap_err();
        assert!(err.contains("(fp16 not supported)"), "{err}");
        assert_eq!(
            ok(resolve("paddle-manga", "FP16", supported(true), None, "")),
            (Precision::Fp16, "fp16".into())
        );
    }
}
