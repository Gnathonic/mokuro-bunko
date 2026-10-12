//! Host-side numerics against fixtures made by the Python 0.5.2 code
//! (`tests/golden/make_golden.py`): crops (OpenCV 5.0), Pillow resizes, NaFlex and
//! `smart_resize` sizes, and both detokenizers.

use std::path::{Path, PathBuf};

use bunko_vlm::crop::{LINE_MARGIN_EM, SECOND_MARGIN_EM, hayai_line_crops, paddle_quad_crop};
use bunko_vlm::detok::Detokenizer;
use bunko_vlm::hayai::size_for_budget;
use bunko_vlm::paddle::smart_resize;
use bunko_vlm::resample::{Filter, resize};
use bunko_vlm::{Quad, Rgb};
use serde_json::Value;

fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

fn png(name: &str) -> Rgb {
    let img = image::open(dir().join(name))
        .unwrap_or_else(|e| panic!("{name}: {e}"))
        .to_rgb8();
    Rgb::from_raw(img.width() as usize, img.height() as usize, img.into_raw()).unwrap()
}

fn golden() -> Value {
    serde_json::from_str(&std::fs::read_to_string(dir().join("golden.json")).unwrap()).unwrap()
}

/// (values differing, max abs difference)
fn diff(a: &Rgb, b: &Rgb) -> (usize, u8) {
    assert_eq!((a.width, a.height), (b.width, b.height), "crop size");
    let mut n = 0;
    let mut worst = 0;
    for (x, y) in a.data.iter().zip(&b.data) {
        let d = x.abs_diff(*y);
        if d > 0 {
            n += 1;
            worst = worst.max(d);
        }
    }
    (n, worst)
}

fn quad_of(v: &Value) -> Quad {
    let q = v.as_array().unwrap();
    std::array::from_fn(|i| [q[i][0].as_f64().unwrap(), q[i][1].as_f64().unwrap()])
}

#[test]
fn crops_match_opencv() {
    let g = golden();
    let page = png("page.png").to_bgr();
    let (mut total, mut differ, mut worst) = (0usize, 0usize, 0u8);
    for (k, q) in g["quads"].as_array().unwrap().iter().enumerate() {
        let quad = quad_of(&q["quad"]);
        let vertical = q["vertical"].as_bool().unwrap();
        let crops = hayai_line_crops(&page, &quad, vertical);
        assert_eq!(
            crops.len() as u64,
            q["chunks"].as_u64().unwrap(),
            "quad {k}: chunk count"
        );
        for (c, crop) in crops.iter().enumerate() {
            let (n, w) = diff(crop, &png(&format!("hayai_{k}_{c}.png")));
            total += crop.data.len();
            differ += n;
            worst = worst.max(w);
        }
        for (em, tag) in [(LINE_MARGIN_EM, 25), (SECOND_MARGIN_EM, 50)] {
            let crop = paddle_quad_crop(&page, &quad, em);
            let (n, w) = diff(&crop, &png(&format!("paddle_{k}_{tag}.png")));
            total += crop.data.len();
            differ += n;
            worst = worst.max(w);
        }
    }
    eprintln!("crops: {differ} of {total} channel values differ, max |diff| {worst}");
    assert!(worst <= 1, "max diff {worst}");
    assert!(
        (differ as f64) < 0.002 * total as f64,
        "{differ} of {total} differ"
    );
}

#[test]
fn pillow_resize_is_bit_exact() {
    let g = golden();
    for r in g["resize"].as_array().unwrap() {
        let k = r["k"].as_u64().unwrap();
        let (w, h) = (
            r["out"][0].as_u64().unwrap() as usize,
            r["out"][1].as_u64().unwrap() as usize,
        );
        let src = png(&format!("rs_{k}_in.png"));
        for (f, name) in [(Filter::Bilinear, "bilinear"), (Filter::Bicubic, "bicubic")] {
            let got = resize(&src, w, h, f);
            let want = png(&format!("rs_{k}_{name}.png"));
            assert_eq!(diff(&got, &want), (0, 0), "case {k} {name}");
        }
    }
}

#[test]
fn naflex_and_smart_resize_sizes() {
    let g = golden();
    for c in g["naflex"].as_array().unwrap() {
        let (h, w) = (
            c["hw"][0].as_u64().unwrap() as usize,
            c["hw"][1].as_u64().unwrap() as usize,
        );
        let budget = c["budget"].as_u64().unwrap() as usize;
        let want = (
            c["out"][0].as_u64().unwrap() as usize,
            c["out"][1].as_u64().unwrap() as usize,
        );
        assert_eq!(
            size_for_budget(h, w, budget),
            want,
            "naflex {h}x{w} @ {budget}"
        );
    }
    for c in g["smart_resize"].as_array().unwrap() {
        let (h, w) = (
            c["hw"][0].as_u64().unwrap() as usize,
            c["hw"][1].as_u64().unwrap() as usize,
        );
        let got = smart_resize(h, w).ok();
        let want = c["out"].as_array().map(|o| {
            (
                o[0].as_u64().unwrap() as usize,
                o[1].as_u64().unwrap() as usize,
            )
        });
        assert_eq!(got, want, "smart_resize {h}x{w}");
    }
}

fn hf_snapshot(repo: &str, rev: &str) -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join(".cache/huggingface/hub")
        .join(format!("models--{}", repo.replace('/', "--")))
        .join("snapshots")
        .join(rev)
}

#[test]
fn detokenizers_match_tokenizers_lib() {
    let g = golden();
    for (key, repo, rev) in [
        (
            "detok_hayai",
            bunko_vlm::hayai::REPO,
            bunko_vlm::hayai::REVISION,
        ),
        (
            "detok_paddle",
            bunko_vlm::paddle::BASE_REPO,
            bunko_vlm::paddle::BASE_REVISION,
        ),
    ] {
        let path = hf_snapshot(repo, rev).join("tokenizer.json");
        if !path.exists() {
            eprintln!("skipping {key}: {} not present", path.display());
            continue;
        }
        let d = Detokenizer::from_file(&path).unwrap();
        for c in g[key].as_array().unwrap() {
            let ids: Vec<u32> = c["ids"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_u64().unwrap() as u32)
                .collect();
            assert_eq!(d.decode(&ids), c["text"].as_str().unwrap(), "{key} {ids:?}");
        }
        if key == "detok_paddle" {
            assert_eq!(d.special_count(), 22);
        }
    }
}
