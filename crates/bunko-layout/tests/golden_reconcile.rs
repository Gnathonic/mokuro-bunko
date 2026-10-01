//! Golden: `line_reconcile` (seeded fuzz + hand-made cases + every call the
//! 0.5.2 unit tests make) and `difflib.SequenceMatcher` property cases.

mod common;

use bunko_layout::difflib::SequenceMatcher;
use bunko_layout::json::{Separators, Value};
use bunko_layout::reconcile::*;
use common::*;

fn opt_confs(v: &Value) -> Option<Vec<f64>> {
    match v {
        Value::Null => None,
        x => Some(arr(x).iter().map(f).collect()),
    }
}

/// An int argument, or None when Python passed something else (a float).
fn int_arg(v: &Value) -> Option<i64> {
    match v {
        Value::Int(x) => Some(*x),
        Value::Bool(x) => Some(i64::from(*x)),
        _ => None,
    }
}

#[test]
fn seeded_reconcile_cases_match_python() {
    let cases = load("cases/reconcile_fuzz.json.gz");
    let mut errs = Vec::new();
    let mut n = 0;
    for c in arr(&cases) {
        let vlm = s(get(c, "vlm"));
        let ctc = s(get(c, "ctc"));
        let cells = i(get(c, "cells"));
        let thin = b(get(c, "thin"));
        let conf = opt_f(get(c, "ctc_conf"));
        let confs = opt_confs(get(c, "ctc_char_confs"));
        let r = reconcile_line(vlm, ctc, cells, thin, conf, confs.as_deref());
        if let Err(e) = check_reconciled(&r, get(c, "result")) {
            errs.push(format!(
                "reconcile_line({vlm:?}, {ctc:?}, {cells}, thin={thin}, conf={conf:?}):\n{e}"
            ));
            continue;
        }
        if needs_second_read(&r) != b(get(c, "needs_second")) {
            errs.push(format!("needs_second_read({r:?})"));
        }
        if let Some(second) = c.get("second") {
            let settled = settle_disputes(reconciled_from(get(c, "result")), s(second), cells);
            if let Err(e) = check_reconciled(&settled, get(c, "settled")) {
                errs.push(format!(
                    "settle_disputes(.., {:?}, {cells}):\n{e}",
                    s(second)
                ));
            }
        }
        if let Some(kw) = c.get("verdict_args") {
            let (keep, why) = engine_only_verdict(
                &r,
                i(get(kw, "cells")),
                f(get(kw, "det_score")),
                f(get(kw, "main")),
                f(get(kw, "thickness")),
                f(get(kw, "pitch")),
                i(get(kw, "neighbours")) as usize,
            );
            let want = arr(get(c, "verdict"));
            if keep != b(&want[0]) || why != s(&want[1]) {
                errs.push(format!(
                    "engine_only_verdict {keep} {why} != {}",
                    short(get(c, "verdict"))
                ));
            }
        }
        if corroborates(vlm, &r.text, cells) != b(get(c, "corroborates")) {
            errs.push(format!("corroborates({vlm:?}, {:?}, {cells})", r.text));
        }
        let o = arr(get(c, "overlap_repeat"));
        let got = overlap_repeat(s(&o[0]), s(&o[1]), i(&o[2]));
        if got as i64 != i(&o[3]) {
            errs.push(format!(
                "overlap_repeat({:?}, {:?}, {}) = {got}",
                s(&o[0]),
                s(&o[1]),
                i(&o[2])
            ));
        }
        n += 1;
    }
    assert!(n > 1000, "only {n} cases");
    assert!(
        errs.is_empty(),
        "{} of {n} differ:\n{}",
        errs.len(),
        errs.join("\n\n")
    );
}

#[test]
fn unit_test_reconcile_calls_match_python() {
    let cases = load("cases/unit_capture.json.gz");
    let mut errs = Vec::new();
    let mut counts = std::collections::BTreeMap::new();
    let mut skipped = 0;
    for c in arr(&cases) {
        let fname = s(get(c, "fn"));
        let a = get(c, "args");
        let want = get(c, "result");
        let num = |k: &str| f(get(a, k));
        let ok: Option<bool> = match fname {
            "reconcile_line" => {
                let Some(cells) = int_arg(get(a, "cells")) else {
                    skipped += 1;
                    continue;
                };
                let confs = opt_confs(get(a, "ctc_char_confs"));
                let r = reconcile_line(
                    s(get(a, "vlm")),
                    s(get(a, "ctc")),
                    cells,
                    b(get(a, "thin")),
                    opt_f(get(a, "ctc_conf")),
                    confs.as_deref(),
                );
                if let Err(e) = check_reconciled(&r, want) {
                    errs.push(format!("reconcile_line {}:\n{e}", short(a)));
                }
                None
            }
            "settle_disputes" => {
                let Some(cells) = int_arg(get(a, "cells")) else {
                    skipped += 1;
                    continue;
                };
                let r =
                    settle_disputes(reconciled_from(get(a, "line")), s(get(a, "second")), cells);
                if let Err(e) = check_reconciled(&r, want) {
                    errs.push(format!("settle_disputes {}:\n{e}", short(a)));
                }
                None
            }
            "engine_only_verdict" => {
                let (Some(cells), Some(nb)) =
                    (int_arg(get(a, "cells")), int_arg(get(a, "neighbours")))
                else {
                    skipped += 1;
                    continue;
                };
                let line = reconciled_from(get(a, "line"));
                let (keep, why) = engine_only_verdict(
                    &line,
                    cells,
                    num("det_score"),
                    num("main"),
                    num("thickness"),
                    num("pitch"),
                    nb as usize,
                );
                let w = arr(want);
                Some(keep == b(&w[0]) && why == s(&w[1]))
            }
            "corroborates" => {
                let Some(cells) = int_arg(get(a, "cells")) else {
                    skipped += 1;
                    continue;
                };
                Some(corroborates(s(get(a, "vlm")), s(get(a, "second")), cells) == b(want))
            }
            "needs_second_read" => {
                Some(needs_second_read(&reconciled_from(get(a, "line"))) == b(want))
            }
            "widen_punctuation" => {
                let keep: Option<Vec<bool>> = match get(a, "keep") {
                    Value::Null => None,
                    k => Some(arr(k).iter().map(b).collect()),
                };
                Some(widen_punctuation(s(get(a, "text")), keep.as_deref()) == s(want))
            }
            "repeated_tail" => Some(repeated_tail(s(get(a, "text"))) as i64 == i(want)),
            "engine_looped" => match (int_arg(get(a, "cells")), int_arg(get(a, "axis"))) {
                (Some(cells), Some(axis)) => {
                    Some(engine_looped(s(get(a, "vlm")), cells, axis) == b(want))
                }
                _ => {
                    skipped += 1;
                    continue;
                }
            },
            "is_runaway" => match int_arg(get(a, "cells")) {
                Some(cells) => {
                    Some(is_runaway(s(get(a, "text")), s(get(a, "ctc")), cells) == b(want))
                }
                None => {
                    skipped += 1;
                    continue;
                }
            },
            "line_cells" => {
                Some(line_cells(num("main"), num("thickness"), num("pitch")) == i(want))
            }
            "axis_cells" => {
                Some(axis_cells(num("main"), num("thickness"), num("pitch")) == i(want))
            }
            "region_cells" => {
                Some(region_cells(num("main"), num("thickness"), num("pitch")) == i(want))
            }
            "is_region" => Some(is_region(num("main"), num("thickness"), num("pitch")) == b(want)),
            "token_cap" => match int_arg(get(a, "cells")) {
                Some(cells) => Some(token_cap(cells) == i(want)),
                None => {
                    skipped += 1;
                    continue;
                }
            },
            "overlap_repeat" => Some(
                overlap_repeat(
                    s(get(a, "before")),
                    s(get(a, "after")),
                    i(get(a, "max_glyphs")),
                ) as i64
                    == i(want),
            ),
            "fold" => Some(fold(s(get(a, "text"))) == s(want)),
            "page_summary" => {
                let lines: Vec<Reconciled> =
                    arr(get(a, "lines")).iter().map(reconciled_from).collect();
                let got = page_summary(&lines).dumps(Separators::Compact);
                Some(got == want.dumps(Separators::Compact))
            }
            _ => continue,
        };
        if ok == Some(false) {
            errs.push(format!("{fname} {} != {}", short(a), short(want)));
        }
        *counts.entry(fname.to_string()).or_insert(0) += 1;
    }
    eprintln!("reconcile unit calls replayed: {counts:?}, skipped (non-int args): {skipped}");
    assert!(
        errs.is_empty(),
        "{} differ:\n{}",
        errs.len(),
        errs.join("\n\n")
    );
}

#[test]
fn sequence_matcher_matches_difflib() {
    let cases = load("cases/difflib.json.gz");
    let mut errs = Vec::new();
    for c in arr(&cases) {
        let a: Vec<char> = s(get(c, "a")).chars().collect();
        let bb: Vec<char> = s(get(c, "b")).chars().collect();
        let sm = SequenceMatcher::new(&a, &bb);
        let ops: Vec<Value> = sm
            .opcodes()
            .iter()
            .map(|o| {
                Value::Array(vec![
                    o.tag.as_str().into(),
                    Value::Int(o.i1 as i64),
                    Value::Int(o.i2 as i64),
                    Value::Int(o.j1 as i64),
                    Value::Int(o.j2 as i64),
                ])
            })
            .collect();
        let blocks: Vec<Value> = sm
            .matching_blocks()
            .iter()
            .map(|m| {
                Value::Array(vec![
                    Value::Int(m.a as i64),
                    Value::Int(m.b as i64),
                    Value::Int(m.size as i64),
                ])
            })
            .collect();
        if &Value::Array(ops) != get(c, "opcodes")
            || &Value::Array(blocks) != get(c, "blocks")
            || !same(sm.ratio(), f(get(c, "ratio")))
        {
            errs.push(format!("{:?} vs {:?}", s(get(c, "a")), s(get(c, "b"))));
        }
    }
    assert!(arr(&cases).len() >= 1000);
    assert!(
        errs.is_empty(),
        "{} differ: {}",
        errs.len(),
        errs.join("\n")
    );
}
