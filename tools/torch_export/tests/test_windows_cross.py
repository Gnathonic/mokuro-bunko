"""The MinGW wrapper the windows-cuda-* cross-builds compile through (toolchain.windows_cross)."""

import subprocess

import pytest

from torch_export import toolchain


def test_the_wrapper_defines_the_math_constants_and_drops_gcc_only_flags(tmp_path):
    # A GPU package with a host kernel (AnimeText's folded stride constant) compiles a
    # .kernel.cpp that includes cpp_prefix.h, whose PhiloxRNGEngine.h uses M_PI; MinGW's
    # math.h hides M_PI under -std=c++20 unless _USE_MATH_DEFINES is set (as on MSVC)
    s = toolchain.mingw_wrapper(tmp_path / "mingw-bin", "g++")
    assert "-D_USE_MATH_DEFINES" in s
    assert "-fno-tree-loop-vectorize|-fexcess-precision=fast) ;;" in s
    assert f"exec {tmp_path / 'mingw-bin'}/x86_64-w64-mingw32-g++ " in s


def test_the_wrapper_compiles_m_pi_under_strict_cxx20(tmp_path):
    mingw = toolchain.WORK / "win" / toolchain.LLVM_MINGW / "bin"
    if not (mingw / "x86_64-w64-mingw32-g++").exists():
        pytest.skip("no llvm-mingw (python -m torch_export fetch-windows-cross)")
    w = tmp_path / "x86_64-w64-mingw32-g++"
    w.write_text(toolchain.mingw_wrapper(mingw, "g++"))
    w.chmod(0o755)
    src = tmp_path / "k.cpp"
    src.write_text("#include <cmath>\nextern \"C\" double k() { return std::cos(2.0 * M_PI); }\n")
    r = subprocess.run([str(w), str(src), "-std=c++20", "-pedantic", "-fexcess-precision=fast", "-c",
                        "-o", str(tmp_path / "k.o")], capture_output=True, text=True)
    assert r.returncode == 0, r.stderr
