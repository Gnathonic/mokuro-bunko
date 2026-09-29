"""AMD cards that PyTorch's ROCm wheels were not built for, and AMD cards
with no system ROCm at all.

Measured on an RX 6600 (gfx1032, no /opt/rocm): `backend: auto` chose the
CPU, and with ROCm wheels a matmul dumped core until HSA_OVERRIDE_GFX_VERSION
named its family's built target (10.3.0 -> gfx1030).
"""

from __future__ import annotations

from pathlib import Path

import pytest

from mokuro_bunko.ocr import rocm_gfx
from mokuro_bunko.ocr.installer import detect_rocm

FLAGS = "gfx900 gfx906 gfx908 gfx90a gfx942 gfx1030 gfx1100 gfx1101 gfx1102 gfx1200 gfx1201"


def _sysfs(root: Path, *versions: int) -> Path:
    for index, version in enumerate(versions):
        node = root / "class" / "kfd" / "kfd" / "topology" / "nodes" / str(index)
        node.mkdir(parents=True)
        (node / "properties").write_text(
            f"cpu_cores_count 0\ngfx_target_version {version}\nmax_waves_per_simd 16\n",
            encoding="utf-8",
        )
    return root


@pytest.mark.parametrize(
    ("version", "name"),
    [(100302, "gfx1032"), (100300, "gfx1030"), (110001, "gfx1101"), (90010, "gfx90a"),
     (120001, "gfx1201"), (0, None)],
)
def test_the_kernel_version_number_names_the_target(version: int, name: str | None) -> None:
    assert rocm_gfx.gfx_name(version) == name


def test_cpu_nodes_are_not_targets(tmp_path: Path) -> None:
    assert rocm_gfx.device_targets(_sysfs(tmp_path, 0, 100302)) == ["gfx1032"]


def test_an_unbuilt_card_borrows_its_family_s_built_target() -> None:
    assert rocm_gfx.override_for(["gfx1032"], FLAGS) == "10.3.0"
    assert rocm_gfx.override_for(["gfx1034"], FLAGS) == "10.3.0"


def test_a_built_card_needs_nothing() -> None:
    assert rocm_gfx.override_for(["gfx1030"], FLAGS) is None
    assert rocm_gfx.override_for(["gfx1201"], FLAGS) is None


def test_a_family_the_build_lacks_entirely_is_left_alone() -> None:
    # No gfx1010 in the build: an override would only trade a clear error
    # for a wrong one, so none is set.
    assert rocm_gfx.override_for(["gfx1012"], FLAGS) is None


def test_the_override_is_set_only_when_the_user_has_not_set_one(tmp_path: Path) -> None:
    sysfs = _sysfs(tmp_path, 0, 100302)
    env: dict[str, str] = {}
    assert rocm_gfx.apply_override(env, FLAGS, sysfs) == "10.3.0"
    assert env["HSA_OVERRIDE_GFX_VERSION"] == "10.3.0"
    mine = {"HSA_OVERRIDE_GFX_VERSION": "10.3.1"}
    assert rocm_gfx.apply_override(mine, FLAGS, sysfs) is None
    assert mine["HSA_OVERRIDE_GFX_VERSION"] == "10.3.1"


def test_no_amd_device_changes_nothing(tmp_path: Path) -> None:
    env: dict[str, str] = {}
    assert rocm_gfx.apply_override(env, FLAGS, tmp_path) is None
    assert env == {}


def test_an_amd_gpu_is_found_from_the_driver_alone(tmp_path: Path) -> None:
    sysfs = _sysfs(tmp_path / "sys", 0, 100302)
    kfd = tmp_path / "dev" / "kfd"
    kfd.parent.mkdir()
    kfd.write_text("", encoding="utf-8")
    assert rocm_gfx.amd_gpu_present(sysfs, kfd)
    assert not rocm_gfx.amd_gpu_present(sysfs, tmp_path / "dev" / "missing")
    assert not rocm_gfx.amd_gpu_present(_sysfs(tmp_path / "cpu-only", 0), kfd)


def test_rocm_is_detected_without_a_system_rocm(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr("mokuro_bunko.ocr.installer.Path", _no_opt_rocm(Path))
    monkeypatch.setattr(
        "mokuro_bunko.ocr.installer.subprocess.run",
        _raise(FileNotFoundError("rocm-smi")),
    )
    monkeypatch.setattr("mokuro_bunko.ocr.rocm_gfx.amd_gpu_present", lambda *a, **k: True)
    assert detect_rocm() == (True, None)
    monkeypatch.setattr("mokuro_bunko.ocr.rocm_gfx.amd_gpu_present", lambda *a, **k: False)
    assert detect_rocm() == (False, None)


def test_the_verify_snippets_apply_the_override_and_compute() -> None:
    from mokuro_bunko.ocr.installer import EnginesInstaller, OCRInstaller

    for snippet in (OCRInstaller._VERIFY_SNIPPET, EnginesInstaller._VERIFY_SNIPPET):
        assert "HSA_OVERRIDE_GFX_VERSION" in snippet
        # A device that is only "available" is not a device that computes.
        assert "@" in snippet and "device=\"cuda\"" in snippet
        compile(snippet, "<verify>", "exec")


def _no_opt_rocm(real: type[Path]) -> type[Path]:
    class NoOptRocm(type(real())):  # type: ignore[misc]
        def exists(self) -> bool:
            if str(self) == "/opt/rocm":
                return False
            return super().exists()

    return NoOptRocm


def _raise(error: BaseException):  # type: ignore[no-untyped-def]
    def run(*_args: object, **_kwargs: object) -> None:
        raise error

    return run


def test_the_helper_is_staged_with_the_runner_and_applied_first() -> None:
    from mokuro_bunko.ocr import engine_runner
    from mokuro_bunko.ocr.staging import RUNNER_MODULES

    assert "rocm_gfx.py" in RUNNER_MODULES
    import inspect

    main = inspect.getsource(engine_runner.main)
    assert main.index("apply_rocm_override()") < main.index("return serve(args)")
