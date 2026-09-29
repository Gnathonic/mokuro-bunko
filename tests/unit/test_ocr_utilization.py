"""GPU and CPU busy, sampled while a benchmark's pages come out (ADDENDUM 9).

The owner's "neither the GPU nor the CPU seemed tapped" has to become a
number, and the number has to be about the SAME seconds the rate is about:
the window between the first and last emission of a trial, never the model
load in front of it and never the gap between trials.

Every probe here reads a path, so the tests hand it a directory of files
instead of a kernel.
"""

from __future__ import annotations

import time
from pathlib import Path

import pytest

from mokuro_bunko.ocr import utilization as util


def _sysfs(root: Path, *cards: int | None) -> Path:
    """A fake ``/sys/class/drm``. ``None`` = a card with no busy file."""
    for index, value in enumerate(cards):
        device = root / f"card{index}" / "device"
        device.mkdir(parents=True)
        if value is not None:
            (device / "gpu_busy_percent").write_text(f"{value}\n", encoding="utf-8")
    return root


def _stat(path: Path, user: float, idle: float) -> Path:
    path.write_text(
        f"cpu  {user} 0 0 {idle} 0 0 0 0 0 0\ncpu0 {user} 0 0 {idle} 0 0 0 0 0 0\n",
        encoding="utf-8",
    )
    return path


class TestTheGpuProbe:
    def test_it_reads_the_card_the_row_was_placed_on(self, tmp_path: Path) -> None:
        root = _sysfs(tmp_path / "drm", 11, 97)
        assert util.gpu_reader(0, root=root)() == 11
        assert util.gpu_reader(1, root=root)() == 97
        # `auto`/absent is card 0, which is what torch picks too.
        assert util.gpu_reader(None, root=root)() == 11
        # A card this host does not have does not crash a benchmark over it.
        assert util.gpu_reader(9, root=root)() == 11

    def test_cards_are_ordered_numerically_not_lexically(self, tmp_path: Path) -> None:
        root = tmp_path / "drm"
        for index in (0, 2, 10):
            device = root / f"card{index}" / "device"
            device.mkdir(parents=True)
            (device / "gpu_busy_percent").write_text(str(index), encoding="utf-8")
        assert [p.parent.parent.name for p in util.gpu_busy_files(root)] == [
            "card0",
            "card2",
            "card10",
        ]

    def test_a_host_with_neither_sysfs_nor_nvidia_says_nothing(self, tmp_path: Path) -> None:
        read = util.gpu_reader(0, root=tmp_path / "nothing-here", nvidia=lambda index: None)
        assert read() is None

    def test_nvidia_is_the_fallback_when_sysfs_has_no_busy_file(
        self, tmp_path: Path
    ) -> None:
        root = _sysfs(tmp_path / "drm", None)
        asked: list[int | None] = []

        def nvidia(index: int | None) -> float | None:
            asked.append(index)
            return 63.0

        read = util.gpu_reader(1, root=root, nvidia=nvidia)
        assert read() == 63.0
        assert asked == [1, 1], "asked once to decide, once to answer"

    def test_rubbish_in_the_file_is_not_a_percentage(self, tmp_path: Path) -> None:
        path = tmp_path / "busy"
        path.write_text("N/A\n", encoding="utf-8")
        assert util.read_percent_file(path) is None
        assert util.read_percent_file(tmp_path / "absent") is None


class TestTheCpuProbe:
    def test_the_percentage_is_the_delta_between_two_reads(self, tmp_path: Path) -> None:
        stat = _stat(tmp_path / "stat", user=100.0, idle=900.0)
        read = util.cpu_reader(stat)
        _stat(stat, user=175.0, idle=925.0)
        # 75 busy jiffies of the 100 that passed.
        assert read() == pytest.approx(75.0)
        _stat(stat, user=175.0, idle=1025.0)
        assert read() == pytest.approx(0.0)

    def test_iowait_counts_as_idle(self, tmp_path: Path) -> None:
        stat = tmp_path / "stat"
        stat.write_text("cpu  0 0 0 0 0 0 0 0 0 0\n", encoding="utf-8")
        read = util.cpu_reader(stat)
        stat.write_text("cpu  50 0 0 0 50 0 0 0 0 0\n", encoding="utf-8")
        assert read() == pytest.approx(50.0)

    def test_a_stat_file_that_is_not_there_says_nothing(self, tmp_path: Path) -> None:
        assert util.cpu_totals(tmp_path / "absent") is None
        assert util.cpu_reader(tmp_path / "absent")() is None


class TestTheMeansAreOverTheWindowOnly:
    def _sampler(self, series: list[tuple[float, float, float]]) -> util.UtilizationSampler:
        """A sampler driven by hand: (at, gpu, cpu) ticks, no thread, no clock."""
        clock = [0.0]
        gpu = [value for _at, value, _cpu in series]
        cpu = [value for _at, _gpu, value in series]
        sampler = util.UtilizationSampler(
            gpu=lambda: gpu.pop(0),
            cpu=lambda: cpu.pop(0),
            clock=lambda: clock[0],
        )
        for at, _g, _c in series:
            clock[0] = at
            sampler.sample()
        return sampler

    def test_the_load_in_front_of_the_first_page_is_not_in_the_mean(self) -> None:
        # A model load pinning one core and leaving the GPU idle, then the
        # pages: 100% GPU while they come out. The window is 10..13.
        sampler = self._sampler(
            [
                (8.0, 0.0, 100.0),
                (9.0, 0.0, 100.0),
                (10.0, 90.0, 40.0),
                (11.0, 100.0, 50.0),
                (12.0, 98.0, 30.0),
                (13.0, 92.0, 40.0),
                (14.0, 0.0, 5.0),
            ]
        )
        assert sampler.means(10.0, 13.0) == {"gpu_busy_pct": 95.0, "cpu_busy_pct": 40.0}

    def test_a_window_with_no_tick_inside_it_says_nothing(self) -> None:
        sampler = self._sampler([(0.0, 50.0, 50.0), (1.0, 50.0, 50.0)])
        assert sampler.means(0.1, 0.9) == {"gpu_busy_pct": None, "cpu_busy_pct": None}
        assert sampler.means(None, 3.0) == {"gpu_busy_pct": None, "cpu_busy_pct": None}
        assert sampler.means(3.0, 1.0) == {"gpu_busy_pct": None, "cpu_busy_pct": None}

    def test_a_probe_that_cannot_answer_leaves_a_null_not_a_zero(self) -> None:
        sampler = util.UtilizationSampler(gpu=lambda: None, cpu=lambda: 30.0, clock=lambda: 1.0)
        sampler.sample()
        assert sampler.means(0.0, 2.0) == {"gpu_busy_pct": None, "cpu_busy_pct": 30.0}

    def test_a_probe_that_raises_costs_a_sample_not_the_benchmark(self) -> None:
        def boom() -> float:
            raise OSError("the card went away")

        sampler = util.UtilizationSampler(gpu=boom, cpu=lambda: 10.0, clock=lambda: 1.0)
        assert sampler.sample().gpu_pct is None
        assert sampler.means(0.0, 2.0)["cpu_busy_pct"] == 10.0


class TestTheDeviceABenchmarkWatches:
    def test_the_engine_s_card_wins_over_the_detector_s(self) -> None:
        pools = type("P", (), {"stage_device": {"detect": "cpu", "engine": "gpu:1"}})()
        assert util.first_gpu_device(pools) == "gpu:1"
        assert util.device_index("gpu:1") == 1

    def test_a_monolithic_row_has_one_stage_and_it_counts(self) -> None:
        pools = type("P", (), {"stage_device": {"mokuro": "gpu:0"}})()
        assert util.first_gpu_device(pools) == "gpu:0"

    def test_auto_and_absent_are_both_no_opinion(self) -> None:
        assert util.first_gpu_device(type("P", (), {"stage_device": {}})()) is None
        assert util.first_gpu_device(type("P", (), {"stage_device": {"engine": "auto"}})()) is None
        assert util.device_index("cpu") is None and util.device_index(None) is None


def test_the_thread_samples_and_stops(tmp_path: Path) -> None:
    root = _sysfs(tmp_path / "drm", 42)
    stat = _stat(tmp_path / "stat", user=10.0, idle=90.0)
    with util.sampler_for("gpu:0", root=root, stat=stat, interval=0.01) as sampler:
        deadline = time.monotonic() + 5.0
        while len(sampler.samples) < 3 and time.monotonic() < deadline:
            time.sleep(0.01)
    assert len(sampler.samples) >= 3
    assert all(sample.gpu_pct == 42 for sample in sampler.samples)
    before = len(sampler.samples)
    assert sampler.means(sampler.samples[0].at, sampler.samples[-1].at)["gpu_busy_pct"] == 42.0
    assert len(sampler.samples) == before, "stopped means stopped"
