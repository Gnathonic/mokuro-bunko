"""Portability checks on built packages (run by ``build`` after every package).

* ABI: the native code in a package must not need a newer glibc / libstdc++ than
  libtorch itself does (libtorch 2.13 wheels: manylinux_2_28). A wrapper compiled by
  a rolling-release host toolchain needs GLIBC_2.38+ and fails to load in, e.g., a
  Debian 12/13 container; Linux packages are therefore built in the glibc-2.28
  container (container.py) and this check fails the build otherwise.
* ISA: x86-64-v3 targets must contain no AVX-512 (zmm) instructions (a Zen 5-built
  package SIGILLs on Zen 3).
"""

from __future__ import annotations

import re
import subprocess
import tempfile
import zipfile
from pathlib import Path

ABI_PREFIXES = ("GLIBC", "GLIBCXX", "CXXABI")


def _vkey(v: str) -> tuple[int, ...]:
    return tuple(int(x) for x in v.split("."))


def required_versions(so: Path) -> dict[str, str]:
    """Highest GLIBC_/GLIBCXX_/CXXABI_ version a shared object requires (dynamic symbols)."""
    out = subprocess.run(["objdump", "-T", str(so)], capture_output=True, text=True, check=True).stdout
    und: dict[str, str] = {}  # only symbols the object NEEDS (UND), not ones it defines
    for line in out.splitlines():
        if "*UND*" not in line:
            continue
        m = re.search(r"\b(GLIBC|GLIBCXX|CXXABI)_([0-9][0-9.]*)\b", line)
        if m and (m[1] not in und or _vkey(m[2]) > _vkey(und[m[1]])):
            und[m[1]] = m[2]
    return und


def libtorch_baseline() -> dict[str, str]:
    """What the running torch's own libraries require (the floor a package may not raise)."""
    import torch

    lib = Path(torch.__file__).parent / "lib"
    base: dict[str, str] = {}
    for name in ("libc10.so", "libtorch_cpu.so", "libtorch_cuda.so", "libtorch_hip.so"):
        if (lib / name).exists():
            for k, v in required_versions(lib / name).items():
                if k not in base or _vkey(v) > _vkey(base[k]):
                    base[k] = v
    return base


def _native_files(pt2: Path, d: str) -> list[Path]:
    with zipfile.ZipFile(pt2) as z:
        return [Path(z.extract(n, d)) for n in z.namelist() if n.endswith((".so", ".dll", ".dylib", ".pyd"))]


def abi_report(pt2: Path, baseline: dict[str, str]) -> tuple[dict[str, str], list[str]]:
    need: dict[str, str] = {}
    with tempfile.TemporaryDirectory() as d:
        for f in _native_files(pt2, d):
            if f.suffix != ".so":
                continue
            for k, v in required_versions(f).items():
                if k not in need or _vkey(v) > _vkey(need[k]):
                    need[k] = v
    bad = [f"{k}_{v} > {k}_{baseline.get(k, '?')}" for k, v in need.items()
           if k in baseline and _vkey(v) > _vkey(baseline[k])]
    return need, bad


def zmm_count(path: Path) -> dict[str, int]:
    files = sorted(path.rglob("*.pt2")) if path.is_dir() else [path]
    out = {}
    for f in files:
        with tempfile.TemporaryDirectory() as d:
            for p in _native_files(f, d):
                dis = subprocess.run(["objdump", "-d", "--no-show-raw-insn", str(p)], capture_output=True, text=True).stdout
                out[f"{f}:{p.name}"] = dis.count("%zmm")
    return out


PT_GNU_STACK = 0x6474E551
PF_X = 1


def _gnu_stack_flag_offsets(elf: bytes) -> list[int]:
    """Byte offsets of the p_flags field of every PT_GNU_STACK header that has PF_X."""
    import struct

    if elf[:4] != b"\x7fELF" or elf[4] != 2 or elf[5] != 1:  # ELF64 little-endian only
        return []
    e_phoff, = struct.unpack_from("<Q", elf, 0x20)
    e_phentsize, e_phnum = struct.unpack_from("<HH", elf, 0x36)
    out = []
    for i in range(e_phnum):
        off = e_phoff + i * e_phentsize
        p_type, p_flags = struct.unpack_from("<II", elf, off)
        if p_type == PT_GNU_STACK and p_flags & PF_X:
            out.append(off + 4)
    return out


def _central_entry(raw: bytearray, name: str) -> int:
    """Offset of the central-directory record of ``name`` (walks every record)."""
    import struct

    pos = 0
    want = name.encode()
    while True:
        pos = raw.find(b"PK\x01\x02", pos)
        if pos < 0:
            raise SystemExit(f"central directory entry of {name} not found")
        n, m, k = struct.unpack_from("<HHH", raw, pos + 28)
        if bytes(raw[pos + 46:pos + 46 + n]) == want:
            return pos
        pos += 46 + n + m + k


def clear_execstack(pt2: Path) -> int:
    """Drop PF_X from PT_GNU_STACK of every .so stored in a .pt2 (in place; entries are
    stored uncompressed, so the bytes and the two CRC fields are patched where they are).

    glibc >= 2.41 refuses to dlopen a library that asks for an executable stack; objects
    made by ``ld -r -b binary`` (embedded kernels/constants) without a .note.GNU-stack
    section make old binutils mark the whole wrapper RWE. Returns the number of fixes."""
    import struct
    import zlib

    fixed = 0
    with zipfile.ZipFile(pt2) as z:
        infos = [i for i in z.infolist() if i.filename.endswith(".so")]
        datas = {i.filename: z.read(i) for i in infos}
    with open(pt2, "r+b") as f:
        raw = bytearray(f.read())
        for i in infos:
            data = bytearray(datas[i.filename])
            offs = _gnu_stack_flag_offsets(bytes(data))
            if not offs:
                continue
            if i.compress_type != zipfile.ZIP_STORED:
                raise SystemExit(f"{pt2}:{i.filename} is compressed; cannot patch in place")
            for o in offs:
                flags, = struct.unpack_from("<I", data, o)
                struct.pack_into("<I", data, o, flags & ~PF_X)
            crc = zlib.crc32(data) & 0xFFFFFFFF
            h = i.header_offset
            n, m = struct.unpack_from("<HH", raw, h + 26)
            start = h + 30 + n + m
            raw[start:start + len(data)] = data
            struct.pack_into("<I", raw, h + 14, crc)
            cd = _central_entry(raw, i.filename)
            struct.pack_into("<I", raw, cd + 16, crc)
            fixed += len(offs)
        f.seek(0)
        f.write(raw)
    with zipfile.ZipFile(pt2) as z:  # CRCs verified on read
        bad = z.testzip()
        if bad:
            raise SystemExit(f"{pt2}: {bad} corrupt after execstack patch")
    return fixed


def execstack_problems(pt2: Path) -> list[str]:
    out = []
    with zipfile.ZipFile(pt2) as z:
        for i in z.infolist():
            if i.filename.endswith(".so") and _gnu_stack_flag_offsets(z.read(i)):
                out.append(f"{Path(i.filename).name}: PT_GNU_STACK is RWE (glibc >= 2.41 refuses it)")
    return out


def windows_extension(pt2: Path) -> int:
    """Cross-compiled Windows packages: the MinGW-built wrapper is a PE DLL that inductor
    names ``*.wrapper.so``; libtorch's Windows loader only takes ``*.wrapper.pyd`` (it
    would otherwise try to recompile the .cpp with MSVC at load). Renames in place."""
    with zipfile.ZipFile(pt2) as z:
        infos = z.infolist()
        if not any(i.filename.endswith(".wrapper.so") for i in infos):
            return 0
        entries = [(i, z.read(i)) for i in infos]
    tmp = pt2.with_suffix(".rename.tmp")
    n = 0
    with zipfile.ZipFile(tmp, "w", zipfile.ZIP_STORED) as out:
        for i, data in entries:
            name = i.filename
            if name.endswith(".so"):
                name = name[:-3] + ".pyd"
                n += 1
            zi = zipfile.ZipInfo(name, date_time=i.date_time)
            zi.compress_type = zipfile.ZIP_STORED
            zi.external_attr = i.external_attr
            out.writestr(zi, data)
    tmp.replace(pt2)
    return n


def normalize(pt2: Path) -> None:
    """Make a package byte-reproducible. Two builds of the same graph produce identical
    native code (wrapper .so/.pyd, kernel binaries, weight blobs) but differ in (a) the
    ``// Compile cmd`` / ``// Link cmd`` trailer inductor appends to the bundled .cpp
    sources when it compiled them (absent on a cache hit) and (b) the random
    ``.data/serialization_id``. Strip (a), derive (b) from the other members' bytes."""
    import hashlib

    with zipfile.ZipFile(pt2) as z:
        entries = [(i, z.read(i)) for i in z.infolist()]
    h = hashlib.sha256()
    fixed = []
    for i, data in entries:
        if i.filename.endswith(".cpp"):
            cut = data.find(b"\n// Compile cmd\n")
            if cut >= 0:
                data = data[:cut] + b"\n"
        if not i.filename.endswith("serialization_id"):
            h.update(i.filename.encode() + b"\0" + data)
        fixed.append((i, data))
    sid = str(int(h.hexdigest(), 16))[:40].encode()
    tmp = pt2.with_suffix(".norm.tmp")
    with zipfile.ZipFile(tmp, "w", zipfile.ZIP_STORED) as out:
        for i, data in fixed:
            zi = zipfile.ZipInfo(i.filename, date_time=(1980, 1, 1, 0, 0, 0))
            zi.compress_type = zipfile.ZIP_STORED
            zi.external_attr = i.external_attr
            out.writestr(zi, sid if i.filename.endswith("serialization_id") else data)
    tmp.replace(pt2)


SYSTEM_PREFIXES = ("/usr/lib/", "/System/", "/lib/", "/lib64/")


def _rewrite_members(pt2: Path, fn) -> int:
    """Rewrite native members of a stored .pt2 in place: fn(path_on_disk) -> bool changed."""
    with zipfile.ZipFile(pt2) as z:
        entries = [(i, z.read(i)) for i in z.infolist()]
    changed = 0
    out_entries = []
    with tempfile.TemporaryDirectory() as d:
        for i, data in entries:
            if i.filename.endswith((".so", ".dylib")):
                p = Path(d) / Path(i.filename).name
                p.write_bytes(data)
                if fn(p):
                    changed += 1
                    data = p.read_bytes()
            out_entries.append((i, data))
    if changed:
        tmp = pt2.with_suffix(".rw.tmp")
        with zipfile.ZipFile(tmp, "w", zipfile.ZIP_STORED) as out:
            for i, data in out_entries:
                zi = zipfile.ZipInfo(i.filename, date_time=i.date_time)
                zi.compress_type = zipfile.ZIP_STORED
                zi.external_attr = i.external_attr
                out.writestr(zi, data)
        tmp.replace(pt2)
    return changed


def _macho(path: Path) -> tuple[str | None, list[str], list[str]]:
    """(install name, dependency install names, LC_RPATH entries) of a Mach-O file."""
    lines = subprocess.run(["otool", "-l", str(path)], capture_output=True, text=True, check=True).stdout.splitlines()
    ident, deps, rpaths, cmd = None, [], [], None
    for ln in lines:
        s = ln.strip()
        if s.startswith("cmd "):
            cmd = s.split()[1]
        elif s.startswith(("name ", "path ")):
            val = s.split(None, 1)[1].rsplit(" (offset", 1)[0]
            if cmd == "LC_ID_DYLIB":
                ident = val
            elif cmd in ("LC_LOAD_DYLIB", "LC_LOAD_WEAK_DYLIB", "LC_REEXPORT_DYLIB"):
                deps.append(val)
            elif cmd == "LC_RPATH":
                rpaths.append(val)
    return ident, deps, rpaths


def macos_relocatable(pt2: Path) -> int:
    """macOS: the wrapper's install name is the build host's absolute path and libomp is
    imported from an absolute Homebrew-style path (/opt/llvm-openmp/lib/libomp.dylib).
    Rewrite the id and every non-system absolute dependency to @rpath/<file name> (libtorch's
    own libraries -- libomp included -- are already loaded under those names), drop absolute
    LC_RPATHs, re-sign ad hoc (arm64 requires a valid signature)."""

    def fix(p: Path) -> bool:
        ident, deps, rpaths = _macho(p)
        args = []
        if ident and not ident.startswith("@"):
            args += ["-id", f"@rpath/{Path(ident).name}"]
        for dep in deps:
            if dep.startswith("/") and not dep.startswith(SYSTEM_PREFIXES):
                args += ["-change", dep, f"@rpath/{Path(dep).name}"]
        for rp in rpaths:
            if rp.startswith("/"):
                args += ["-delete_rpath", rp]
        if not args:
            return False
        subprocess.run(["install_name_tool", *args, str(p)], check=True, capture_output=True)
        subprocess.run(["codesign", "--force", "--sign", "-", str(p)], check=True, capture_output=True)
        return True

    return _rewrite_members(pt2, fix)


HOST_MARKERS = ("/home/", "/Users/", "/work/", "C:\\Users", "C:/Users", "/tmp/")


def host_path_problems(pt2: Path) -> list[str]:
    """No absolute build-host path in what the loader or the dynamic linker reads: ELF
    DT_RUNPATH/DT_RPATH/DT_NEEDED/DT_SONAME, Mach-O install name/dependencies/LC_RPATH, and
    the package metadata (*_metadata.json). (The bundled .cpp sources are informational and
    are not checked.)"""
    out = []
    with zipfile.ZipFile(pt2) as z, tempfile.TemporaryDirectory() as d:
        for n in z.namelist():
            base = Path(n).name
            if n.endswith("_metadata.json"):
                text = z.read(n).decode("utf-8", "replace")
                for m in HOST_MARKERS:
                    if m in text:
                        out.append(f"{base}: metadata contains a host path ({m}...)")
                        break
            if not n.endswith((".so", ".dylib")):
                continue
            p = Path(z.extract(n, d))
            head = p.read_bytes()[:4]
            if head == b"\x7fELF":
                dyn = subprocess.run(["readelf", "-dW", str(p)], capture_output=True, text=True).stdout
                for line in dyn.splitlines():
                    if any(t in line for t in ("(RUNPATH)", "(RPATH)", "(NEEDED)", "(SONAME)")):
                        val = line.rsplit("[", 1)[-1].rstrip("]")
                        if any(part.startswith("/") for part in val.split(":")):
                            out.append(f"{base}: {line.split()[1]} {val}")
            elif head[:4] in (b"\xcf\xfa\xed\xfe", b"\xca\xfe\xba\xbe"):
                ident, deps, rpaths = _macho(p)
                if ident and not ident.startswith("@"):
                    out.append(f"{base}: install name {ident}")
                out += [f"{base}: imports {x}" for x in deps if x.startswith("/") and not x.startswith(SYSTEM_PREFIXES)]
                out += [f"{base}: LC_RPATH {x}" for x in rpaths if x.startswith("/")]
    return out
