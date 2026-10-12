#!/usr/bin/env python3
"""Append the crates only libbunko_android.so uses to xtask's THIRD-PARTY-LICENSES.md.

`xtask licenses` reports the dependency graph of the `mokuro-bunko` binary (lite flavor,
here for the Android target). The Android library shares that graph minus the CLI bits
and adds its JNI glue (jni, jni-sys, cesu8, combine, ...). This script walks
`cargo metadata` for `bunko-android`, finds the crates missing from the report's table,
and appends a table plus their licence files. Copyleft-only crates fail the build, as in
xtask.

usage: android_licenses.py <workspace root> <target triple> <THIRD-PARTY-LICENSES.md>
"""

import json
import re
import subprocess
import sys
from pathlib import Path

COPYLEFT = re.compile(r"\b(A?GPL|LGPL|SSPL|EUPL|OSL|CDDL)", re.I)


def copyleft_only(expr: str) -> bool:
    alternatives = re.split(r"\s+OR\s+|/", expr)
    return bool(alternatives) and all(COPYLEFT.search(a) for a in alternatives)


def licence_files(directory: Path) -> list[Path]:
    names = ("LICENSE", "LICENCE", "COPYING", "UNLICENSE", "NOTICE")
    return sorted(p for p in directory.iterdir() if p.is_file() and p.name.upper().startswith(names))


def main() -> int:
    root, target, report = Path(sys.argv[1]), sys.argv[2], Path(sys.argv[3])
    text = report.read_text(encoding="utf-8")
    listed = set(re.findall(r"^\| ([^|\s]+) \| ([^|\s]+) \|", text, re.M))

    meta = json.loads(
        subprocess.run(
            ["cargo", "metadata", "--format-version", "1", "--filter-platform", target],
            cwd=root, check=True, capture_output=True, text=True,
        ).stdout
    )
    packages = {p["id"]: p for p in meta["packages"]}
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    start = next(i for i, p in packages.items() if p["name"] == "bunko-android" and p["source"] is None)

    seen, stack = set(), [start]
    while stack:
        pid = stack.pop()
        if pid in seen:
            continue
        seen.add(pid)
        for dep in nodes[pid]["deps"]:
            if any(k["kind"] is None for k in dep["dep_kinds"]):
                stack.append(dep["pkg"])

    extra = sorted(
        (packages[i] for i in seen
         if packages[i]["source"] is not None and (packages[i]["name"], packages[i]["version"]) not in listed),
        key=lambda p: (p["name"], p["version"]),
    )
    bad = [p for p in extra if copyleft_only(p.get("license") or "")]
    if bad:
        for p in bad:
            print(f"error: copyleft licence: {p['name']} {p['version']} ({p['license']})", file=sys.stderr)
        return 1

    out = ["", "## Rust crates used only by the Android library", "",
           "| Crate | Version | Licence |", "|---|---|---|"]
    out += [f"| {p['name']} | {p['version']} | {p.get('license') or '(see licence file)'} |" for p in extra]
    for p in extra:
        directory = Path(p["manifest_path"]).parent
        files = licence_files(directory)
        if not files:
            print(f"warning: no licence file: {p['name']} {p['version']}", file=sys.stderr)
            out += ["", f"### {p['name']} {p['version']}", "",
                    f"The crate ships no licence file. It is licensed {p.get('license')}; "
                    "the standard texts of those licences appear above."]
        for f in files:
            out += ["", f"### {p['name']} {p['version']} ({f.name})", "", "```text",
                    f.read_text(encoding="utf-8", errors="replace").rstrip(), "```"]
    report.write_text(text.rstrip() + "\n" + "\n".join(out) + "\n", encoding="utf-8")
    print(f"{len(extra)} Android-only crates added: " + ", ".join(f"{p['name']} {p['version']}" for p in extra))
    return 0


if __name__ == "__main__":
    sys.exit(main())
