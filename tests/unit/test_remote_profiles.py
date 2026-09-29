"""`processors/<name>.json`: what a row costs on a particular machine."""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from mokuro_bunko.ocr.remote.profiles import ProcessorProfiles, profiles_dir
from mokuro_bunko.ocr.remote.protocol import PROTOCOL_VERSION


def test_a_new_profile_is_written_where_the_spec_says(tmp_path: Path) -> None:
    profiles = ProcessorProfiles(tmp_path)
    profiles.set_identity("tower", host={"gpu": "RTX 4090"},
                          catalog={"engines": ["mokuro"]})
    path = profiles_dir(tmp_path) / "tower.json"
    assert path.is_file()
    body = json.loads(path.read_text(encoding="utf-8"))
    assert body["name"] == "tower"
    assert body["host"]["gpu"] == "RTX 4090"
    assert body["catalog"]["engines"] == ["mokuro"]
    assert body["rows"] == {}


def test_pools_bench_and_runs_live_under_the_row_id(tmp_path: Path) -> None:
    profiles = ProcessorProfiles(tmp_path)
    profiles.set_pools("tower", "g-2", {"stage_workers": {"detect": 3},
                                        "queue_capacity": {},
                                        "stage_device": {"engine": "gpu:0"}})
    profiles.set_bench("tower", "g-2", {"pages_per_second": 41.2, "window_seconds": 24.1,
                                        "gpu_busy_pct": 88, "at": "2026-09-22T00:00:00Z"})
    row = profiles.row("tower", "g-2")
    assert row is not None
    assert row.pools["stage_workers"] == {"detect": 3}
    assert row.bench is not None and row.bench["pages_per_second"] == 41.2


def test_a_finished_volume_accumulates_evidence_not_a_model(tmp_path: Path) -> None:
    profiles = ProcessorProfiles(tmp_path)
    profiles.record_run("tower", "g-2", pages=200, seconds=10.0, congestion=None)
    profiles.record_run("tower", "g-2", pages=100, seconds=10.0, congestion=None)
    row = profiles.row("tower", "g-2")
    assert row is not None
    assert row.runs["volumes"] == 2
    assert row.runs["pages"] == 300
    assert row.runs["seconds"] == pytest.approx(20.0)
    assert row.runs["pages_per_second"] == pytest.approx(15.0)


def test_a_run_that_cannot_make_a_rate_is_not_one(tmp_path: Path) -> None:
    profiles = ProcessorProfiles(tmp_path)
    profiles.record_run("tower", "g-2", pages=0, seconds=10.0, congestion=None)
    profiles.record_run("tower", "g-2", pages=10, seconds=0.0, congestion=None)
    assert profiles.row("tower", "g-2") is None


def test_only_the_last_few_runs_congestion_is_kept(tmp_path: Path) -> None:
    profiles = ProcessorProfiles(tmp_path, keep_runs=3)
    for n in range(6):
        profiles.record_run(
            "tower", "g-2", pages=10, seconds=1.0,
            congestion={"bottleneck": f"stage-{n}", "stages": [], "queues": []},
        )
    row = profiles.row("tower", "g-2")
    assert row is not None
    assert [entry["bottleneck"] for entry in row.runs["congestion"]] == [
        "stage-3", "stage-4", "stage-5"
    ]


def test_an_unknown_processor_or_row_reads_as_nothing(tmp_path: Path) -> None:
    profiles = ProcessorProfiles(tmp_path)
    assert profiles.row("nobody", "g-2") is None
    profiles.set_identity("tower", host={}, catalog={})
    assert profiles.row("tower", "g-9") is None


def test_a_deleted_row_is_pruned_from_every_profile(tmp_path: Path) -> None:
    profiles = ProcessorProfiles(tmp_path)
    profiles.set_pools("tower", "g-2", {"stage_workers": {}})
    profiles.set_pools("tower", "g-3", {"stage_workers": {}})
    profiles.set_pools("box", "g-3", {"stage_workers": {}})
    profiles.prune(["g-2"])
    assert profiles.row("tower", "g-2") is not None
    assert profiles.row("tower", "g-3") is None
    assert profiles.row("box", "g-3") is None


def test_a_name_with_a_separator_in_it_cannot_escape_the_directory(
    tmp_path: Path
) -> None:
    profiles = ProcessorProfiles(tmp_path)
    profiles.set_identity("../../etc/passwd", host={}, catalog={})
    written = list(profiles_dir(tmp_path).glob("*.json"))
    assert len(written) == 1
    assert written[0].parent == profiles_dir(tmp_path)


def test_a_corrupt_profile_reads_as_empty_and_is_rewritten(tmp_path: Path) -> None:
    profiles = ProcessorProfiles(tmp_path)
    profiles.set_identity("tower", host={}, catalog={})
    (profiles_dir(tmp_path) / "tower.json").write_text("{not json", encoding="utf-8")
    assert profiles.load("tower") == {}
    profiles.set_pools("tower", "g-2", {"stage_workers": {"detect": 2}})
    assert profiles.row("tower", "g-2") is not None


# -- the recipe: a row whose output-affecting fields changed has no entry ----


RECIPE = ("hayai-nova", "ctd", 8)


def test_an_entry_measured_for_another_recipe_reads_as_absent(tmp_path: Path) -> None:
    profiles = ProcessorProfiles(tmp_path)
    profiles.set_bench("tower", "g-2", {"pages_per_second": 4.0}, recipe=RECIPE)
    assert profiles.row("tower", "g-2", recipe=RECIPE) is not None
    assert profiles.row("tower", "g-2", recipe=("hayai-nova", "ppocr-manga", 8)) is None
    # Asked without a recipe, it is still there -- for the admin panel.
    assert profiles.row("tower", "g-2") is not None


def test_writing_under_a_new_recipe_starts_the_entry_over(tmp_path: Path) -> None:
    profiles = ProcessorProfiles(tmp_path)
    profiles.set_pools("tower", "g-2", {"stage_workers": {"detect": 6}}, recipe=RECIPE)
    profiles.record_run("tower", "g-2", pages=10, seconds=1.0, congestion=None,
                        recipe=RECIPE)
    changed = ("hayai-nova", "ppocr-manga", 8)
    profiles.record_run("tower", "g-2", pages=20, seconds=1.0, congestion=None,
                        recipe=changed)
    row = profiles.row("tower", "g-2", recipe=changed)
    assert row is not None
    assert row.pools == {}, "pools tuned for another pipeline do not carry over"
    assert row.runs["volumes"] == 1


def test_prune_never_creates_a_profile(tmp_path: Path) -> None:
    ProcessorProfiles(tmp_path).prune(["g-1"])
    assert not profiles_dir(tmp_path).exists()


def test_names_lists_every_profile(tmp_path: Path) -> None:
    profiles = ProcessorProfiles(tmp_path)
    profiles.set_identity("tower", host={}, catalog={})
    profiles.set_identity("box", host={}, catalog={})
    assert profiles.names() == ["box", "tower"]


def test_every_store_in_the_process_writes_under_one_lock(tmp_path: Path) -> None:
    """The worker, the admin API and the register handler each hold a store
    over the same files: a per-instance lock would let two read-modify-writes
    race and lose one of them."""
    assert ProcessorProfiles(tmp_path)._lock is ProcessorProfiles(tmp_path / "x")._lock


def test_a_registration_records_the_machine_in_its_profile(tmp_path: Path) -> None:
    """Spec section 4: the profile carries `host` and `catalog`, so a
    benchmark still reads "on tower (RTX 4090)" once tower is offline."""
    import io

    from mokuro_bunko.ocr.remote.library_api import ProcessorAPI
    from mokuro_bunko.ocr.remote.registry import ProcessorRegistry

    api = ProcessorAPI(lambda e, s: [], ProcessorRegistry(),
                       profiles=ProcessorProfiles(tmp_path))
    body = json.dumps({
        "protocol": PROTOCOL_VERSION, "name": "tower", "host": {"gpu": "RTX 4090"},
        "catalog": {"engines": ["hayai-nova"], "detectors": ["ctd"], "devices": []},
    }).encode()
    environ = {
        "REQUEST_METHOD": "POST", "PATH_INFO": "/_processor/register",
        "CONTENT_LENGTH": str(len(body)), "wsgi.input": io.BytesIO(body),
        "mokuro.role": "processor", "mokuro.username": "tower",
    }
    statuses: list[str] = []
    b"".join(api(environ, lambda status, headers: statuses.append(status)))
    assert statuses[0].startswith("200")
    written = ProcessorProfiles(tmp_path).load("tower")
    assert written["host"] == {"gpu": "RTX 4090"}
    assert written["catalog"]["detectors"] == ["ctd"]


# -- one file per stored name (B3) ---------------------------------------------


@pytest.mark.parametrize(
    "names",
    [
        ("ビースト", "ボックス"),
        ("tower 1", "tower/1", "tower_1"),
        ("Tower", "tower"),
        ("...", "processor"),
    ],
)
def test_names_the_registry_keeps_apart_never_share_a_profile(
    tmp_path: Path, names: tuple[str, ...]
) -> None:
    """The ledger: the profile key is the STORED name. Sanitising it folded
    distinct machines into one file -- one machine's identity, pools, bench
    and runs then read as the other's."""
    from mokuro_bunko.ocr.remote.profiles import profile_filename

    assert len({profile_filename(name) for name in names}) == len(names)
    profiles = ProcessorProfiles(tmp_path)
    for n, name in enumerate(names):
        profiles.set_identity(name, host={"gpu": f"card {n}"}, catalog={})
        profiles.set_pools(name, "g-2", {"stage_device": {"engine": f"gpu:{n}"}})
    for n, name in enumerate(names):
        assert profiles.load(name)["host"] == {"gpu": f"card {n}"}
        row = profiles.row(name, "g-2")
        assert row is not None and row.pools == {"stage_device": {"engine": f"gpu:{n}"}}
    assert sorted(profiles.names()) == sorted(names)


def test_a_plain_name_keeps_its_readable_file(tmp_path: Path) -> None:
    from mokuro_bunko.ocr.remote.profiles import profile_filename

    assert profile_filename("tower") == "tower.json"
    assert profile_filename("box-2.lan") == "box-2.lan.json"


def test_a_long_name_stays_a_short_file_inside_the_directory(tmp_path: Path) -> None:
    profiles = ProcessorProfiles(tmp_path)
    name = "機" * 64
    profiles.set_identity(name, host={}, catalog={})
    (written,) = profiles_dir(tmp_path).glob("*.json")
    assert written.parent == profiles_dir(tmp_path)
    assert len(written.name.encode("utf-8")) < 100
    assert profiles.names() == [name]


def test_a_deleted_row_is_pruned_from_a_digested_profile_too(tmp_path: Path) -> None:
    profiles = ProcessorProfiles(tmp_path)
    profiles.set_pools("ビースト", "g-3", {"stage_workers": {}})
    profiles.prune(["g-2"])
    assert profiles.row("ビースト", "g-3") is None


# -- an entry whose three tables are all empty holds no pools ----------------
#
# What the paddle-manga-animetext auto-benchmarks left on desktop and tower:
# ``{stage_workers: {}, queue_capacity: {}, stage_device: {}}``. Read as pools,
# it replaced the row's own table (``detect: cpu``, ``queue_capacity: {engine:
# 4, post: 4}``) on those machines, and -- counted as pools an admin had saved
# -- no later auto-benchmark could ever replace it.

DEPLOYED_EMPTY = {"stage_workers": {}, "queue_capacity": {}, "stage_device": {}}


def test_an_entry_whose_tables_are_all_empty_holds_no_pools(tmp_path: Path) -> None:
    profiles = ProcessorProfiles(tmp_path)
    profiles.set_pools("desktop", "g-3", dict(DEPLOYED_EMPTY), recipe=RECIPE)
    profiles.set_bench("desktop", "g-3", {"pages_per_second": 1.07}, recipe=RECIPE)
    row = profiles.row("desktop", "g-3", recipe=RECIPE)
    assert row is not None, "the bench is still an entry"
    assert row.pools == {}, "three empty tables are no opinion about this machine"


def test_an_autobench_may_fill_an_entry_whose_tables_are_all_empty(tmp_path: Path) -> None:
    profiles = ProcessorProfiles(tmp_path)
    profiles.set_pools("desktop", "g-3", dict(DEPLOYED_EMPTY), recipe=RECIPE)
    measured = {"stage_workers": {"detect": 3}, "queue_capacity": {"engine": 4},
                "stage_device": {"detect": "cpu"}}
    profiles.set_pools("desktop", "g-3", measured, recipe=RECIPE, keep_existing=True)
    row = profiles.row("desktop", "g-3", recipe=RECIPE)
    assert row is not None and row.pools == measured


def test_keep_existing_still_keeps_a_table_that_says_something(tmp_path: Path) -> None:
    profiles = ProcessorProfiles(tmp_path)
    saved = {"stage_workers": {}, "queue_capacity": {}, "stage_device": {"detect": "auto"}}
    profiles.set_pools("desktop", "g-3", saved, recipe=RECIPE)
    profiles.set_pools("desktop", "g-3", {"stage_workers": {"detect": 3}},
                       recipe=RECIPE, keep_existing=True)
    row = profiles.row("desktop", "g-3", recipe=RECIPE)
    assert row is not None and row.pools == saved
