//! Golden: `line_layout` on real, fixture, jittered and synthetic pages, and
//! every layout call the 0.5.2 unit tests make. Byte-exact page JSON, bit-exact
//! floats (bodies, ruby spans).

mod common;

use bunko_layout::json::{Separators, Value};
use bunko_layout::layout::{PageLayout, column_pieces, layout_page};
use bunko_layout::records::RawPage;
use bunko_layout::script::{is_ruby_script, normalize_text};
use bunko_layout::sidecar::{MOKURO_FORMAT_VERSION, layout_page_dict};
use common::*;

fn check_layout(name: &str, got: &PageLayout, want: &Value) -> Vec<String> {
    let mut errs = Vec::new();
    let blocks: Vec<Value> = got.blocks.iter().map(|b| b.to_value()).collect();
    if &Value::Array(blocks.clone()) != get(want, "blocks") {
        errs.push(format!(
            "{name}: blocks differ\n got {}\nwant {}",
            Value::Array(blocks),
            get(want, "blocks")
        ));
    }
    let groups: Vec<Vec<i64>> = arr(get(want, "groups"))
        .iter()
        .map(|g| arr(g).iter().map(i).collect())
        .collect();
    let got_groups: Vec<Vec<i64>> = got
        .groups
        .iter()
        .map(|g| g.iter().map(|&x| x as i64).collect())
        .collect();
    if groups != got_groups {
        errs.push(format!("{name}: groups {got_groups:?} != {groups:?}"));
    }
    let kinds: Vec<&str> = arr(get(want, "kinds")).iter().map(s).collect();
    let got_kinds: Vec<&str> = got.kinds.iter().map(|k| k.as_str()).collect();
    if kinds != got_kinds {
        errs.push(format!("{name}: kinds {got_kinds:?} != {kinds:?}"));
    }
    let dropped: Vec<i64> = arr(get(want, "dropped")).iter().map(i).collect();
    if dropped != got.dropped.iter().map(|&x| x as i64).collect::<Vec<_>>() {
        errs.push(format!("{name}: dropped {:?} != {dropped:?}", got.dropped));
    }
    let ruby = arr(get(want, "ruby"));
    if ruby.len() != got.ruby.len() {
        errs.push(format!("{name}: {} ruby != {}", got.ruby.len(), ruby.len()));
    } else {
        for (g, w) in got.ruby.iter().zip(ruby) {
            let quad_ok = g
                .quad
                .iter()
                .zip(arr(get(w, "quad")))
                .all(|(p, q)| same(p.0, f(&arr(q)[0])) && same(p.1, f(&arr(q)[1])));
            let span = arr(get(w, "span"));
            let chars = arr(get(w, "chars"));
            if g.line as i64 != i(get(w, "line"))
                || g.base as i64 != i(get(w, "base"))
                || g.text != s(get(w, "text"))
                || !quad_ok
                || !same(g.span.0, f(&span[0]))
                || !same(g.span.1, f(&span[1]))
                || g.chars != (i(&chars[0]), i(&chars[1]))
            {
                errs.push(format!("{name}: ruby {g:?} != {}", short(w)));
            }
        }
    }
    let bodies = arr(get(want, "bodies"));
    if bodies.len() != got.bodies.len() {
        errs.push(format!(
            "{name}: {} bodies != {}",
            got.bodies.len(),
            bodies.len()
        ));
    } else {
        for (g, w) in got.bodies.iter().zip(bodies) {
            let members: Vec<i64> = arr(get(w, "members")).iter().map(i).collect();
            let ok = g.vertical == b(get(w, "vertical"))
                && same(g.theta, f(get(w, "theta")))
                && same(g.em, f(get(w, "em")))
                && same(g.top, f(get(w, "top")))
                && same(g.bottom, f(get(w, "bottom")))
                && same(g.cross0, f(get(w, "cross0")))
                && same(g.cross1, f(get(w, "cross1")))
                && same(g.gap, f(get(w, "gap")))
                && members == g.members.iter().map(|&x| x as i64).collect::<Vec<_>>();
            if !ok {
                errs.push(format!("{name}: body {g:?} != {}", short(w)));
            }
        }
    }
    errs
}

fn check_page(name: &str, page: &RawPage, expect: &Value) -> Vec<String> {
    let mut errs = check_layout(name, &layout_page(page), get(expect, "layout"));
    let pieces: Vec<Vec<i64>> = arr(get(expect, "column_pieces"))
        .iter()
        .map(|g| arr(g).iter().map(i).collect())
        .collect();
    let got: Vec<Vec<i64>> = column_pieces(&page.lines)
        .iter()
        .map(|g| g.iter().map(|&x| x as i64).collect())
        .collect();
    if pieces != got {
        errs.push(format!("{name}: column_pieces {got:?} != {pieces:?}"));
    }
    let (dict, _) = layout_page_dict(page, MOKURO_FORMAT_VERSION);
    let json = dict.to_value(None).dumps(Separators::Default);
    if json != s(get(expect, "page_json")) {
        errs.push(format!(
            "{name}: page json differs\n got {json}\nwant {}",
            s(get(expect, "page_json"))
        ));
    }
    errs
}

#[test]
fn layout_pages_match_python() {
    let cases = load("cases/layout_pages.json.gz");
    let reals = real_pages();
    let mut errs = Vec::new();
    let mut n = 0;
    for case in arr(&cases) {
        let name = s(get(case, "name"));
        let page = match case.get("ref").and_then(Value::as_str) {
            Some("real") => reals
                .iter()
                .find(|(n, _)| n == name)
                .expect("real page")
                .1
                .rounded(),
            Some("fixture") => RawPage::from_value(&fixture_page(name)),
            Some("page129") => RawPage::from_value(&load(&format!("inputs/{name}.detect.json"))),
            _ => RawPage::from_value(get(case, "page")),
        };
        errs.extend(check_page(name, &page, get(case, "expect")));
        n += 1;
    }
    assert!(n >= 100, "only {n} pages");
    assert!(
        errs.is_empty(),
        "{} of {n} pages differ:\n{}",
        errs.len(),
        errs.join("\n")
    );
}

#[test]
fn unit_test_layout_calls_match_python() {
    let cases = load("cases/unit_capture.json.gz");
    let mut errs = Vec::new();
    let mut counts = std::collections::BTreeMap::new();
    for case in arr(&cases) {
        let fname = s(get(case, "fn"));
        let args = get(case, "args");
        let result = get(case, "result");
        match fname {
            "layout_page" => {
                let page = RawPage::from_value(get(args, "page"));
                errs.extend(check_layout(
                    "unit layout_page",
                    &layout_page(&page),
                    result,
                ));
            }
            "column_pieces" => {
                let page = RawPage::from_value(get(args, "page"));
                let want: Vec<Vec<i64>> = arr(result)
                    .iter()
                    .map(|g| arr(g).iter().map(i).collect())
                    .collect();
                let got: Vec<Vec<i64>> = column_pieces(&page.lines)
                    .iter()
                    .map(|g| g.iter().map(|&x| x as i64).collect())
                    .collect();
                if want != got {
                    errs.push(format!(
                        "column_pieces {got:?} != {want:?} for {}",
                        short(args)
                    ));
                }
            }
            "normalize_text" => {
                let got = normalize_text(s(get(args, "text")));
                if got != s(result) {
                    errs.push(format!(
                        "normalize_text({:?}) = {got:?} != {:?}",
                        s(get(args, "text")),
                        s(result)
                    ));
                }
            }
            "is_ruby_script" => {
                let got = is_ruby_script(s(get(args, "text")));
                if got != b(result) {
                    errs.push(format!(
                        "is_ruby_script({:?}) = {got}",
                        s(get(args, "text"))
                    ));
                }
            }
            _ => continue,
        }
        *counts.entry(fname.to_string()).or_insert(0) += 1;
    }
    eprintln!("layout unit calls replayed: {counts:?}");
    assert!(
        errs.is_empty(),
        "{} differ:\n{}",
        errs.len(),
        errs.join("\n")
    );
}
