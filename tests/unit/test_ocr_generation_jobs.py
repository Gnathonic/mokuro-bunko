"""Running a job AS a generation: paths, snapshots and the stamp.

The unit of work is ``(volume, generation)``. Everything a job writes -- the
sidecar, the log, the workspace cache, the record its outcome goes under --
is named after the ROW it was claimed with, not after its engine and not
after whatever the settings say by the time it finishes.
"""

from __future__ import annotations

import json
import zipfile
from pathlib import Path

import pytest

from mokuro_bunko.ocr.generations import GenerationSpec, parse_generation_list
from mokuro_bunko.ocr.processor import MokuroRunResult, OCRProcessor

ROWS = [
    {"name": "mokuro", "engine": "mokuro", "primary": True},
    {"name": "nova-ctd", "engine": "hayai-nova", "detector": "ctd"},
    {"name": "nova-ppocr", "engine": "hayai-nova", "detector": "ppocr-manga"},
]


@pytest.fixture
def storage(tmp_path: Path) -> Path:
    (tmp_path / "library" / "Series").mkdir(parents=True)
    (tmp_path / "inbox").mkdir()
    return tmp_path


@pytest.fixture
def volume(storage: Path) -> Path:
    cbz = storage / "library" / "Series" / "Vol 1.cbz"
    with zipfile.ZipFile(cbz, "w") as archive:
        archive.writestr("page1.jpg", b"not really a jpeg")
    return cbz


def make_processor(storage: Path, rows: list[dict] | None = None) -> OCRProcessor:
    processor = OCRProcessor(
        storage_path=storage,
        python_path=Path("/nonexistent/python"),
        generations=parse_generation_list(rows or ROWS),
        engines_python_path=Path("/nonexistent/engines-python"),
    )
    return processor


def fake_run(
    processor: OCRProcessor,
    monkeypatch: pytest.MonkeyPatch,
    payload: dict | None = None,
    seen: list[tuple[GenerationSpec, Path]] | None = None,
) -> None:
    """Make `_run_engine` write the sidecar the row asks for and succeed."""

    def run(
        generation: GenerationSpec,
        input_path: Path,
        output_dir: Path,
        total_images: int = 0,
        source_cbz: Path | None = None,
    ) -> MokuroRunResult:
        if seen is not None:
            seen.append((generation, output_dir))
        stem = input_path.stem if input_path.is_file() else input_path.name
        # The mokuro CLI writes `<stem>.mokuro` whatever row asked for it;
        # the engine runner writes exactly the path it was handed. WHICH of
        # the two ran is the processor's own question, so that a served row
        # falling back to the CLI is faked the way it really behaves.
        suffix = (
            ".mokuro" if processor.runs_mokuro_cli(generation) else generation.sidecar_suffix
        )
        written = output_dir / f"{stem}{suffix}"
        written.write_text(json.dumps(payload or {"pages": []}), encoding="utf-8")
        return MokuroRunResult(True, None, None)

    monkeypatch.setattr(processor, "_run_engine", run)


class TestOutputPaths:
    def test_each_row_writes_a_file_named_after_it(
        self, storage: Path, volume: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        processor = make_processor(storage)
        fake_run(processor, monkeypatch)
        for row in processor.generations:
            assert processor.process_library_ocr(volume, row)
        names = sorted(p.name for p in volume.parent.glob("*.mokuro"))
        assert names == ["Vol 1.mokuro", "Vol 1.nova-ctd.mokuro", "Vol 1.nova-ppocr.mokuro"]

    def test_two_rows_on_one_engine_do_not_share_a_log(self, storage: Path) -> None:
        processor = make_processor(storage)
        primary, first, second = processor.generations
        volume = storage / "library" / "Series" / "Vol 1.cbz"
        paths = {
            row.name: processor._get_mokuro_log_path(volume, row, volume)
            for row in (primary, first, second)
        }
        assert len({str(path) for path in paths.values()}) == 3
        # The primary row keeps the historical bare name.
        assert paths["mokuro"].name == "Series_Vol 1.log"
        assert paths["nova-ctd"].name == "Series_Vol 1.nova-ctd.log"

    def test_two_rows_on_one_engine_do_not_share_a_workspace_cache(
        self, storage: Path, tmp_path: Path
    ) -> None:
        processor = make_processor(storage)
        _, first, second = processor.generations
        workspace = tmp_path / "ws"
        workspace.mkdir()
        extract = workspace / "Vol 1"
        extract.mkdir()
        commands = [
            processor._engine_runner_command(row, extract, workspace) for row in (first, second)
        ]
        caches = [cmd[cmd.index("--cache-dir") + 1] for cmd in commands]
        stats = [cmd[cmd.index("--stats-file") + 1] for cmd in commands]
        assert caches[0] != caches[1]
        assert stats[0] != stats[1]
        # Keyed by the immutable id, so a rename does not move them either.
        assert first.id in caches[0] and second.id in caches[1]
        # And never under --cache-dir, where the server counts JSON files as
        # finished pages.
        assert not stats[0].startswith(caches[0])

    def test_the_command_carries_the_rows_own_recipe_and_pools(
        self, storage: Path, tmp_path: Path
    ) -> None:
        processor = make_processor(
            storage,
            [
                {"name": "mokuro", "engine": "mokuro", "primary": True},
                {
                    "name": "tuned",
                    "engine": "hayai-nova",
                    "detector": "ctd",
                    "patch_budget": 256,
                    "pools": {
                        "stage_workers": {"detect": 4, "post": 2},
                        "queue_capacity": {"post": 8},
                    },
                },
            ],
        )
        workspace = tmp_path / "ws"
        workspace.mkdir()
        extract = workspace / "Vol 1"
        extract.mkdir()
        cmd = processor._engine_runner_command(processor.generations[1], extract, workspace)
        assert cmd[cmd.index("--engine") + 1] == "hayai-nova"
        assert cmd[cmd.index("--detector") + 1] == "ctd"
        assert cmd[cmd.index("--patches") + 1] == "256"
        assert cmd[cmd.index("--stage-workers") + 1] == "detect=4,post=2"
        assert cmd[cmd.index("--queue-capacity") + 1] == "post=8"

    def test_an_untuned_row_passes_no_pool_flags_at_all(
        self, storage: Path, tmp_path: Path
    ) -> None:
        processor = make_processor(storage)
        workspace = tmp_path / "ws"
        workspace.mkdir()
        extract = workspace / "Vol 1"
        extract.mkdir()
        cmd = processor._engine_runner_command(processor.generations[1], extract, workspace)
        assert "--stage-workers" not in cmd
        assert "--queue-capacity" not in cmd


class TestMidRunRename:
    def test_a_rename_mid_run_lands_under_the_name_it_started_with(
        self, storage: Path, volume: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        processor = make_processor(storage)
        started_as = processor.generations[1]
        fake_run(processor, monkeypatch)

        def rename_midway(*args, **kwargs):
            # What `apply_settings` does to every slot, running or not.
            processor.configure(
                parse_generation_list(
                    [
                        ROWS[0],
                        {"id": started_as.id, "name": "renamed", "engine": "hayai-nova",
                         "detector": "ctd"},
                    ]
                )
            )
            return original(*args, **kwargs)

        original = processor._run_engine
        monkeypatch.setattr(processor, "_run_engine", rename_midway)

        assert processor.process_library_ocr(volume, started_as) is True
        # The finished volume is NOT discarded and no failure is recorded.
        assert (volume.parent / "Vol 1.nova-ctd.mokuro").exists()
        assert not (volume.parent / "Vol 1.renamed.mokuro").exists()
        assert processor.last_failure is None


class TestPrimaryFirst:
    def test_a_volume_owing_the_primary_row_offers_every_row_at_once(
        self, storage: Path, volume: Path
    ) -> None:
        processor = make_processor(storage)
        # Nothing done yet: every row may run now; each sidecar is stamped
        # with the volume's own uuid whichever lands first.
        assert [row.name for row in processor.missing_generations(volume)] == [
            "mokuro", "nova-ctd", "nova-ppocr",
        ]

        (volume.parent / "Vol 1.mokuro").write_text(
            json.dumps({"volume_uuid": "the-one"}), encoding="utf-8"
        )
        assert [row.name for row in processor.missing_generations(volume)] == [
            "nova-ctd",
            "nova-ppocr",
        ]

    def test_without_an_enabled_primary_row_everything_runs(
        self, storage: Path, volume: Path
    ) -> None:
        # A config must always have a primary row, but a HOST may lose it:
        # the server drops rows whose environment failed to install, and a
        # machine that could not install mokuro runs the rest anyway.
        narrowed = [
            row for row in parse_generation_list(ROWS) if not row.primary
        ]
        processor = OCRProcessor(
            storage_path=storage,
            python_path=Path("/nonexistent/python"),
            generations=narrowed,
            engines_python_path=Path("/nonexistent/engines-python"),
        )
        assert [row.name for row in processor.missing_generations(volume)] == [
            "nova-ctd",
            "nova-ppocr",
        ]

    def test_a_secondary_sidecar_inherits_the_primary_uuid(
        self, storage: Path, volume: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        processor = make_processor(storage)
        (volume.parent / "Vol 1.mokuro").write_text(
            json.dumps({"volume_uuid": "the-one"}), encoding="utf-8"
        )
        fake_run(processor, monkeypatch, payload={"pages": [], "volume_uuid": "a-fresh-one"})
        assert processor.process_library_ocr(volume, processor.generations[1])
        written = json.loads((volume.parent / "Vol 1.nova-ctd.mokuro").read_text(encoding="utf-8"))
        assert written["volume_uuid"] == "the-one"

    def test_the_sequential_path_runs_the_primary_row_first(
        self, storage: Path, volume: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # Even when the primary row is LAST in the list: every other row's
        # sidecar inherits its uuid.
        processor = make_processor(
            storage,
            [
                {"name": "nova", "engine": "hayai-nova"},
                {"name": "mokuro", "engine": "mokuro", "primary": True},
            ],
        )
        seen: list[tuple[GenerationSpec, Path]] = []
        fake_run(processor, monkeypatch, seen=seen)
        monkeypatch.setattr(processor, "process_library_thumbnail", lambda path: True)
        processor.process_library_cbz(volume)
        assert [row.name for row, _ in seen] == ["mokuro", "nova"]


class TestOcrEngineStamp:
    """How the reader tells server OCR from a layer somebody edited."""

    def test_a_secondary_layer_is_stamped(
        self, storage: Path, volume: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        processor = make_processor(storage)
        (volume.parent / "Vol 1.mokuro").write_text("{}", encoding="utf-8")
        fake_run(processor, monkeypatch)
        assert processor.process_library_ocr(volume, processor.generations[1])
        written = json.loads((volume.parent / "Vol 1.nova-ctd.mokuro").read_text(encoding="utf-8"))
        assert written["ocr_engine"]["id"] == "hayai-nova"
        assert written["ocr_engine"]["generation"] == "nova-ctd"
        assert "mokuro-bunko" in written["ocr_engine"]["generator"]

    def test_a_monolithic_engine_running_as_a_secondary_row_is_stamped_too(
        self, storage: Path, volume: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # The mokuro CLI writes no ocr_engine block of its own, so without
        # this the layer arrives on every device badged as an edit.
        processor = make_processor(
            storage,
            [
                {"name": "mokuro", "engine": "mokuro", "primary": True},
                {"name": "mk-half", "engine": "mokuro", "precision": "auto-speed"},
            ],
        )
        (volume.parent / "Vol 1.mokuro").write_text("{}", encoding="utf-8")
        fake_run(processor, monkeypatch)
        assert processor.process_library_ocr(volume, processor.generations[1])
        written = json.loads((volume.parent / "Vol 1.mk-half.mokuro").read_text(encoding="utf-8"))
        assert written["ocr_engine"]["id"] == "mokuro"
        assert written["ocr_engine"]["generation"] == "mk-half"
        # A detector is not invented: mokuro detects behind its own CLI.
        assert "detector" not in written["ocr_engine"]

    def test_the_primary_sidecar_stays_pure_upstream_mokuro(
        self, storage: Path, volume: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        processor = make_processor(storage)
        fake_run(processor, monkeypatch)
        assert processor.process_library_ocr(volume, processor.generations[0])
        written = json.loads((volume.parent / "Vol 1.mokuro").read_text(encoding="utf-8"))
        assert "ocr_engine" not in written

    def test_what_the_runner_already_wrote_is_kept(
        self, storage: Path, volume: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        processor = make_processor(storage)
        (volume.parent / "Vol 1.mokuro").write_text("{}", encoding="utf-8")
        fake_run(
            processor,
            monkeypatch,
            payload={
                "pages": [],
                "ocr_engine": {
                    "id": "hayai-nova",
                    "recognizer": "JustANormalTinkerer/hayai-ocr-v2.5-nova",
                    "detector": "ctd",
                    "weights": {"detector": "abc"},
                },
            },
        )
        assert processor.process_library_ocr(volume, processor.generations[1])
        block = json.loads(
            (volume.parent / "Vol 1.nova-ctd.mokuro").read_text(encoding="utf-8")
        )["ocr_engine"]
        assert block["recognizer"] == "JustANormalTinkerer/hayai-ocr-v2.5-nova"
        assert block["weights"] == {"detector": "abc"}
        assert block["generation"] == "nova-ctd"


class TestOsPriority:
    def test_the_first_enabled_row_keeps_normal_priority(self, storage: Path) -> None:
        processor = make_processor(storage)
        first, second, third = processor.generations
        assert processor.is_backlog_generation(first) is False
        assert processor.is_backlog_generation(second) is True
        assert processor.is_backlog_generation(third) is True

    def test_it_follows_the_list_order_and_nothing_else(self, storage: Path) -> None:
        # Reordering the rows moves which job runs at normal priority, from
        # the same function the queue takes its order from.
        processor = make_processor(storage, [ROWS[1], ROWS[0], ROWS[2]])
        assert processor.is_backlog_generation(processor.generations[0]) is False
        assert processor.is_backlog_generation(processor.generations[1]) is True

    def test_a_disabled_first_row_does_not_hold_the_slot(self, storage: Path) -> None:
        processor = make_processor(
            storage,
            [
                {"name": "off", "engine": "paddle-manga", "enabled": False},
                {"name": "mokuro", "engine": "mokuro", "primary": True},
            ],
        )
        assert processor.is_backlog_generation(processor.generations[1]) is False


class TestFailureRecordPruning:
    """A settings change sweeps records of rows that are gone -- only those."""

    def test_a_renamed_rows_record_is_swept(self, storage: Path) -> None:
        from mokuro_bunko.ocr.watcher import OCRWorker

        worker = OCRWorker(storage_path=storage, generations=parse_generation_list(ROWS))
        volume = storage / "library" / "Series" / "Vol 1.cbz"
        volume.write_bytes(b"")
        worker._record_ocr_failure(volume, worker.generations[1])
        assert "Series/Vol 1.cbz@nova-ctd" in worker._load_failures()

        worker.apply_settings(
            parse_generation_list(
                [
                    ROWS[0],
                    {
                        "id": worker.generations[1].id,
                        "name": "renamed",
                        "engine": "hayai-nova",
                        "detector": "ctd",
                    },
                ]
            )
        )
        assert worker._load_failures() == {}

    def test_a_volume_whose_name_contains_an_at_sign_keeps_its_record(
        self, storage: Path
    ) -> None:
        # The primary row's key is the BARE relative path, so splitting a key
        # on '@' would read `1.cbz` as a generation name and delete a real
        # failure -- taking its backoff with it.
        from mokuro_bunko.ocr.watcher import OCRWorker

        worker = OCRWorker(storage_path=storage, generations=parse_generation_list(ROWS))
        volume = storage / "library" / "Series" / "Vol@1.cbz"
        volume.write_bytes(b"")
        worker._record_ocr_failure(volume, worker.generations[0])
        assert "Series/Vol@1.cbz" in worker._load_failures()

        worker.apply_settings(parse_generation_list(ROWS))
        assert "Series/Vol@1.cbz" in worker._load_failures()


class TestClaimingForOneGeneration:
    """The claim can be narrowed to one row without becoming a second queue."""

    def test_a_narrowed_claim_takes_only_that_rows_job(self, storage: Path) -> None:
        from mokuro_bunko.ocr.watcher import OCRWorker

        series = storage / "library" / "Series"
        for index in (1, 2):
            (series / f"Vol {index}.cbz").write_bytes(b"")
            # The primary row is already done, so both secondary rows queue.
            (series / f"Vol {index}.mokuro").write_text("{}", encoding="utf-8")
        worker = OCRWorker(storage_path=storage, generations=parse_generation_list(ROWS))
        _, first, second = worker.generations

        claimed = worker.claim_next(generation_id=second.id)
        assert claimed is not None and claimed[1] == second.id
        # The other row's jobs are untouched and still claimable -- on the
        # same volume too: its generations may run at once.
        other = worker.claim_next(generation_id=first.id)
        assert other is not None and other[1] == first.id
        assert other[0] == claimed[0]

    def test_a_narrowed_claim_does_not_wait_for_the_primary_row(self, storage: Path) -> None:
        from mokuro_bunko.ocr.watcher import OCRWorker

        (storage / "library" / "Series" / "Vol 1.cbz").write_bytes(b"")
        worker = OCRWorker(storage_path=storage, generations=parse_generation_list(ROWS))
        assert worker.claim_next(generation_id=worker.generations[1].id) is not None
        assert worker.claim_next(generation_id=worker.generations[0].id) is not None


class TestFailureRecordsOfVanishedVolumes:
    def test_a_record_whose_archive_is_gone_is_swept_and_a_live_one_kept(
        self, tmp_path: Path
    ) -> None:
        from mokuro_bunko.ocr.generations import parse_generation_list
        from mokuro_bunko.ocr.watcher import OCRWorker

        storage = tmp_path / "storage"
        (storage / "library" / "Alive").mkdir(parents=True)
        (storage / "library" / "Alive" / "Vol 1.cbz").write_bytes(b"x")
        rows = parse_generation_list(
            [
                {"name": "mokuro", "engine": "mokuro", "primary": True},
                {"name": "nova", "engine": "hayai-nova", "detector": "ctd"},
            ]
        )
        worker = OCRWorker(storage_path=storage, generations=rows, sessions=False)
        worker._save_failures(
            {
                "Alive/Vol 1.cbz@nova": {"series": "Alive", "volume": "Vol 1", "generation": "nova"},
                "Gone/Vol 3.cbz@nova": {"series": "Gone", "volume": "Vol 3", "generation": "nova"},
                "Gone/Vol 3.cbz": {"series": "Gone", "volume": "Vol 3", "generation": "mokuro"},
                "mystery": {"generation": "nova"},  # names no volume: kept
            }
        )
        worker._prune_failure_records()
        assert set(worker._load_failures()) == {"Alive/Vol 1.cbz@nova", "mystery"}
