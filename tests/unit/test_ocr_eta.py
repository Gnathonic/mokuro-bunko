"""The OCR ETA: where a page rate comes from, and when the queue ends.

The bug these pin down: the in-volume ETA used to be ``elapsed / done`` with
``elapsed`` measured from the moment the volume started -- which on a served
engine is several seconds before its first page comes out, because the model
is still loading and the pipeline is still filling. A twelve-page volume that
finishes in eight seconds opened at over a minute, and only crept towards the
truth as the load was amortised away.

So: no rate may ever be divided out of a window that begins before the first
EMISSION (ADDENDUM 9), the FIRST number shown must come from the best prior
this machine has (this session's volumes, then the congestion history, then
the saved benchmark), and a queue's finishing time is a lane simulation over
those rates rather than a multiplication.
"""

from __future__ import annotations

import io
import json
import time
import zipfile
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.ocr.control import OcrControl
from mokuro_bunko.ocr.eta import (
    DEFAULT_STARTUP_SECONDS,
    MAX_LATENCY_SECONDS,
    MIN_INFLIGHT_PAGES,
    Lane,
    RateEstimate,
    RateModel,
    StartupEstimate,
    emission_rate,
    fit_latency,
    iso_utc,
    plan_queue,
)
from mokuro_bunko.ocr.generations import parse_generation_list
from mokuro_bunko.ocr.processor import OCRProcessor
from mokuro_bunko.ocr.session import SessionVolume
from mokuro_bunko.ocr.watcher import OCRWorker, _SessionJob
from mokuro_bunko.queue.api import QueueAPI

# A clock that is not "now", so an assertion on a formatted instant is a
# statement about the arithmetic rather than about when the test ran.
NOON = datetime(2026, 9, 22, 12, 0, 0, tzinfo=timezone.utc).timestamp()


def _congestion(storage: Path, runs: dict[str, list[dict[str, Any]]]) -> None:
    (storage / ".ocr-congestion.json").write_text(json.dumps(runs), encoding="utf-8")


def _bench(storage: Path, rows: dict[str, dict[str, Any]]) -> None:
    (storage / ".ocr-bench.json").write_text(json.dumps(rows), encoding="utf-8")


class TestEmissionRate:
    """``(M - 1) / (t_last - t_first)`` and nothing else."""

    def test_one_page_is_not_a_rate(self) -> None:
        # A window that BEGINS at the first page spans no time when only that
        # page has landed. Dividing by it is how 217052 pages/s happens.
        assert emission_rate(1, 0.0) is None
        assert emission_rate(1, 5.0) is None

    def test_m_minus_one_over_the_window(self) -> None:
        # 11 pages landed after the first, over 10 seconds.
        assert emission_rate(12, 10.0) == 1.1

    def test_a_window_of_no_time_is_not_a_rate(self) -> None:
        assert emission_rate(12, 0.0) is None


class TestLatencyFit:
    """``seconds = latency + pages / rate`` -- the part that is not pages.

    Measured on a real library: a served mokuro row produced (4, 2.0 s),
    (12, 3.0 s), (24, 4.0 s). A pure page rate is 1.7-2.7 s out on the short
    ones, because the pipeline fills and drains around every volume however
    short it is. The intercept is what that costs.
    """

    def test_two_exact_points_recover_the_line(self) -> None:
        # 20 pages apart, 2 s apart: a tenth of a second a page, and 1.6 s
        # that has nothing to do with pages.
        assert fit_latency([(4.0, 2.0), (24.0, 4.0)]) == pytest.approx(1.6)

    def test_the_measured_three_points_fit_within_a_fifth_of_a_second(self) -> None:
        latency = fit_latency([(4.0, 2.0), (12.0, 3.0), (24.0, 4.0)])
        assert latency is not None
        # The middle point is the check: 1.6 + 12/10 = 2.8 against 3.0.
        assert abs((latency + 12.0 / 10.0) - 3.0) < 0.2

    def test_one_page_count_cannot_separate_the_two(self) -> None:
        # Every volume the same length: any split between a fixed cost and a
        # per-page cost fits it equally well, so none is claimed.
        assert fit_latency([(200.0, 20.0), (200.0, 22.0)]) is None
        assert fit_latency([(200.0, 20.0)]) is None
        assert fit_latency([]) is None

    def test_a_negative_intercept_is_clamped_to_nothing(self) -> None:
        # The line through these says a volume costs -1 s before its pages.
        assert fit_latency([(10.0, 1.0), (20.0, 3.0)]) == 0.0

    def test_an_absurd_intercept_is_clamped_to_the_ceiling(self) -> None:
        assert fit_latency([(10.0, 100.0), (20.0, 101.0)]) == MAX_LATENCY_SECONDS

    def test_a_line_that_says_pages_are_free_is_not_a_fit(self) -> None:
        # Longer volume, same time or less: noise, not a fixed cost, and an
        # intercept read off it would swallow the whole volume.
        assert fit_latency([(10.0, 5.0), (20.0, 5.0)]) is None
        assert fit_latency([(10.0, 6.0), (20.0, 5.0)]) is None

    def test_the_newest_pairs_weigh_most(self) -> None:
        """Same decay as the EWMA, so a machine whose load changed is believed."""
        latency = fit_latency([(4.0, 2.0), (24.0, 4.0), (4.0, 12.0), (24.0, 14.0)])
        assert latency is not None
        # The newest pair of points says 11.6 s of fixed cost, the oldest
        # says 1.6, and the answer leans hard on the newest.
        assert latency > 9.0


class TestTheFittedSlopeIsTheRate:
    """The slope where a line fits, the pooled rate where it does not.

    The two cannot be mixed. A pooled rate is total pages over total seconds,
    so every volume's fixed cost is already amortised across its pages;
    adding a fitted intercept on top of one charges the pipeline fill twice.
    Measured on four real runs of the served road: the fit's own slope and
    intercept reproduced three volumes as 1.59 / 2.12 / 2.90 s against
    actuals of 1.26 / 2.28 / 2.78, while the same intercept over the pooled
    7.17 pages/s gave 1.89 / 3.00 / 4.67 -- up to 1.7x over.
    """

    def test_the_slope_wins_over_the_pooled_rate(self, tmp_path: Path) -> None:
        # Four real runs of one row, two lengths, so a line fits.
        _congestion(
            tmp_path,
            {
                "g-1": [
                    {"pages": 12, "elapsed": 2.10, "volume_pages": 12, "volume_seconds": 2.10},
                    {"pages": 24, "elapsed": 2.92, "volume_pages": 24, "volume_seconds": 2.92},
                    {"pages": 12, "elapsed": 2.12, "volume_pages": 12, "volume_seconds": 2.12},
                    {"pages": 24, "elapsed": 2.90, "volume_pages": 24, "volume_seconds": 2.90},
                ]
            },
        )
        estimate = RateModel(tmp_path).rate("g-1")
        assert estimate is not None
        pooled = 72 / 10.04
        assert pooled == pytest.approx(7.17, abs=0.01)
        # The MARGINAL cost of a page is nothing like the pooled average,
        # because the pooled average has the fill baked into it.
        assert estimate.pages_per_second > 14.0
        assert estimate.latency_seconds == pytest.approx(1.33, abs=0.02)
        assert estimate.source == "history (fit)"
        # And the two together reproduce the volumes they were fitted from.
        assert estimate.volume_seconds(12) == pytest.approx(2.11, abs=0.05)
        assert estimate.volume_seconds(24) == pytest.approx(2.91, abs=0.05)

    def test_the_pooled_rate_returns_when_no_line_fits(self, tmp_path: Path) -> None:
        # One page count: nothing can separate a fixed cost from a slow page,
        # so there is no slope to prefer and no intercept to charge.
        _congestion(
            tmp_path,
            {
                "g-1": [
                    {"pages": 12, "elapsed": 2.0, "volume_pages": 12, "volume_seconds": 2.0},
                    {"pages": 12, "elapsed": 4.0, "volume_pages": 12, "volume_seconds": 4.0},
                ]
            },
        )
        estimate = RateModel(tmp_path).rate("g-1")
        assert estimate is not None
        assert estimate.pages_per_second == pytest.approx(24 / 6.0)
        assert estimate.latency_seconds == 0.0
        assert estimate.source == "history"
        # Which is exactly the arithmetic from before the term existed.
        assert estimate.volume_seconds(12) == estimate.seconds_for(12)

    def test_a_session_says_which_of_the_two_it_used(self, tmp_path: Path) -> None:
        model = RateModel(tmp_path)
        model.record_volume("g-1", 12, 2.0)
        model.record_volume("g-1", 12, 4.0)
        assert model.rate("g-1").source == "session"  # type: ignore[union-attr]
        model.record_volume("g-1", 24, 4.0)
        assert model.rate("g-1").source == "session (fit)"  # type: ignore[union-attr]


class TestLatencyThroughTheModel:
    """Where the fit comes from, and what it does to a rate."""

    def test_the_session_supplies_it_once_two_lengths_have_run(
        self, tmp_path: Path
    ) -> None:
        model = RateModel(tmp_path)
        model.record_volume("g-1", 4, 2.0)
        assert model.latency("g-1") == 0.0  # one length says nothing
        model.record_volume("g-1", 24, 4.0)
        assert model.latency("g-1") == pytest.approx(1.6)
        estimate = model.rate("g-1")
        assert estimate is not None
        assert estimate.latency_seconds == pytest.approx(1.6)

    def test_the_congestion_history_is_the_prior(self, tmp_path: Path) -> None:
        """A record written before the volume pair existed still votes."""
        _congestion(
            tmp_path,
            {"g-1": [{"pages": 4, "elapsed": 2.0}, {"pages": 24, "elapsed": 4.0}]},
        )
        assert RateModel(tmp_path).latency("g-1") == pytest.approx(1.6)

    def test_the_volume_pair_beats_the_pipelines_item_count(
        self, tmp_path: Path
    ) -> None:
        """Found by running it: an ITEM is a unit of stage work, not a page.

        On the served road a four-page volume recorded sixteen items and a
        twelve-page one recorded thirty-eight, so a rate or a fit taken from
        `pages`/`elapsed` on that road is measuring the wrong thing entirely.
        The runner's own count now rides beside it and wins.
        """
        _congestion(
            tmp_path,
            {
                "g-1": [
                    {"pages": 16, "elapsed": 1.2, "volume_pages": 4, "volume_seconds": 2.0},
                    {"pages": 24, "elapsed": 3.7, "volume_pages": 24, "volume_seconds": 4.0},
                ]
            },
        )
        model = RateModel(tmp_path)
        assert model.latency("g-1") == pytest.approx(1.6)
        estimate = model.rate("g-1")
        assert estimate is not None
        # The line through the REAL pairs (4, 2.0) and (24, 4.0): a tenth of
        # a second a page. Fitted through the item counts it would not be.
        assert estimate.pages_per_second == pytest.approx(10.0)
        assert estimate.source == "history (fit)"

    def test_the_session_pulls_the_number_towards_its_own(
        self, tmp_path: Path
    ) -> None:
        """Both halves of the cost model blend, and by the same weight.

        The session's line says 10 s of fixed cost, the history's says 1.6,
        and the session has 30 pages of evidence against a 64-page prior --
        so it moves the answer about a third of the way and no further.
        """
        _congestion(
            tmp_path,
            {"g-1": [{"pages": 4, "elapsed": 2.0}, {"pages": 24, "elapsed": 4.0}]},
        )
        model = RateModel(tmp_path)
        model.record_volume("g-1", 10, 11.0)
        model.record_volume("g-1", 20, 12.0)
        weight = 30 / (30 + 64)
        assert model.latency("g-1") == pytest.approx(weight * 10.0 + (1 - weight) * 1.6)

    def test_the_volume_a_session_opened_with_is_not_a_steady_state_sample(
        self, tmp_path: Path
    ) -> None:
        """Measured twice, in both directions.

        The first volume through a fresh session pays for the pipeline
        filling behind it. That is a per-SESSION cost and `startup` already
        charges it once. Fitting a per-VOLUME intercept through it billed
        every later volume for it again (a session's third volume came out
        3.2x over); pooling it into the RATE made every later volume look
        slower than it is (the same volume, 2.1x over). It is out of both.
        """
        model = RateModel(tmp_path)
        model.record_volume("g-1", 4, 9.0, first_of_session=True)  # fill-inflated
        model.record_volume("g-1", 4, 2.0)
        model.record_volume("g-1", 24, 4.0)
        assert model.latency("g-1") == pytest.approx(1.6)
        estimate = model.rate("g-1")
        assert estimate is not None
        # The line through the two STEADY volumes, (4, 2.0) and (24, 4.0):
        # a tenth of a second a page beside 1.6 s of fill. The
        # four-pages-in-nine-seconds opener is in neither half of it.
        assert estimate.pages_per_second == pytest.approx(10.0)
        assert estimate.source == "session (fit)"
        # Its PAGES still count: it really did read them, so it moves what
        # the session is reported to have seen, and how far that evidence
        # outranks the machine's older evidence.
        assert estimate.volumes_observed == 3

    def test_the_opening_volume_is_used_while_it_is_all_there_is(
        self, tmp_path: Path
    ) -> None:
        """A fill-inflated measurement of THIS machine beats none at all.

        It is the only volume the session has finished, so it is the rate --
        under a source that says exactly what it is, so that nothing reads it
        as the speed of a pipeline that was already full.
        """
        model = RateModel(tmp_path)
        model.record_volume("g-1", 40, 20.0, first_of_session=True)
        estimate = model.rate("g-1")
        assert estimate is not None
        assert estimate.pages_per_second == pytest.approx(2.0)
        assert estimate.source == "session (opening volume)"
        assert estimate.volumes_observed == 1
        # One length still cannot separate a fixed cost from a slow page.
        assert model.latency("g-1") == 0.0

        # The moment a steady-state volume lands, it takes over outright.
        model.record_volume("g-1", 40, 5.0)
        taken_over = model.rate("g-1")
        assert taken_over is not None
        assert taken_over.pages_per_second == pytest.approx(8.0)
        assert taken_over.source == "session"

    def test_a_flagged_history_run_is_out_of_the_rate_and_the_fit(
        self, tmp_path: Path
    ) -> None:
        _congestion(
            tmp_path,
            {
                "g-1": [
                    {"pages": 9, "elapsed": 9.0, "volume_pages": 4,
                     "volume_seconds": 9.0, "volume_first": True},
                    {"pages": 4, "elapsed": 2.0, "volume_pages": 4, "volume_seconds": 2.0},
                    {"pages": 24, "elapsed": 4.0, "volume_pages": 24, "volume_seconds": 4.0},
                ]
            },
        )
        model = RateModel(tmp_path)
        assert model.latency("g-1") == pytest.approx(1.6)
        # The line through the two STEADY runs. Had the fill-inflated opener
        # been in it, four pages in nine seconds would have dragged both the
        # slope and the intercept a long way out.
        assert model.rate("g-1").pages_per_second == pytest.approx(10.0)  # type: ignore[union-attr]

    def test_a_history_of_nothing_but_openers_is_still_used(
        self, tmp_path: Path
    ) -> None:
        """Same rule as the session: better than no measurement at all."""
        _congestion(
            tmp_path,
            {
                "g-1": [
                    {"pages": 9, "elapsed": 9.0, "volume_pages": 4,
                     "volume_seconds": 9.0, "volume_first": True},
                ]
            },
        )
        estimate = RateModel(tmp_path).rate("g-1")
        assert estimate is not None
        assert estimate.pages_per_second == pytest.approx(4 / 9.0)
    def test_nothing_fittable_is_no_latency_at_all(self, tmp_path: Path) -> None:
        _bench(tmp_path, {"g-1": {"best": {"pages_per_second": 10.0}}})
        model = RateModel(tmp_path)
        assert model.latency("g-1") == 0.0
        estimate = model.rate("g-1")
        assert estimate is not None and estimate.latency_seconds == 0.0
        # Which is exactly how a volume was priced before the term existed.
        assert estimate.volume_seconds(20) == estimate.seconds_for(20)

    def test_a_volume_costs_its_fill_plus_its_pages(self, tmp_path: Path) -> None:
        model = RateModel(tmp_path)
        model.record_volume("g-1", 4, 2.0)
        model.record_volume("g-1", 24, 4.0)
        estimate = model.rate("g-1")
        assert estimate is not None
        assert estimate.volume_seconds(0) == pytest.approx(estimate.latency_seconds)
        assert estimate.volume_seconds(10) == pytest.approx(
            estimate.latency_seconds + estimate.seconds_for(10)
        )

    def test_it_rides_on_the_report(self, tmp_path: Path) -> None:
        model = RateModel(tmp_path)
        model.record_volume("g-1", 4, 2.0)
        model.record_volume("g-1", 24, 4.0)
        assert model.report("g-1")["rate"]["latency_seconds"] == pytest.approx(1.6)


class TestRateSourcePriority:
    """Session beats history beats benchmark, and each says which it is."""

    def test_nothing_measured_is_no_rate_at_all(self, tmp_path: Path) -> None:
        # Never a default rate: a made-up speed is exactly the failure this
        # replaces. The caller says "not known yet" instead.
        assert RateModel(tmp_path).rate("g-1") is None

    def test_the_benchmark_is_the_first_thing_to_speak(self, tmp_path: Path) -> None:
        _bench(tmp_path, {"g-1": {"best": {"pages_per_second": 13.0}}})
        estimate = RateModel(tmp_path).rate("g-1")
        assert estimate is not None
        assert estimate.pages_per_second == 13.0
        assert estimate.source == "bench"
        assert estimate.volumes_observed == 0

    def test_the_baseline_stands_in_when_a_bench_found_no_better(
        self, tmp_path: Path
    ) -> None:
        _bench(tmp_path, {"g-1": {"baseline": {"pages_per_second": 2.0}}})
        estimate = RateModel(tmp_path).rate("g-1")
        assert estimate is not None and estimate.pages_per_second == 2.0

    def test_the_history_outranks_the_benchmark(self, tmp_path: Path) -> None:
        _bench(tmp_path, {"g-1": {"best": {"pages_per_second": 13.0}}})
        _congestion(
            tmp_path,
            {"g-1": [{"pages": 200, "elapsed": 100.0}, {"pages": 100, "elapsed": 100.0}]},
        )
        estimate = RateModel(tmp_path).rate("g-1")
        assert estimate is not None
        # Pooled, not averaged per run: 300 pages over 200 seconds.
        assert estimate.pages_per_second == 1.5
        assert estimate.source == "history"
        assert estimate.volumes_observed == 2

    def test_one_real_volume_this_session_takes_over_from_both(
        self, tmp_path: Path
    ) -> None:
        _bench(tmp_path, {"g-1": {"best": {"pages_per_second": 13.0}}})
        _congestion(tmp_path, {"g-1": [{"pages": 200, "elapsed": 100.0}]})
        model = RateModel(tmp_path)
        model.record_volume("g-1", 180, 60.0)  # 3 pages/s, against a history of 2
        estimate = model.rate("g-1")
        assert estimate is not None
        # 180 pages of this session against a 64-page prior: the session owns
        # about three quarters of the answer and is named as the source.
        assert 2.6 < estimate.pages_per_second < 2.7
        assert estimate.source == "session"
        assert estimate.volumes_observed == 1

    def test_a_couple_of_tiny_volumes_do_not_replace_the_history(
        self, tmp_path: Path
    ) -> None:
        """Measured on a real library, and the reason the weight exists.

        The first volumes through a freshly opened session are latency, not
        throughput: a four-page volume costs nearly what a twenty-four-page
        one does while the pipeline fills. An EWMA that let those two own the
        number predicted the third volume of the session five times too slow.
        """
        _congestion(tmp_path, {"g-1": [{"pages": 1080, "elapsed": 84.0}]})  # 12.9 pages/s
        model = RateModel(tmp_path)
        model.record_volume("g-1", 4, 4.0)  # 1.0 pages/s
        model.record_volume("g-1", 12, 7.0)  # 1.7 pages/s
        estimate = model.rate("g-1")
        assert estimate is not None
        assert estimate.source == "session (fit)+history"
        # Sixteen pages against a 64-page prior: the session moves the
        # number without taking it over. Its own line says 2.7 pages/s, the
        # history says 12.9, and the answer stays much the nearer the
        # history -- which is the whole point of the weight.
        assert 6.0 < estimate.pages_per_second < 9.0

    def test_a_row_with_no_evidence_is_unaffected_by_another_row(
        self, tmp_path: Path
    ) -> None:
        model = RateModel(tmp_path)
        model.record_volume("g-1", 100, 50.0)
        assert model.rate("g-2") is None

    def test_a_volume_that_cannot_make_a_rate_is_not_one(self, tmp_path: Path) -> None:
        model = RateModel(tmp_path)
        model.record_volume("g-1", 0, 10.0)
        model.record_volume("g-1", 10, 0.0)
        model.record_volume("g-1", None, "eleven")
        assert model.rate("g-1") is None


class TestSessionEwma:
    """Newest volume is half the answer (alpha 0.5)."""

    def test_the_first_volume_is_the_whole_answer(self, tmp_path: Path) -> None:
        model = RateModel(tmp_path)
        model.record_volume("g-1", 100, 50.0)
        assert model.rate("g-1").pages_per_second == 2.0  # type: ignore[union-attr]

    def test_the_second_volume_is_half_of_it(self, tmp_path: Path) -> None:
        model = RateModel(tmp_path)
        model.record_volume("g-1", 100, 50.0)  # 2.0 pages/s
        model.record_volume("g-1", 100, 25.0)  # 4.0 pages/s
        estimate = model.rate("g-1")
        assert estimate is not None
        assert estimate.pages_per_second == 3.0
        assert estimate.volumes_observed == 2

    def test_a_machine_that_speeds_up_is_believed_within_two_volumes(
        self, tmp_path: Path
    ) -> None:
        model = RateModel(tmp_path)
        model.record_volume("g-1", 100, 100.0)  # 1 page/s
        for _ in range(3):
            model.record_volume("g-1", 100, 10.0)  # 10 pages/s
        estimate = model.rate("g-1")
        assert estimate is not None
        # 1 -> 5.5 -> 7.75 -> 8.875: most of the way there by the third.
        assert estimate.pages_per_second > 8.0


class TestBlendingTheVolumeInFlight:
    """(d) corrects (a)-(c) as the volume proves itself, and never before."""

    def test_too_few_pages_to_vote(self, tmp_path: Path) -> None:
        _bench(tmp_path, {"g-1": {"best": {"pages_per_second": 10.0}}})
        model = RateModel(tmp_path)
        # Three pages in thirty seconds is a tenth of the benchmark's rate,
        # and still not evidence: the first pages out of a pipeline come out
        # at the fill rate.
        estimate = model.rate(
            "g-1", observed_pages=MIN_INFLIGHT_PAGES - 1, observed_seconds=30.0
        )
        assert estimate is not None
        assert estimate.pages_per_second == 10.0
        assert estimate.source == "bench"

    def test_a_slow_volume_drags_the_prior_down(self, tmp_path: Path) -> None:
        _bench(tmp_path, {"g-1": {"best": {"pages_per_second": 10.0}}})
        model = RateModel(tmp_path)
        # 16 pages after the first, over 32 s: 0.5 pages/s, against a
        # benchmark of 10. At 17 observed pages the volume is worth about
        # half, and the blend is in seconds per page, so the answer leans
        # towards the slower of the two.
        estimate = model.rate("g-1", observed_pages=17, observed_seconds=32.0)
        assert estimate is not None
        assert estimate.source == "bench+volume"
        assert 0.9 < estimate.pages_per_second < 1.2

    def test_the_blend_grows_with_the_evidence(self, tmp_path: Path) -> None:
        _bench(tmp_path, {"g-1": {"best": {"pages_per_second": 10.0}}})
        model = RateModel(tmp_path)
        early = model.rate("g-1", observed_pages=5, observed_seconds=8.0)
        late = model.rate("g-1", observed_pages=101, observed_seconds=200.0)
        assert early is not None and late is not None
        # Both see a 0.5 pages/s volume; the one with a hundred pages of it
        # believes the volume, the one with five still believes the bench.
        assert early.pages_per_second > late.pages_per_second
        assert late.pages_per_second < 0.7

    def test_with_no_prior_the_volume_is_the_answer(self, tmp_path: Path) -> None:
        estimate = RateModel(tmp_path).rate(
            "g-1", observed_pages=21, observed_seconds=10.0
        )
        assert estimate is not None
        assert estimate.pages_per_second == 2.0
        assert estimate.source == "volume"


class TestStartup:
    """Reported separately, never added to a rate, and never invented twice."""

    def test_the_default_is_marked_rough(self, tmp_path: Path) -> None:
        startup = RateModel(tmp_path).startup("g-1")
        assert startup.seconds == DEFAULT_STARTUP_SECONDS
        assert startup.source == "default"
        assert startup.rough is True

    def test_the_benchmark_beats_the_default(self, tmp_path: Path) -> None:
        _bench(tmp_path, {"g-1": {"startup_seconds": 11.2}})
        startup = RateModel(tmp_path).startup("g-1")
        assert startup.seconds == 11.2
        assert startup.source == "bench"
        assert startup.rough is False

    def test_this_process_beats_the_benchmark(self, tmp_path: Path) -> None:
        _bench(tmp_path, {"g-1": {"startup_seconds": 11.2}})
        model = RateModel(tmp_path)
        model.record_startup("g-1", 4.5)
        startup = model.startup("g-1")
        assert startup.seconds == 4.5
        assert startup.source == "session"
        assert startup.rough is False

    def test_startup_never_enters_a_rate(self, tmp_path: Path) -> None:
        _bench(tmp_path, {"g-1": {"startup_seconds": 600.0}})
        model = RateModel(tmp_path)
        model.record_volume("g-1", 100, 50.0)
        assert model.rate("g-1").pages_per_second == 2.0  # type: ignore[union-attr]


class TestProgressMetricsFromFirstEmission:
    """`_progress_metrics` after the fix: no elapsed-time guessing."""

    def test_before_the_first_page_there_is_no_eta(self, tmp_path: Path) -> None:
        processor = OCRProcessor(storage_path=tmp_path)
        percent, eta, status = processor._progress_metrics(
            done=0, total_images=12, elapsed=0.0, rate=13.0
        )
        assert status == "starting"
        assert eta is None
        assert percent == 0

    def test_the_first_page_is_priced_by_the_prior_not_by_the_wait(
        self, tmp_path: Path
    ) -> None:
        """The owner's case, in one assertion.

        A twelve-page mokuro-fp16 volume on the served road. The first page
        result arrives five seconds after the volume started, because the
        fork holds the whole volume in flight and answers in a burst. The
        OLD code divided one page by those five seconds and predicted the
        remaining eleven at five seconds each: 55 s, for a volume that
        finishes in under ten. The window now starts AT that first emission,
        so it is zero, and the number shown is the benchmark's 13 pages/s.
        """
        processor = OCRProcessor(storage_path=tmp_path)
        old_eta = int((12 - 1) / (1 / 5.0))
        assert old_eta == 55

        percent, eta, status = processor._progress_metrics(
            done=1, total_images=12, elapsed=0.0, rate=13.0
        )
        assert status == "running"
        assert eta == 0  # 11 pages at 13 pages/s, to the second
        assert percent == 8

    def test_without_a_prior_it_falls_back_to_the_emission_rate(
        self, tmp_path: Path
    ) -> None:
        processor = OCRProcessor(storage_path=tmp_path)
        # 21 pages landed, 20 intervals over 10 s = 2 pages/s; 79 left.
        _, eta, status = processor._progress_metrics(
            done=21, total_images=100, elapsed=10.0
        )
        assert status == "running"
        assert eta == 39

    def test_a_single_page_never_extrapolates_from_elapsed(
        self, tmp_path: Path
    ) -> None:
        processor = OCRProcessor(storage_path=tmp_path)
        _, eta, _ = processor._progress_metrics(done=1, total_images=12, elapsed=5.0)
        assert eta is None

    def test_all_pages_out_is_finalizing(self, tmp_path: Path) -> None:
        processor = OCRProcessor(storage_path=tmp_path)
        percent, eta, status = processor._progress_metrics(
            done=195, total_images=195, elapsed=120.0, rate=2.0
        )
        assert (percent, eta, status) == (100, 0, "finalizing")


def _rate_for(rates: dict[str, float], latency: float = 0.0):
    def rate_for(
        generation_id: str, observed_pages: int = 0, observed_seconds: float = 0.0
    ) -> RateEstimate | None:
        value = rates.get(generation_id)
        return RateEstimate(value, "bench", 0, latency) if value else None

    return rate_for


def _startup_for(startups: dict[str, float]):
    def startup_for(generation_id: str) -> StartupEstimate:
        return StartupEstimate(startups.get(generation_id, 0.0), "bench")

    return startup_for


def _pending(*items: tuple[str, str, int | None]) -> list[dict[str, Any]]:
    return [
        {"series": "S", "volume": volume, "generation": gen, "generation_id": gen, "pages": pages}
        for gen, volume, pages in items
    ]


class TestLaneSimulation:
    """The queue across `ocr.concurrency` lanes, in run order."""

    def test_one_lane_runs_the_queue_end_to_end(self) -> None:
        plan = plan_queue(
            [],
            _pending(("g-1", "A", 100), ("g-1", "B", 200)),
            lane_count=1,
            rate_for=_rate_for({"g-1": 10.0}),
            startup_for=_startup_for({"g-1": 20.0}),
            now=NOON,
        )
        # 20 s to open the session, then 10 s of pages, then 20 s more.
        assert [item["eta_seconds"] for item in plan.pending] == [30, 50]
        assert plan.pending[0]["eta_at"] == iso_utc(NOON + 30)
        assert plan.done_at == iso_utc(NOON + 50)

    def test_two_lanes_share_the_queue(self) -> None:
        plan = plan_queue(
            [],
            _pending(("g-1", "A", 100), ("g-1", "B", 100), ("g-1", "C", 100)),
            lane_count=2,
            rate_for=_rate_for({"g-1": 10.0}),
            startup_for=_startup_for({"g-1": 0.0}),
            now=NOON,
        )
        # A and B start together; C waits for whichever frees first.
        assert [item["eta_seconds"] for item in plan.pending] == [10, 10, 20]
        assert plan.done_at == iso_utc(NOON + 20)

    def test_switching_rows_pays_that_rows_startup(self) -> None:
        plan = plan_queue(
            [],
            _pending(("g-1", "A", 100), ("g-2", "B", 100)),
            lane_count=1,
            rate_for=_rate_for({"g-1": 10.0, "g-2": 10.0}),
            startup_for=_startup_for({"g-1": 5.0, "g-2": 30.0}),
            now=NOON,
        )
        # 5 + 10 for the first row, then 30 more to load the second.
        assert [item["eta_seconds"] for item in plan.pending] == [15, 55]

    def test_a_row_already_running_skips_its_startup(self) -> None:
        running = [
            {
                "generation_id": "g-1",
                "slot": 0,
                "done_pages": 50,
                "total_pages": 100,
                "first_page_at": NOON - 5.0,
                "status": "running",
            }
        ]
        plan = plan_queue(
            running,
            _pending(("g-1", "B", 100)),
            lane_count=1,
            rate_for=_rate_for({"g-1": 10.0}),
            startup_for=_startup_for({"g-1": 30.0}),
            now=NOON,
        )
        # 5 s left on the running volume, then straight into the next one:
        # the session is warm, so its 30 s is not paid again.
        assert plan.running[0]["eta_seconds"] == 5
        assert plan.pending[0]["eta_seconds"] == 15

    def test_a_lane_stops_at_a_volume_it_cannot_price(self) -> None:
        """Found by running it: a lookahead volume was priced as if the
        volume ahead of it -- whose length the runner had not announced --
        took no time at all, and came out four minutes early."""
        running = [
            {
                "generation_id": "g-1", "slot": 0, "done_pages": 0,
                "total_pages": None, "status": "starting",
                "session_started_at": NOON - 1.0,
            },
            {
                "generation_id": "g-1", "slot": 0, "done_pages": 0,
                "total_pages": 4, "status": "starting",
                "session_started_at": NOON - 1.0,
            },
        ]
        plan = plan_queue(
            running, [], lane_count=1,
            rate_for=_rate_for({"g-1": 10.0}),
            startup_for=_startup_for({"g-1": 0.0}),
            now=NOON,
        )
        assert plan.running[0]["eta_at"] is None
        assert plan.running[1]["eta_at"] is None
        assert plan.running[1]["reason"]
        assert plan.done_at is None

    def test_a_session_holds_two_volumes_on_one_lane(self) -> None:
        """A lookahead of two is one pipeline, not two lanes.

        Both volumes report as running; their pages go through the SAME
        stream, so the lane's remaining time is the sum of what they have
        left -- not the larger of two independent jobs.
        """
        running = [
            {
                "generation_id": "g-1", "slot": 0, "done_pages": 90, "total_pages": 100,
                "first_page_at": NOON - 9.0, "status": "running",
            },
            {
                "generation_id": "g-1", "slot": 0, "done_pages": 0, "total_pages": 100,
                "session_started_at": NOON - 60.0, "status": "starting",
            },
        ]
        plan = plan_queue(
            running,
            _pending(("g-1", "C", 100)),
            lane_count=1,
            rate_for=_rate_for({"g-1": 10.0}),
            startup_for=_startup_for({"g-1": 30.0}),
            now=NOON,
        )
        # 1 s left on the first, a whole 10 s for the second, then C.
        assert plan.pending[0]["eta_seconds"] == 21

    def test_two_slots_are_two_lanes(self) -> None:
        running = [
            {
                "generation_id": "g-1", "slot": 0, "done_pages": 50, "total_pages": 100,
                "first_page_at": NOON - 5.0, "status": "running",
            },
            {
                "generation_id": "g-1", "slot": 1, "done_pages": 90, "total_pages": 100,
                "first_page_at": NOON - 9.0, "status": "running",
            },
        ]
        plan = plan_queue(
            running,
            _pending(("g-1", "C", 100)),
            lane_count=2,
            rate_for=_rate_for({"g-1": 10.0}),
            startup_for=_startup_for({"g-1": 0.0}),
            now=NOON,
        )
        # Slot 1 frees after 1 s, so C runs there: 1 + 10.
        assert plan.pending[0]["eta_seconds"] == 11

    def test_an_unknown_page_count_uses_the_median_and_says_so(self) -> None:
        plan = plan_queue(
            [],
            _pending(("g-1", "A", 100), ("g-1", "B", 300), ("g-1", "C", None)),
            lane_count=1,
            rate_for=_rate_for({"g-1": 10.0}),
            startup_for=_startup_for({"g-1": 0.0}),
            now=NOON,
        )
        assert plan.pending[2]["pages"] == 200  # median of 100 and 300
        assert plan.pending[2]["rough"] is True
        assert plan.pending[0]["rough"] is False
        assert [item["eta_seconds"] for item in plan.pending] == [10, 40, 60]

    def test_no_rate_stops_the_prediction_there_and_after(self) -> None:
        plan = plan_queue(
            [],
            _pending(("g-1", "A", 100), ("g-2", "B", 100), ("g-1", "C", 100)),
            lane_count=1,
            rate_for=_rate_for({"g-1": 10.0}),
            startup_for=_startup_for({"g-1": 0.0}),
            now=NOON,
        )
        assert plan.pending[0]["eta_at"] == iso_utc(NOON + 10)
        assert plan.pending[1]["eta_at"] is None
        assert "g-2" in plan.pending[1]["reason"]
        # And everything behind the unknown volume, because the lane it
        # holds frees at an unknown time.
        assert plan.pending[2]["eta_at"] is None
        assert "behind" in plan.pending[2]["reason"]
        # A queue with an unknown volume in it has an unknown end.
        assert plan.done_at is None

    def test_a_row_that_reloads_per_volume_pays_every_time(self) -> None:
        plan = plan_queue(
            [],
            _pending(("g-1", "A", 100), ("g-1", "B", 100)),
            lane_count=1,
            rate_for=_rate_for({"g-1": 10.0}),
            startup_for=_startup_for({"g-1": 20.0}),
            now=NOON,
            startup_every_volume=lambda gen_id: True,
        )
        # The one-volume command line loads its model again for each volume.
        assert [item["eta_seconds"] for item in plan.pending] == [30, 60]

    def test_a_running_volume_of_unknown_length_blocks_rather_than_crashes(
        self,
    ) -> None:
        """Found by running it: a 500 on the first poll of every session.

        A volume is submitted before the runner has announced how long it is,
        so for a second or two the lane it holds frees at an unknown time.
        That makes everything behind it unknown -- which the contract already
        says -- but the arithmetic reached ``int(round(inf))`` first.
        """
        running = [
            {
                "generation_id": "g-1", "slot": 0, "done_pages": 0,
                "total_pages": None, "status": "starting",
                "session_started_at": NOON - 1.0,
            }
        ]
        plan = plan_queue(
            running,
            _pending(("g-1", "B", 100)),
            lane_count=1,
            rate_for=_rate_for({"g-1": 10.0}),
            startup_for=_startup_for({"g-1": 20.0}),
            now=NOON,
        )
        assert plan.pending[0]["eta_at"] is None
        assert plan.pending[0]["reason"]
        assert plan.done_at is None
        # And it is JSON, which is what the 500 was really about.
        json.dumps(plan.pending)

    def test_every_queued_volume_pays_the_fixed_cost_once(self) -> None:
        plan = plan_queue(
            [],
            _pending(("g-1", "A", 100), ("g-1", "B", 100)),
            lane_count=1,
            rate_for=_rate_for({"g-1": 10.0}, latency=1.6),
            startup_for=_startup_for({"g-1": 0.0}),
            now=NOON,
        )
        # 1.6 s of fill and 10 s of pages, each -- and the startup is a
        # separate charge that this row does not pay twice.
        assert [item["eta_seconds"] for item in plan.pending] == [12, 23]
        assert plan.pending[0]["latency_seconds"] == 1.6

    def test_the_fixed_cost_is_what_saves_the_short_volumes(self) -> None:
        """The reason the term exists, in the arithmetic.

        A four-page volume really took 2.0 s on the measured machine. At
        10 pages/s alone that is 0.4 s -- five times too fast. With the fitted
        1.6 s of fill it is 2.0 s.
        """
        without = plan_queue(
            [], _pending(("g-1", "A", 4)), lane_count=1,
            rate_for=_rate_for({"g-1": 10.0}),
            startup_for=_startup_for({"g-1": 0.0}), now=NOON,
        )
        with_fill = plan_queue(
            [], _pending(("g-1", "A", 4)), lane_count=1,
            rate_for=_rate_for({"g-1": 10.0}, latency=1.6),
            startup_for=_startup_for({"g-1": 0.0}), now=NOON,
        )
        assert without.pending[0]["eta_seconds"] == 0
        assert with_fill.pending[0]["eta_seconds"] == 2

    def test_an_empty_queue_with_nothing_running_has_no_end(self) -> None:
        plan = plan_queue(
            [], [], lane_count=2,
            rate_for=_rate_for({}), startup_for=_startup_for({}), now=NOON,
        )
        assert plan.done_at is None

    def test_the_end_is_the_running_job_when_nothing_is_queued(self) -> None:
        running = [
            {
                "generation_id": "g-1", "slot": 0, "done_pages": 50, "total_pages": 100,
                "first_page_at": NOON - 5.0, "status": "running",
            }
        ]
        plan = plan_queue(
            running, [], lane_count=2,
            rate_for=_rate_for({"g-1": 10.0}),
            startup_for=_startup_for({"g-1": 0.0}),
            now=NOON,
        )
        # The idle second lane does not drag the answer back to now.
        assert plan.done_at == iso_utc(NOON + 5)


class TestARateOnOneMachine:
    """`RateModel.rate_on`: a row's rate on the machine a lane belongs to.

    A processor's evidence lives under ``<row>@<name>`` so it never moves
    this server's rate; the queue still has to price a processor's lanes, so
    each machine is asked for its OWN number first and then falls back --
    this server's measurements, another machine's, the row's benchmark --
    rather than to nothing.
    """

    def test_this_servers_lane_is_the_rows_own_rate(self, tmp_path: Path) -> None:
        _bench(tmp_path, {"g-1": {"best": {"pages_per_second": 13.0}}})
        model = RateModel(tmp_path)
        assert model.rate_on("g-1", None) == model.rate("g-1")
        assert model.rate_on("g-1", "g-1") == model.rate("g-1")

    def test_a_machine_is_priced_by_its_own_evidence_first(self, tmp_path: Path) -> None:
        _bench(tmp_path, {"g-1": {"best": {"pages_per_second": 13.0}}})
        model = RateModel(tmp_path)
        model.record_volume("g-1", 100, 100.0)  # this server: 1 page/s
        model.record_volume("g-1@tower", 100, 10.0)  # tower: 10 pages/s
        estimate = model.rate_on("g-1", "g-1@tower")
        assert estimate is not None and estimate.pages_per_second == 10.0

    def test_its_own_benchmark_is_its_own_evidence(self, tmp_path: Path) -> None:
        model = RateModel(tmp_path)
        model.record_volume("g-1", 100, 100.0)
        prior = RateEstimate(40.0, "bench", 0, 0.0)
        estimate = model.rate_on("g-1", "g-1@tower", machine_prior=prior)
        assert estimate is not None and estimate.pages_per_second == 40.0

    def test_with_nothing_of_its_own_this_servers_measurement_stands_in(
        self, tmp_path: Path
    ) -> None:
        _bench(tmp_path, {"g-1": {"best": {"pages_per_second": 13.0}}})
        model = RateModel(tmp_path)
        model.record_volume("g-1", 100, 50.0)  # 2 pages/s here
        model.record_volume("g-1@box", 100, 20.0)  # 5 pages/s on box
        estimate = model.rate_on("g-1", "g-1@tower")
        assert estimate is not None and estimate.pages_per_second == 2.0

    def test_then_another_machines_measurement_before_any_benchmark(
        self, tmp_path: Path
    ) -> None:
        _bench(tmp_path, {"g-1": {"best": {"pages_per_second": 13.0}}})
        model = RateModel(tmp_path)
        model.record_volume("g-1@box", 100, 20.0)
        estimate = model.rate_on("g-1", "g-1@tower")
        assert estimate is not None and estimate.pages_per_second == 5.0

    def test_another_row_on_that_machine_is_never_borrowed(self, tmp_path: Path) -> None:
        model = RateModel(tmp_path)
        model.record_volume("g-10@box", 100, 20.0)
        assert model.rate_on("g-1", "g-1@tower") is None

    def test_last_the_rows_benchmark(self, tmp_path: Path) -> None:
        _bench(tmp_path, {"g-1": {"best": {"pages_per_second": 13.0}}})
        estimate = RateModel(tmp_path).rate_on("g-1", "g-1@tower")
        assert estimate is not None and estimate.pages_per_second == 13.0
        assert estimate.source == "bench"

    def test_the_volume_in_flight_blends_in_as_it_does_locally(
        self, tmp_path: Path
    ) -> None:
        model = RateModel(tmp_path)
        model.record_volume("g-1@tower", 100, 10.0)
        estimate = model.rate_on(
            "g-1", "g-1@tower", observed_pages=40, observed_seconds=40.0
        )
        assert estimate is not None
        assert estimate.source.endswith("+volume")
        assert 1.0 < estimate.pages_per_second < 10.0

    def test_startup_is_the_machines_own_then_the_rows(self, tmp_path: Path) -> None:
        model = RateModel(tmp_path)
        model.record_startup("g-1", 30.0)
        assert model.startup_on("g-1", "g-1@tower").seconds == 30.0
        assert model.startup_on("g-1", "g-1@tower", machine_prior=4.0).seconds == 4.0
        model.record_startup("g-1@tower", 6.0)
        assert model.startup_on("g-1", "g-1@tower", machine_prior=4.0).seconds == 6.0
        assert model.startup_on("g-1", None).seconds == 30.0

    def test_forgetting_a_row_forgets_it_on_every_machine(self, tmp_path: Path) -> None:
        model = RateModel(tmp_path)
        model.record_volume("g-1@tower", 100, 10.0)
        model.record_startup("g-1@tower", 6.0)
        model.record_volume("g-10@tower", 100, 10.0)
        model.forget("g-1")
        assert model.rate_on("g-1", "g-1@tower") is None
        assert model.startup_on("g-1", "g-1@tower").source == "default"
        assert model.rate("g-10@tower") is not None


def _machine_rate_for(rates: dict[tuple[str, str | None], float]):
    def rate_for(
        generation_id: str,
        machine: str | None = None,
        observed_pages: int = 0,
        observed_seconds: float = 0.0,
    ) -> RateEstimate | None:
        value = rates.get((generation_id, machine))
        return RateEstimate(value, "bench", 0, 0.0) if value else None

    return rate_for


def _machine_startup_for(seconds: float = 0.0):
    def startup_for(generation_id: str, machine: str | None = None) -> StartupEstimate:
        return StartupEstimate(seconds, "bench")

    return startup_for


class TestLanesOnDifferentMachines:
    """`lane_machines`: each lane is priced by the machine it runs on."""

    def test_each_lane_prices_its_items_with_its_own_machines_rate(self) -> None:
        plan = plan_queue(
            [],
            _pending(("g-1", "A", 20), ("g-1", "B", 20), ("g-1", "C", 20)),
            lane_count=2,
            lane_machines=["local", "tower"],
            rate_for=_machine_rate_for({("g-1", "local"): 1.0, ("g-1", "tower"): 5.0}),
            startup_for=_machine_startup_for(),
            now=NOON,
        )
        # Each to the lane that FINISHES it first, as the worker assigns
        # them: tower reads all three (4 s each, 12 s in all) before this
        # server would have read one (20 s).
        assert [item["eta_seconds"] for item in plan.pending] == [4, 8, 12]
        assert plan.done_at == iso_utc(NOON + 12)

    def test_a_running_volume_holds_a_lane_of_its_own_machine(self) -> None:
        running = [
            {"generation_id": "g-1", "slot": 1, "machine": "tower", "status": "running",
             "done_pages": 5, "total_pages": 20, "first_page_at": NOON - 1.0},
        ]
        plan = plan_queue(
            running,
            _pending(("g-1", "A", 20), ("g-1", "B", 20)),
            lane_count=2,
            lane_machines=["local", "tower"],
            rate_for=_machine_rate_for({("g-1", "local"): 1.0, ("g-1", "tower"): 5.0}),
            startup_for=_machine_startup_for(),
            now=NOON,
        )
        # Fifteen pages left at tower's 5 pages/s.
        assert plan.running[0]["eta_seconds"] == 3
        # The local lane is free, but tower finishes A (3 + 4 s) and then B
        # (+ 4 s) before this server would finish A (20 s).
        assert [item["eta_seconds"] for item in plan.pending] == [7, 11]

    def test_a_lane_whose_machine_cannot_be_priced_stops_the_prediction(self) -> None:
        plan = plan_queue(
            [],
            _pending(("g-1", "A", 20)),
            lane_count=1,
            lane_machines=["tower"],
            rate_for=_machine_rate_for({("g-1", "local"): 1.0}),
            startup_for=_machine_startup_for(),
            now=NOON,
        )
        assert plan.pending[0]["eta_at"] is None
        assert plan.done_at is None


class TestRunningJobFields:
    """What the status endpoint says about the volume being read."""

    def test_a_starting_job_is_priced_from_the_prior_never_from_the_wait(
        self,
    ) -> None:
        """The owner's twelve-page case, through the whole pipeline.

        Nothing has come out of this volume, so its own emissions say
        nothing. What IS known is what the model load still owes (12 s of a
        measured 20) and how fast this row reads a page (13/s from the
        benchmark), and 12 + 12/13 is a number this machine can be held to.
        The elapsed wait never enters it.
        """
        running = [
            {
                "generation_id": "g-1", "slot": 0, "done_pages": 0, "total_pages": 12,
                "session_started_at": NOON - 8.0, "status": "starting",
            }
        ]
        plan = plan_queue(
            running, [], lane_count=1,
            rate_for=_rate_for({"g-1": 13.0}),
            startup_for=_startup_for({"g-1": 20.0}),
            now=NOON,
        )
        job = plan.running[0]
        assert job["status"] == "starting"
        assert job["startup_seconds"] == 12  # 20 promised, 8 already spent
        assert job["eta_seconds"] == 13  # 12 s of load, then 12 pages at 13/s
        assert job["eta_at"] == iso_utc(NOON + 12 + 12 / 13)
        assert plan.done_at == job["eta_at"]

    def test_a_warm_session_stops_counting_down_a_startup(self) -> None:
        running = [
            {
                "generation_id": "g-1", "slot": 0, "done_pages": 0, "total_pages": 12,
                "session_started_at": NOON - 300.0, "status": "starting",
            }
        ]
        plan = plan_queue(
            running, [], lane_count=1,
            rate_for=_rate_for({"g-1": 13.0}),
            startup_for=_startup_for({"g-1": 20.0}),
            now=NOON,
        )
        # Nothing left to load, so nothing to say about loading -- and the
        # page's "starting up (≈ 0s)" goes with it.
        assert plan.running[0]["startup_seconds"] is None
        assert plan.running[0]["eta_seconds"] == 1

    def test_the_second_volume_of_a_session_gets_its_turn_priced(self) -> None:
        """A lookahead volume used to read "starting up" for minutes.

        It is submitted the moment the session opens and sits behind the one
        being read. It has a page count and its row has a rate, so its turn
        has a time: when the volume ahead finishes, plus its own length.
        """
        running = [
            {
                "generation_id": "g-1", "slot": 0, "done_pages": 50, "total_pages": 100,
                "first_page_at": NOON - 5.0, "status": "running",
            },
            {
                "generation_id": "g-1", "slot": 0, "done_pages": 0, "total_pages": 100,
                "session_started_at": NOON - 2.0, "status": "starting",
            },
        ]
        plan = plan_queue(
            running, [], lane_count=1,
            rate_for=_rate_for({"g-1": 10.0}),
            startup_for=_startup_for({"g-1": 20.0}),
            now=NOON,
        )
        assert plan.running[0]["eta_seconds"] == 5
        # 5 s for the one ahead, then its own 100 pages -- and NOT another
        # model load, because the session is already open.
        assert plan.running[1]["eta_seconds"] == 15
        assert plan.running[1]["startup_seconds"] is None
        assert plan.done_at == iso_utc(NOON + 15)

    def test_a_volume_with_no_page_out_yet_still_owes_its_fill(self) -> None:
        running = [
            {
                "generation_id": "g-1", "slot": 0, "done_pages": 0, "total_pages": 12,
                "session_started_at": NOON - 300.0, "status": "starting",
            }
        ]
        plan = plan_queue(
            running, [], lane_count=1,
            rate_for=_rate_for({"g-1": 10.0}, latency=1.6),
            startup_for=_startup_for({"g-1": 20.0}),
            now=NOON,
        )
        # Warm session, so no model load left -- but the pipeline has still
        # to fill around this volume: 1.6 + 12/10.
        assert plan.running[0]["startup_seconds"] is None
        assert plan.running[0]["eta_seconds"] == 3
        assert plan.running[0]["latency_seconds"] == 1.6

    def test_once_a_page_is_out_the_fill_has_been_paid(self) -> None:
        running = [
            {
                "generation_id": "g-1", "slot": 0, "done_pages": 2, "total_pages": 12,
                "first_page_at": NOON - 1.0, "status": "running",
            }
        ]
        plan = plan_queue(
            running, [], lane_count=1,
            rate_for=_rate_for({"g-1": 10.0}, latency=1.6),
            startup_for=_startup_for({"g-1": 0.0}),
            now=NOON,
        )
        # Ten pages left at 10/s, and NOT another 1.6 s: the pipeline filled
        # before that first page came out, which is how we know it did.
        assert plan.running[0]["eta_seconds"] == 1
        assert plan.running[0]["latency_seconds"] == 1.6

    def test_a_running_job_carries_its_rate_and_where_it_came_from(self) -> None:
        running = [
            {
                "generation_id": "g-1", "slot": 0, "done_pages": 6, "total_pages": 12,
                "first_page_at": NOON - 1.0, "status": "running",
            }
        ]
        plan = plan_queue(
            running, [], lane_count=1,
            rate_for=_rate_for({"g-1": 13.0}),
            startup_for=_startup_for({"g-1": 20.0}),
            now=NOON,
        )
        job = plan.running[0]
        assert job["rate_pages_per_second"] == 13.0
        assert job["rate_source"] == "bench"
        assert job["eta_seconds"] == 0
        assert job["eta_at"] == iso_utc(NOON)

    def test_an_error_card_does_not_take_the_queues_end_with_it(self) -> None:
        running = [
            {"generation_id": "g-1", "slot": 0, "done_pages": 3, "status": "error"}
        ]
        plan = plan_queue(
            running,
            _pending(("g-1", "B", 100)),
            lane_count=1,
            rate_for=_rate_for({"g-1": 10.0}),
            startup_for=_startup_for({"g-1": 0.0}),
            now=NOON,
        )
        assert plan.done_at == iso_utc(NOON + 10)


class TestIsoFormatting:
    """The server sends UTC and only UTC."""

    def test_the_instant_is_iso_8601_zulu(self) -> None:
        assert iso_utc(NOON) == "2026-09-22T12:00:00Z"

    def test_lanes_start_empty(self) -> None:
        assert Lane().generation_id is None and Lane().free_in == 0.0


# --- the wiring: worker events in, status fields out ----------------------


def _library(storage: Path, **series: list[str]) -> None:
    for name, volumes in series.items():
        folder = storage / "library" / name
        folder.mkdir(parents=True, exist_ok=True)
        for volume in volumes:
            with zipfile.ZipFile(folder / f"{volume}.cbz", "w") as zf:
                zf.writestr("page_000.jpg", b"fake image data")
            (folder / f"{volume}.mokuro").write_text("{}", encoding="utf-8")


def _worker(storage: Path, rows: list[Any], **kwargs: Any) -> OCRWorker:
    (storage / "library").mkdir(exist_ok=True)
    (storage / "inbox").mkdir(exist_ok=True)
    return OCRWorker(storage_path=storage, generations=rows, **kwargs)


def _status(app: Any) -> dict[str, Any]:
    captured: dict[str, Any] = {}

    def start_response(status: str, headers: list[tuple[str, str]]) -> None:
        captured["status"] = status

    environ = {
        "REQUEST_METHOD": "GET",
        "PATH_INFO": "/queue/api/status",
        "QUERY_STRING": "",
        "wsgi.input": io.BytesIO(b""),
        "wsgi.errors": io.StringIO(),
        "wsgi.url_scheme": "http",
        "SERVER_NAME": "localhost",
        "SERVER_PORT": "8080",
    }
    b"".join(app(environ, start_response))
    assert captured["status"].startswith("200"), captured
    # The endpoint sends a shaped, per-level payload (`queue.shape`); these
    # tests are about the model underneath it, which `raw_status` returns.
    return app.raw_status()  # type: ignore[no-any-return]


def _dummy_app(environ: dict[str, Any], start_response: Any) -> list[bytes]:
    start_response("404 Not Found", [])
    return [b""]


class TestWorkerLearnsFromItsSession:
    """`ready` and `volume_done` are the evidence; nothing else is."""

    def test_ready_is_the_startup_and_volume_done_is_the_rate(
        self, tmp_path: Path
    ) -> None:
        rows = parse_generation_list(
            [{"name": "nova", "engine": "hayai-nova", "primary": True}]
        )
        worker = _worker(tmp_path, rows)
        worker._handle_session_event(
            {"event": "ready", "startup_seconds": 7.5}, rows[0], {}, []
        )
        assert worker.rates.startup(rows[0].id).seconds == 7.5
        assert worker.rates.startup(rows[0].id).source == "session"
        # No volume has finished, so there is still nothing to say about pages.
        assert worker.rates.rate(rows[0].id) is None

    def test_a_finished_volume_becomes_the_next_ones_prediction(
        self, tmp_path: Path
    ) -> None:
        """The owner's bar: the SECOND volume of a session is predicted from
        the first one's real speed, not from a benchmark and never from an
        elapsed time containing a model load."""
        rows = parse_generation_list(
            [{"name": "nova", "engine": "hayai-nova", "primary": True}]
        )
        _library(tmp_path, S=["A", "B"])
        worker = _worker(tmp_path, rows)
        entry = _SessionJob(
            job=(tmp_path / "library" / "S" / "A.cbz", rows[0].id),
            generation=rows[0],
            volume=SessionVolume(
                id="v1",
                workspace=tmp_path / "ws",
                output=tmp_path / "out.mokuro",
                cache_dir=tmp_path / "cache",
                detect_dir=tmp_path / "detect",
                log=tmp_path / "vol.log",
                title="S",
                volume="A",
            ),
        )
        clock = entry.clock
        worker._handle_session_event(
            {"event": "volume_done", "id": "v1", "pages": 120, "seconds": 10.0},
            rows[0],
            {"v1": entry},
            ["v1"],
            clock,
        )
        estimate = worker.rates.rate(rows[0].id)
        assert estimate is not None
        assert estimate.pages_per_second == 12.0
        # The session OPENED with this volume, so its seconds carry the
        # pipeline fill that `startup` charges separately. It is used -- it
        # is the only measurement of this machine there is -- and it says so.
        assert estimate.source == "session (opening volume)"

        # The next volume through the same session is a steady-state sample,
        # and takes the rate over outright.
        second = _SessionJob(
            job=(tmp_path / "library" / "S" / "B.cbz", rows[0].id),
            generation=rows[0],
            volume=SessionVolume(
                id="v2",
                workspace=tmp_path / "ws",
                output=tmp_path / "out2.mokuro",
                cache_dir=tmp_path / "cache",
                detect_dir=tmp_path / "detect",
                log=tmp_path / "vol2.log",
                title="S",
                volume="B",
            ),
            clock=clock,
        )
        worker._handle_session_event(
            {"event": "volume_done", "id": "v2", "pages": 120, "seconds": 6.0},
            rows[0],
            {"v2": second},
            ["v2"],
            clock,
        )
        settled = worker.rates.rate(rows[0].id)
        assert settled is not None
        assert settled.pages_per_second == 20.0
        assert settled.source == "session"
        assert settled.volumes_observed == 2


class TestStatusFields:
    """What `/queue/api/status` now carries, and what it refuses to carry."""

    def test_pending_items_get_a_local_ready_instant_and_the_queue_an_end(
        self, tmp_path: Path
    ) -> None:
        rows = parse_generation_list(
            [{"name": "mokuro", "engine": "mokuro", "primary": True},
             {"name": "nova", "engine": "hayai-nova"}]
        )
        _library(tmp_path, S=["A", "B"])
        worker = _worker(tmp_path, rows, page_count_lookup=lambda cbz: 100)
        # One finished volume is all it takes for the row to have a rate.
        worker.rates.record_volume(rows[1].id, 100, 10.0)
        worker.rates.record_startup(rows[1].id, 4.0)
        control = OcrControl()
        control.worker = worker
        app = QueueAPI(
            _dummy_app,
            storage_base_path=str(tmp_path),
            generations=rows,
            ocr_control=control,
        )
        data = _status(app)

        assert [item["volume"] for item in data["pending_ocr"]] == ["A", "B"]
        first, second = data["pending_ocr"]
        assert first["pages"] == 100
        assert first["rough"] is False
        assert first["rate_source"] == "session"
        # 4 s to open the session, then 10 s a volume.
        assert first["eta_seconds"] == 14
        assert second["eta_seconds"] == 24
        assert first["eta_at"].endswith("Z")
        assert data["queue_done_at"] == second["eta_at"]

    def test_an_unmeasured_row_promises_nothing(self, tmp_path: Path) -> None:
        rows = parse_generation_list(
            [{"name": "mokuro", "engine": "mokuro", "primary": True},
             {"name": "nova", "engine": "hayai-nova"}]
        )
        _library(tmp_path, S=["A"])
        worker = _worker(tmp_path, rows, page_count_lookup=lambda cbz: 100)
        control = OcrControl()
        control.worker = worker
        app = QueueAPI(
            _dummy_app,
            storage_base_path=str(tmp_path),
            generations=rows,
            ocr_control=control,
        )
        data = _status(app)
        assert data["pending_ocr"][0]["eta_at"] is None
        assert data["pending_ocr"][0]["reason"]
        assert data["queue_done_at"] is None

    def test_a_running_volume_is_priced_from_the_progress_file(
        self, tmp_path: Path
    ) -> None:
        rows = parse_generation_list(
            [{"name": "nova", "engine": "hayai-nova", "primary": True}]
        )
        _library(tmp_path, S=["A"])
        worker = _worker(tmp_path, rows)
        worker.rates.record_volume(rows[0].id, 100, 10.0)  # 10 pages/s
        (tmp_path / ".ocr-progress.json").write_text(
            json.dumps(
                {
                    "active": True,
                    "generation": "nova",
                    "generation_id": rows[0].id,
                    "engine": "hayai-nova",
                    "series": "S",
                    "volume": "A",
                    "slot": 0,
                    "done_pages": 40,
                    "total_pages": 100,
                    "first_page_at": time.time() - 4.0,
                    "status": "running",
                }
            ),
            encoding="utf-8",
        )
        control = OcrControl()
        control.worker = worker
        app = QueueAPI(
            _dummy_app, storage_base_path=str(tmp_path), generations=rows,
            ocr_control=control,
        )
        data = _status(app)
        current = data["current"]
        assert current["status"] == "running"
        # 60 pages left; the volume's own 40 pages in 4 s (about 9.75/s)
        # blended with the session's 10/s lands within a second of 6.
        assert 5 <= current["eta_seconds"] <= 7
        assert current["eta_at"].endswith("Z")
        assert current["rate_source"].startswith("session")
        assert data["queue_done_at"] == current["eta_at"]

    def test_a_volume_that_has_emitted_nothing_shows_a_startup_not_an_eta(
        self, tmp_path: Path
    ) -> None:
        rows = parse_generation_list(
            [{"name": "nova", "engine": "hayai-nova", "primary": True}]
        )
        _library(tmp_path, S=["A"])
        worker = _worker(tmp_path, rows)
        worker.rates.record_startup(rows[0].id, 20.0)
        (tmp_path / ".ocr-progress.json").write_text(
            json.dumps(
                {
                    "active": True,
                    "generation": "nova",
                    "generation_id": rows[0].id,
                    "series": "S",
                    "volume": "A",
                    "slot": 0,
                    "done_pages": 0,
                    "total_pages": 12,
                    "first_page_at": None,
                    "session_started_at": time.time() - 3.0,
                    "status": "starting",
                }
            ),
            encoding="utf-8",
        )
        control = OcrControl()
        control.worker = worker
        app = QueueAPI(
            _dummy_app, storage_base_path=str(tmp_path), generations=rows,
            ocr_control=control,
        )
        current = _status(app)["current"]
        assert current["status"] == "starting"
        assert current["eta_at"] is None
        assert current["eta_seconds"] is None
        assert 16 <= current["startup_seconds"] <= 17


class TestEarliestFinishLanes:
    """`plan_queue` gives each item to the lane that finishes it first."""

    def test_a_slow_lane_still_takes_what_the_fast_one_cannot_reach(self) -> None:
        plan = plan_queue(
            [],
            _pending(*[("g-1", f"V{n}", 20) for n in range(8)]),
            lane_count=2,
            lane_machines=["local", "tower"],
            rate_for=_machine_rate_for({("g-1", "local"): 1.0, ("g-1", "tower"): 5.0}),
            startup_for=_machine_startup_for(),
            now=NOON,
        )
        # tower: 4, 8, 12, 16, 20 (the fifth ties this server's 20 s and
        # goes to the lane found first), ...
        etas = [item["eta_seconds"] for item in plan.pending]
        assert 20 in etas, "this server reads one volume tower could not reach sooner"
        assert plan.done_at is not None

    def test_a_lane_with_no_rate_keeps_the_first_come_rule(self) -> None:
        plan = plan_queue(
            [],
            _pending(("g-1", "A", 20)),
            lane_count=2,
            lane_machines=["local", "tower"],
            rate_for=_machine_rate_for({("g-1", "local"): 1.0}),
            startup_for=_machine_startup_for(),
            now=NOON,
        )
        # tower cannot be priced, so it is not chosen by finish time; the
        # lane that frees first (this server's, listed first) takes it.
        assert plan.pending[0]["eta_seconds"] == 20


class TestAVolumesLayers:
    """A layer is an ordinary job: it waits for its volume's primary no more
    than any other queued job does (every sidecar carries the volume's own id,
    so none has to land first)."""

    def test_an_idle_lane_reads_the_layer_while_the_primary_runs(self) -> None:
        running = [{
            "series": "S", "volume": "A", "generation_id": "g-1", "slot": 0,
            "done_pages": 0, "total_pages": 20, "status": "running", "session_ready": True,
        }]
        layer = {"series": "S", "volume": "A", "generation": "g-2", "generation_id": "g-2",
                 "pages": 40}
        plan = plan_queue(
            running,
            [layer],
            lane_count=2,
            rate_for=_rate_for({"g-1": 2.0, "g-2": 4.0}),
            startup_for=_startup_for({"g-1": 0.0, "g-2": 5.0}),
            now=NOON,
        )
        # The primary frees lane 0 at 10 s; lane 1 is idle and starts the
        # layer now: its row's 5 s load, 40 pages at 4/s.
        (planned,) = plan.pending
        assert planned["eta_seconds"] == 15
        assert plan.done_at == iso_utc(NOON + 15)
