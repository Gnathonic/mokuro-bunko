//! Parity of the reimplemented OpenCV primitives against cv2 5.0 outputs
//! (`tests/golden/cv_golden.json`, made by `tests/golden/gen_cv_golden.py`).

use base64::Engine;
use serde_json::Value;

use super::contours::find_contours;
use super::detect::{ProbMap, db_postprocess};
use super::fillpoly::fill_poly;
use super::geometry::{Quad, order_quad, rect_to_quad};
use super::minrect::{bounding_rect, min_area_rect};
use crate::image::{BgrImage, get_perspective_transform, resize_linear, warp_perspective_cubic};

fn fixture() -> Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/cv_golden.json");
    serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("json")
}

fn bytes(v: &Value) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(v.as_str().expect("b64"))
        .expect("b64")
}

fn n(v: &Value) -> usize {
    v.as_u64().expect("int") as usize
}

#[test]
fn resize_linear_is_bit_exact() {
    let f = fixture();
    for case in f["resize"].as_array().expect("cases") {
        let (w, h, dw, dh) = (n(&case["w"]), n(&case["h"]), n(&case["W"]), n(&case["H"]));
        let src = BgrImage::from_raw(w, h, bytes(&case["src"])).expect("src");
        let out = resize_linear(src.view(), dw, dh);
        assert_eq!(
            out.as_raw(),
            bytes(&case["dst"]).as_slice(),
            "resize {w}x{h} -> {dw}x{dh}"
        );
    }
}

#[test]
fn warp_perspective_cubic_matches() {
    let f = fixture();
    let warp = &f["warp"];
    let src = BgrImage::from_raw(n(&warp["w"]), n(&warp["h"]), bytes(&warp["src"])).expect("src");
    for case in warp["cases"].as_array().expect("cases") {
        let q: Vec<[f32; 2]> = case["quad"]
            .as_array()
            .expect("quad")
            .iter()
            .map(|p| {
                [
                    p[0].as_f64().unwrap_or(0.0) as f32,
                    p[1].as_f64().unwrap_or(0.0) as f32,
                ]
            })
            .collect();
        let quad: Quad = [q[0], q[1], q[2], q[3]];
        let (dw, dh) = (n(&case["W"]), n(&case["H"]));
        let target = [
            [0.0, 0.0],
            [dw as f32, 0.0],
            [dw as f32, dh as f32],
            [0.0, dh as f32],
        ];
        let m = get_perspective_transform(&quad, &target).expect("matrix");
        let want_m: Vec<f64> = case["m"]
            .as_array()
            .expect("m")
            .iter()
            .map(|v| v.as_f64().unwrap_or(0.0))
            .collect();
        for (k, row) in m.iter().enumerate() {
            for (j, v) in row.iter().enumerate() {
                let w = want_m[k * 3 + j];
                assert!(
                    (v - w).abs() <= 1e-12 * w.abs().max(1.0),
                    "matrix [{k}][{j}] {v} vs {w}"
                );
            }
        }
        let out = warp_perspective_cubic(src.view(), &m, dw, dh);
        let want = bytes(&case["dst"]);
        let diff = out
            .as_raw()
            .iter()
            .zip(&want)
            .filter(|(a, b)| a != b)
            .count();
        assert_eq!(
            diff,
            0,
            "warp {quad:?}: {diff} of {} bytes differ",
            want.len()
        );
        let crop = super::crop_line(src.view(), &quad);
        assert_eq!(crop.as_raw(), bytes(&case["crop_line"]).as_slice());
    }
}

/// Polygon vertices with collinear ones removed, as a sorted set.
fn corners(pts: &[[i32; 2]]) -> Vec<[i32; 2]> {
    let n = pts.len();
    if n <= 2 {
        let mut v = pts.to_vec();
        v.sort();
        v.dedup();
        return v;
    }
    let mut out: Vec<[i32; 2]> = Vec::new();
    for k in 0..n {
        let (a, b, c) = (pts[(k + n - 1) % n], pts[k], pts[(k + 1) % n]);
        let d1 = [b[0] - a[0], b[1] - a[1]];
        let d2 = [c[0] - b[0], c[1] - b[1]];
        let cross = d1[0] * d2[1] - d1[1] * d2[0];
        let dot = d1[0] * d2[0] + d1[1] * d2[1];
        if cross != 0 || dot < 0 {
            out.push(b);
        }
    }
    out.sort();
    out.dedup();
    out
}

#[test]
fn contours_fillpoly_minrect_match() {
    let f = fixture();
    let c = &f["contours"];
    let (w, h) = (n(&c["w"]), n(&c["h"]));
    let mask: Vec<bool> = bytes(&c["bitmap"]).iter().map(|&b| b != 0).collect();
    let ours = find_contours(&mask, w, h);
    let theirs = c["contours"].as_array().expect("contours");
    assert_eq!(ours.len(), theirs.len(), "contour count");
    let mut our_sets: Vec<Vec<[i32; 2]>> = ours.iter().map(|p| corners(p)).collect();
    for (k, t) in theirs.iter().enumerate() {
        let pts: Vec<[i32; 2]> = t["pts"]
            .as_array()
            .expect("pts")
            .iter()
            .map(|p| {
                [
                    p[0].as_i64().unwrap_or(0) as i32,
                    p[1].as_i64().unwrap_or(0) as i32,
                ]
            })
            .collect();
        // Same polygon found by our border following.
        let set = corners(&pts);
        let pos = our_sets.iter().position(|s| *s == set);
        assert!(pos.is_some(), "contour {k} {set:?} not found");
        if let Some(p) = pos {
            our_sets.remove(p);
        }
        // fillPoly on cv2's own points.
        let (x0, y0, bw, bh) = bounding_rect(&pts);
        let bb: Vec<i64> = t["bbox"]
            .as_array()
            .expect("bbox")
            .iter()
            .map(|v| v.as_i64().unwrap_or(0))
            .collect();
        assert_eq!([x0 as i64, y0 as i64, bw as i64, bh as i64].to_vec(), bb);
        let mut m = vec![0u8; (bw * bh) as usize];
        let shifted: Vec<[i32; 2]> = pts.iter().map(|p| [p[0] - x0, p[1] - y0]).collect();
        fill_poly(&mut m, bw as usize, bh as usize, &shifted);
        assert_eq!(m, bytes(&t["mask"]), "fillPoly mask of contour {k}");
        // minAreaRect: same rectangle (compare canonical corners).
        let r: Vec<f64> = t["rect"]
            .as_array()
            .expect("rect")
            .iter()
            .map(|v| v.as_f64().unwrap_or(0.0))
            .collect();
        let want = order_quad(&rect_to_quad(r[0], r[1], r[2], r[3], r[4]));
        let got = min_area_rect(&pts);
        let got = order_quad(&rect_to_quad(
            got.cx as f64,
            got.cy as f64,
            got.w as f64,
            got.h as f64,
            got.angle as f64,
        ));
        let area_w = r[2] * r[3];
        let area_g = {
            let g = min_area_rect(&pts);
            g.w as f64 * g.h as f64
        };
        assert!(
            (area_w - area_g).abs() < 1e-3 * area_w.max(1.0),
            "contour {k} area {area_g} vs {area_w}"
        );
        if r[2].min(r[3]) >= 3.0 {
            for (a, b) in want.iter().zip(&got) {
                assert!(
                    (a[0] - b[0]).abs() < 1e-3 && (a[1] - b[1]).abs() < 1e-3,
                    "contour {k}: {want:?} vs {got:?}"
                );
            }
        }
    }
}

#[test]
fn db_postprocess_matches_python() {
    let f = fixture();
    let d = &f["db"];
    let (w, h) = (n(&d["w"]), n(&d["h"]));
    let raw = bytes(&d["prob"]);
    let prob: Vec<f32> = raw
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();
    let ours = db_postprocess(&ProbMap {
        width: w,
        height: h,
        data: &prob,
    });
    let theirs = d["boxes"].as_array().expect("boxes");
    assert_eq!(ours.len(), theirs.len());
    for t in theirs {
        let q: Vec<[f64; 2]> = t["quad"]
            .as_array()
            .expect("q")
            .iter()
            .map(|p| [p[0].as_f64().unwrap_or(0.0), p[1].as_f64().unwrap_or(0.0)])
            .collect();
        let score = t["score"].as_f64().unwrap_or(0.0);
        let found = ours.iter().any(|(oq, os)| {
            (os - score).abs() < 1e-9
                && oq.iter().zip(&q).all(|(a, b)| {
                    (a[0] as f64 - b[0]).abs() < 1e-3 && (a[1] as f64 - b[1]).abs() < 1e-3
                })
        });
        assert!(found, "box {q:?} score {score} not reproduced");
    }
}
