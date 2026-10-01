"""The generations list: what a row may be, and what is derived from it.

Every rule here is enforced at config load as well as in the admin API,
because a hand-edited ``config.yaml`` and ``$MOKURO_OCR_GENERATIONS`` both
bypass the API entirely.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from mokuro_bunko.config import Config, OcrConfig
from mokuro_bunko.ocr.devices import DeviceCatalog, GpuDevice
from mokuro_bunko.ocr.generations import (
    GENERATION_NAME_RE,
    GenerationConfigError,
    GenerationSpec,
    default_generations,
    enabled_generations,
    generation_by_id,
    layer_id_of_sidecar,
    mint_generation_id,
    name_rejection,
    parse_bench_spec,
    parse_generation_list,
    primary_generation,
    required_detectors,
    required_engines,
    seed_generation_name,
    sidecar_siblings,
)

MOKURO_ROW = {"name": "mokuro", "engine": "mokuro", "primary": True}


def rows(*extra: dict) -> list[GenerationSpec]:
    """Parse the default primary row plus whatever else the test wants."""
    return parse_generation_list([dict(MOKURO_ROW), *extra])


class TestNameGrammar:
    """A name is a file-name postfix, and the reader's grammar decides it."""

    @pytest.mark.parametrize(
        "name",
        [
            "mokuro",
            "hayai-nova",
            "a",
            "h" * 24,
            "h" * 32,
            "hayai-nova-animetext-attn",  # 25 chars: silently truncated under the old 24 cap
            "p4ddle-2",
            "0abc",
        ],
    )
    def test_accepts_a_usable_postfix(self, name: str) -> None:
        assert name_rejection(name) is None
        assert GENERATION_NAME_RE.match(name)

    @pytest.mark.parametrize(
        ("name", "why"),
        [
            ("Hayai", "uppercase collapses into one layer id on the reader"),
            ("has space", "a space is not in the reader's grammar"),
            ("has_underscore", "an underscore is not in the reader's grammar"),
            ("paddle.v1", "the reader splits the postfix on the LAST dot"),
            ("-leading", "a leading hyphen reads as a flag"),
            ("h" * 33, "33 characters is past the reader's own slug cap of 32"),
            ("カスタム", "non-ASCII is normalised differently by macOS"),
            ("", "a row still needs a label"),
        ],
    )
    def test_rejects_an_unusable_postfix(self, name: str, why: str) -> None:
        assert name_rejection(name) is not None, why

    @pytest.mark.parametrize("name", ["original", "gcv", "updated-ocr", "tr-en", "tr-"])
    def test_rejects_a_name_the_reader_already_owns(self, name: str) -> None:
        rejection = name_rejection(name)
        assert rejection is not None
        assert "reserved" in rejection

    def test_the_error_names_the_row_and_the_field(self) -> None:
        with pytest.raises(GenerationConfigError) as excinfo:
            parse_generation_list([dict(MOKURO_ROW), {"name": "Nope", "engine": "hayai-nova"}])
        error = excinfo.value
        assert error.row == 1
        assert error.field == "name"
        assert "ocr.generations[1]" in str(error)
        assert "'Nope'" in str(error)

    def test_names_are_unique_across_every_row_enabled_or_not(self) -> None:
        with pytest.raises(GenerationConfigError) as excinfo:
            parse_generation_list(
                [
                    dict(MOKURO_ROW),
                    {"name": "twice", "engine": "hayai-nova"},
                    {"name": "twice", "engine": "paddle-manga", "enabled": False},
                ]
            )
        assert excinfo.value.row == 2
        assert excinfo.value.field == "name"
        assert "unique" in str(excinfo.value)

    def test_the_primary_row_still_needs_a_name_of_its_own(self) -> None:
        # Its FILE is the bare one, but the name is still its label and the
        # key its log and its congestion history are read by.
        parsed = parse_generation_list([{"engine": "mokuro", "primary": True}])
        assert parsed[0].name == "mokuro"
        assert parsed[0].sidecar_suffix == ".mokuro"


class TestSeedName:
    """The default name of a NEW row, evaluated once and then stored."""

    def test_an_engine_with_its_own_detector_is_named_after_the_engine(self) -> None:
        assert seed_generation_name("ppocr-manga", "ctd", []) == "ppocr-manga"
        assert seed_generation_name("mokuro", "ctd", []) == "mokuro"
        assert seed_generation_name("mokuro", "ppocr-manga", []) == "mokuro"

    def test_otherwise_the_name_says_engine_and_detector(self) -> None:
        assert seed_generation_name("hayai-nova", "ctd", []) == "hayai-nova-ctd"
        assert seed_generation_name("paddle-manga", "ppocr-manga", []) == "paddle-manga-ppocr-manga"

    def test_a_long_join_is_truncated_into_the_grammar(self) -> None:
        # hayai-nova + animetext is 26 characters, past the 24 cap.
        seeded = seed_generation_name("hayai-nova", "animetext", [])
        assert len(seeded) <= 24
        assert name_rejection(seeded) is None

    def test_a_collision_appends_a_counter(self) -> None:
        taken = ["hayai-nova-ctd"]
        assert seed_generation_name("hayai-nova", "ctd", taken) == "hayai-nova-ctd-2"
        taken.append("hayai-nova-ctd-2")
        assert seed_generation_name("hayai-nova", "ctd", taken) == "hayai-nova-ctd-3"

    def test_a_reserved_seed_is_skipped(self) -> None:
        assert seed_generation_name("hayai-nova", "ctd", ["original"]) == "hayai-nova-ctd"


class TestIds:
    """The id is minted once and keys every piece of internal state."""

    def test_a_row_without_an_id_gets_the_next_free_one(self) -> None:
        parsed = parse_generation_list(
            [dict(MOKURO_ROW), {"engine": "hayai-nova", "detector": "ctd"}]
        )
        assert [row.id for row in parsed] == ["g-1", "g-2"]

    def test_minting_never_reuses_an_id_that_is_taken(self) -> None:
        assert mint_generation_id(["g-1", "g-4"]) == "g-5"
        assert mint_generation_id([]) == "g-1"

    def test_ids_survive_a_reorder_and_a_rename(self) -> None:
        first = parse_generation_list(
            [dict(MOKURO_ROW), {"engine": "hayai-nova", "detector": "ctd"}]
        )
        stored = [row.to_dict() for row in first]
        # Drag the second row to the top and rename it; ids must not move.
        stored[1]["name"] = "renamed"
        reordered = parse_generation_list([stored[1], stored[0]])
        assert [row.id for row in reordered] == ["g-2", "g-1"]
        assert reordered[0].name == "renamed"

    def test_two_rows_cannot_share_an_id(self) -> None:
        with pytest.raises(GenerationConfigError) as excinfo:
            parse_generation_list(
                [
                    {"id": "g-1", **MOKURO_ROW},
                    {"id": "g-1", "name": "other", "engine": "hayai-nova"},
                ]
            )
        assert excinfo.value.field == "id"

    def test_an_id_that_could_escape_a_directory_is_refused(self) -> None:
        # It names this row's workspace cache and detector dumps.
        with pytest.raises(GenerationConfigError) as excinfo:
            parse_generation_list([{"id": "../etc", **MOKURO_ROW}])
        assert excinfo.value.field == "id"


class TestPrimary:
    """Exactly one enabled row writes the bare `<Volume>.mokuro`."""

    def test_the_primary_row_writes_the_bare_sidecar(self) -> None:
        parsed = rows({"name": "second", "engine": "hayai-nova"})
        assert parsed[0].sidecar_suffix == ".mokuro"
        assert parsed[1].sidecar_suffix == ".second.mokuro"
        assert primary_generation(parsed) is parsed[0]

    def test_no_enabled_primary_is_a_hard_error(self) -> None:
        with pytest.raises(GenerationConfigError) as excinfo:
            parse_generation_list([{"name": "only", "engine": "hayai-nova"}])
        assert excinfo.value.field == "primary"
        assert "<Volume>.mokuro" in str(excinfo.value)

    def test_two_enabled_primaries_is_a_hard_error(self) -> None:
        with pytest.raises(GenerationConfigError) as excinfo:
            parse_generation_list(
                [dict(MOKURO_ROW), {"name": "second", "engine": "hayai-nova", "primary": True}]
            )
        assert excinfo.value.row == 1
        assert excinfo.value.field == "primary"

    def test_any_engine_may_be_primary(self) -> None:
        # The hardcoded mokuro exclusivity is gone: what owns the
        # bare file is the flag, not the engine.
        parsed = parse_generation_list(
            [
                {"name": "nova", "engine": "hayai-nova", "detector": "ctd", "primary": True},
                {"name": "mokuro", "engine": "mokuro"},
                {"name": "half", "engine": "mokuro", "precision": "auto-speed"},
            ]
        )
        assert parsed[0].sidecar_suffix == ".mokuro"
        assert parsed[1].sidecar_suffix == ".mokuro.mokuro"
        assert parsed[2].sidecar_suffix == ".half.mokuro"

    def test_a_list_with_nothing_enabled_needs_no_primary(self) -> None:
        parsed = parse_generation_list([{"name": "off", "engine": "hayai-nova", "enabled": False}])
        assert enabled_generations(parsed) == []


class TestEngineAndDetector:
    """The detector is per row, and null when the engine brings its own."""

    def test_an_engine_with_its_own_detector_stores_null(self) -> None:
        parsed = rows({"name": "ppocr", "engine": "ppocr-manga", "detector": "ctd"})
        assert parsed[1].detector is None
        assert parsed[1].effective_detector == "ppocr-manga"
        assert parsed[1].detector_locked is True

    def test_a_mokuro_engine_stores_null_and_takes_the_served_road(self) -> None:
        parsed = parse_generation_list([{**MOKURO_ROW, "detector": "ctd"}])
        assert parsed[0].detector is None
        assert parsed[0].detector_locked is True
        # It detects behind its own command line, so it reports no detector
        # of ours -- but it is a PROCESS pages are streamed into now, so it
        # has a road and stages like every other row.
        assert parsed[0].reported_detector is None
        assert parsed[0].monolithic is False
        assert parsed[0].served is True
        assert parsed[0].mokuro_env is True
        assert parsed[0].road == "served"
        assert parsed[0].stage_keys == ("feed", "mokuro", "post")

    def test_two_rows_may_share_an_engine_with_different_detectors(self) -> None:
        parsed = rows(
            {"name": "nova-ctd", "engine": "hayai-nova", "detector": "ctd"},
            {"name": "nova-ppocr", "engine": "hayai-nova", "detector": "ppocr-manga"},
        )
        assert [row.effective_detector for row in parsed[1:]] == ["ctd", "ppocr-manga"]
        assert parsed[1].sidecar_suffix != parsed[2].sidecar_suffix
        assert required_engines(parsed) == ["mokuro", "hayai-nova"]
        assert required_detectors(parsed) == ("ctd", "ppocr-manga")

    def test_an_unknown_engine_names_the_row_and_the_field(self) -> None:
        with pytest.raises(GenerationConfigError) as excinfo:
            parse_generation_list([dict(MOKURO_ROW), {"name": "x", "engine": "nope"}])
        assert (excinfo.value.row, excinfo.value.field) == (1, "engine")

    def test_an_unknown_detector_names_the_row_and_the_field(self) -> None:
        with pytest.raises(GenerationConfigError) as excinfo:
            parse_generation_list(
                [dict(MOKURO_ROW), {"name": "x", "engine": "hayai-nova", "detector": "nope"}]
            )
        assert (excinfo.value.row, excinfo.value.field) == (1, "detector")

    def test_an_unknown_patch_budget_names_its_field(self) -> None:
        with pytest.raises(GenerationConfigError) as excinfo:
            parse_generation_list([{**MOKURO_ROW, "patch_budget": 1024}])
        assert excinfo.value.field == "patch_budget"

    def test_patch_budget_applies_only_where_the_recognizer_has_the_knob(self) -> None:
        parsed = rows(
            {"name": "nova", "engine": "hayai-nova"},
            {"name": "paddle", "engine": "paddle-manga"},
        )
        assert [row.patch_budget_applies for row in parsed] == [False, True, False]


class TestPools:
    """Pool sizes are per row, and their keys are that row's stage keys."""

    def test_the_stage_keys_come_from_the_runner_road(self) -> None:
        parsed = rows(
            {"name": "nova", "engine": "hayai-nova", "detector": "ctd"},
            {"name": "recon", "engine": "hayai-nova", "detector": "ppocr-manga"},
            {"name": "line", "engine": "ppocr-manga"},
        )
        assert parsed[1].road == "adapter"
        assert parsed[2].road == "reconciled"
        assert parsed[3].road == "line"
        # The keys are read from STAGE_GRAPHS, never hardcoded here: a road's
        # first stage has been renamed before and may be again.
        assert len(parsed[1].stage_keys) == 3
        assert "engine" in parsed[2].stage_keys

    def test_a_stage_of_another_road_is_refused(self) -> None:
        with pytest.raises(GenerationConfigError) as excinfo:
            rows(
                {
                    "name": "line",
                    "engine": "ppocr-manga",
                    "pools": {"stage_workers": {"engine": 2}},
                }
            )
        assert excinfo.value.field == "pools"
        assert "engine" in str(excinfo.value)

    def test_a_mokuro_row_sizes_the_stages_of_the_served_road(self) -> None:
        """Three stages with queues between them: mokuro goes through the runner.

        The fork's own page pipeline is ONE of the three (``mokuro``, where
        the Workers cell is its ``--num_workers``), with a ``feed`` and a
        ``post`` pool of ours either side of it.
        """
        parsed = parse_generation_list(
            [
                {
                    **MOKURO_ROW,
                    "pools": {
                        "stage_workers": {"mokuro": 4, "feed": 2},
                        "queue_capacity": {"post": 3},
                    },
                }
            ]
        )
        assert parsed[0].road == "served"
        assert parsed[0].pool_stage_keys == ("feed", "mokuro", "post")
        assert parsed[0].pools.stage_workers == {"mokuro": 4, "feed": 2}
        assert parsed[0].pools.queue_capacity == {"post": 3}

    def test_a_mokuro_row_may_not_name_a_stage_of_another_road(self) -> None:
        with pytest.raises(GenerationConfigError) as excinfo:
            parse_generation_list([{**MOKURO_ROW, "pools": {"stage_workers": {"detect": 2}}}])
        assert excinfo.value.field == "pools"
        assert "feed, mokuro, post" in str(excinfo.value)

    def test_only_the_mokuro_stage_of_a_served_row_takes_a_device(self) -> None:
        """Addendum 7 through Addendum 8: the serve process IS the one model.

        Its device is what that process is started with (``--force_cpu`` /
        ``CUDA_VISIBLE_DEVICES``), so the row keeps its Device select on the
        served road exactly as a monolithic row had it.
        """
        parsed = parse_generation_list(
            [{**MOKURO_ROW, "pools": {"stage_device": {"mokuro": "cpu"}}}]
        )
        assert parsed[0].device_stage_keys == ("mokuro",)
        assert parsed[0].pools.stage_device == {"mokuro": "cpu"}

    @pytest.mark.parametrize("stage", ["detect", "engine", "feed", "post"])
    def test_a_served_row_refuses_a_device_for_any_other_stage(self, stage: str) -> None:
        """``feed``/``post`` hold no model; ``detect``/``engine`` are not its stages."""
        with pytest.raises(GenerationConfigError) as excinfo:
            parse_generation_list([{**MOKURO_ROW, "pools": {"stage_device": {stage: "cpu"}}}])
        assert excinfo.value.field == "pools"
        assert "this row's are mokuro" in str(excinfo.value)

    def test_an_unknown_pool_setting_is_refused(self) -> None:
        with pytest.raises(GenerationConfigError) as excinfo:
            rows({"name": "nova", "engine": "hayai-nova", "pools": {"threads": {}}})
        assert excinfo.value.field == "pools"

    @pytest.mark.parametrize(
        ("pool", "value"),
        [("stage_workers", -1), ("stage_workers", 999), ("queue_capacity", 0)],
    )
    def test_an_absurd_size_is_refused(self, pool: str, value: int) -> None:
        with pytest.raises(GenerationConfigError) as excinfo:
            rows(
                {
                    "name": "nova",
                    "engine": "hayai-nova",
                    "detector": "ctd",
                    "pools": {pool: {"engine": value}},
                }
            )
        assert excinfo.value.field == "pools"

    def test_empty_pools_mean_derive(self) -> None:
        parsed = rows({"name": "nova", "engine": "hayai-nova"})
        assert parsed[1].pools.is_empty()


class TestStageDevice:
    """``pools.stage_device``: which device each model-bearing stage sits on."""

    def test_a_device_is_stored_per_stage_and_absent_means_auto(self) -> None:
        parsed = rows(
            {
                "name": "nova",
                "engine": "hayai-nova",
                "detector": "ctd",
                "pools": {"stage_device": {"detect": "cpu"}},
            }
        )
        assert parsed[1].pools.stage_device == {"detect": "cpu"}
        # The engine stage is simply absent: that IS auto.
        assert "engine" not in parsed[1].pools.stage_device

    def test_torchs_own_spelling_is_accepted_and_normalised(self) -> None:
        """The runner takes ``cuda:1``; what is STORED is bunko's ``gpu:1``."""
        parsed = rows(
            {
                "name": "nova",
                "engine": "hayai-nova",
                "detector": "ctd",
                "pools": {"stage_device": {"engine": "cuda:1"}},
            }
        )
        assert parsed[1].pools.stage_device == {"engine": "gpu:1"}

    @pytest.mark.parametrize("stage", ["post", "layout"])
    def test_a_stage_with_no_model_takes_no_device(self, stage: str) -> None:
        with pytest.raises(GenerationConfigError) as excinfo:
            rows(
                {
                    "name": "nova",
                    "engine": "hayai-nova",
                    "detector": "ctd",
                    "pools": {"stage_device": {stage: "cpu"}},
                }
            )
        assert excinfo.value.field == "pools"
        assert "holding a model" in str(excinfo.value)

    def test_a_device_that_is_not_a_device_is_refused(self) -> None:
        with pytest.raises(GenerationConfigError) as excinfo:
            rows(
                {
                    "name": "nova",
                    "engine": "hayai-nova",
                    "pools": {"stage_device": {"engine": "gpu:zero"}},
                }
            )
        assert excinfo.value.field == "pools"

    def test_an_index_this_server_does_not_have_is_refused(self) -> None:
        catalog = DeviceCatalog(gpus=(GpuDevice(index=0, name="RX 9070 XT"),), probed=True)
        with pytest.raises(GenerationConfigError) as excinfo:
            parse_generation_list(
                [
                    dict(MOKURO_ROW),
                    {
                        "name": "nova",
                        "engine": "hayai-nova",
                        "pools": {"stage_device": {"engine": "gpu:1"}},
                    },
                ],
                devices=catalog,
            )
        assert "reports 1 GPU" in str(excinfo.value)

    def test_a_server_that_has_not_looked_refuses_nothing(self) -> None:
        """An unprobed catalog knows cpu; it does not know there is no card."""
        parsed = parse_generation_list(
            [
                dict(MOKURO_ROW),
                {
                    "name": "nova",
                    "engine": "hayai-nova",
                    "pools": {"stage_device": {"engine": "gpu:1"}},
                },
            ],
            devices=DeviceCatalog(),
        )
        assert parsed[1].pools.stage_device == {"engine": "gpu:1"}

    def test_a_cpu_only_model_refuses_a_card_and_says_why(self) -> None:
        with pytest.raises(GenerationConfigError) as excinfo:
            rows(
                {
                    "name": "line",
                    "engine": "ppocr-manga",
                    "pools": {"stage_device": {"detect": "gpu:0"}},
                }
            )
        assert excinfo.value.field == "pools"
        assert "onnxruntime" in str(excinfo.value)

    def test_a_cpu_only_detector_under_another_engine_refuses_a_card_too(self) -> None:
        with pytest.raises(GenerationConfigError) as excinfo:
            rows(
                {
                    "name": "recon",
                    "engine": "hayai-nova",
                    "detector": "ppocr-manga",
                    "pools": {"stage_device": {"detect": "gpu:0"}},
                }
            )
        assert "onnxruntime" in str(excinfo.value)
        # Its engine is free to stay on the card.
        parsed = rows(
            {
                "name": "recon",
                "engine": "hayai-nova",
                "detector": "ppocr-manga",
                "pools": {"stage_device": {"detect": "cpu", "engine": "gpu:0"}},
            }
        )
        assert parsed[1].pools.stage_device == {"detect": "cpu", "engine": "gpu:0"}

    def test_a_monolithic_rows_device_is_its_one_stage(self) -> None:
        parsed = parse_generation_list(
            [{**MOKURO_ROW, "pools": {"stage_device": {"mokuro": "gpu:0"}}}]
        )
        assert parsed[0].pools.stage_device == {"mokuro": "gpu:0"}
        with pytest.raises(GenerationConfigError) as excinfo:
            parse_generation_list(
                [{**MOKURO_ROW, "pools": {"stage_device": {"engine": "gpu:0"}}}]
            )
        assert "mokuro" in str(excinfo.value)

    def test_it_survives_a_round_trip(self) -> None:
        parsed = rows(
            {
                "name": "nova",
                "engine": "hayai-nova",
                "detector": "ctd",
                "pools": {"stage_device": {"detect": "cpu", "engine": "gpu:0"}},
            }
        )
        again = parse_generation_list([row.to_dict() for row in parsed])
        assert again[1].pools.to_dict() == parsed[1].pools.to_dict()

    def test_a_bench_spec_is_held_to_the_same_rule(self) -> None:
        spec = parse_bench_spec(
            {
                "engine": "hayai-nova",
                "detector": "ctd",
                "pools": {"stage_device": {"detect": "cpu"}},
            }
        )
        assert spec.pools.stage_device == {"detect": "cpu"}
        with pytest.raises(GenerationConfigError) as excinfo:
            parse_bench_spec(
                {"engine": "hayai-nova", "detector": "ctd", "pools": {"stage_device": {"post": "cpu"}}}
            )
        assert excinfo.value.field == "pools"
        assert excinfo.value.row is None

    def test_a_device_never_changes_what_is_written(self) -> None:
        """So a device edit does not cancel a running job (Addendum 7)."""
        plain = rows({"name": "nova", "engine": "hayai-nova", "detector": "ctd"})[1]
        moved = rows(
            {
                "name": "nova",
                "engine": "hayai-nova",
                "detector": "ctd",
                "pools": {"stage_device": {"detect": "cpu"}},
            }
        )[1]
        assert plain.output_affecting() == moved.output_affecting()


class TestRoundTrip:
    """What is stored is exactly what is read back."""

    def test_to_dict_parses_back_identically(self) -> None:
        parsed = rows(
            {
                "name": "nova",
                "engine": "hayai-nova",
                "detector": "ctd",
                "patch_budget": 256,
                "pools": {"stage_workers": {"engine": 1}, "queue_capacity": {"post": 4}},
            },
            {"name": "off", "engine": "paddle-manga", "enabled": False},
        )
        stored = [row.to_dict() for row in parsed]
        assert [row.to_dict() for row in parse_generation_list(stored)] == stored

    def test_json_text_is_accepted_for_the_environment_and_the_cli(self) -> None:
        text = json.dumps([MOKURO_ROW, {"name": "nova", "engine": "hayai-nova"}])
        assert [row.name for row in parse_generation_list(text)] == ["mokuro", "nova"]

    def test_an_absent_list_is_one_primary_mokuro_row(self) -> None:
        assert [row.to_dict() for row in parse_generation_list(None)] == [
            row.to_dict() for row in default_generations()
        ]
        assert parse_generation_list([])[0].name == "mokuro"

    def test_the_config_round_trips_through_yaml_shape(self) -> None:
        config = Config()
        config.ocr = OcrConfig(
            generations=rows({"name": "nova", "engine": "hayai-nova", "detector": "ctd"})
        )
        assert Config.from_dict(config.to_dict()).to_dict() == config.to_dict()

    def test_generation_by_id_finds_a_disabled_row_too(self) -> None:
        parsed = rows({"name": "off", "engine": "hayai-nova", "enabled": False})
        assert generation_by_id(parsed, "g-2") is parsed[1]
        assert generation_by_id(parsed, "g-9") is None


class TestRetiredKeys:
    """The pre-generations OCR keys fail at load, by name."""

    @pytest.mark.parametrize(
        ("key", "value"),
        [
            ("engines", ["mokuro"]),
            ("detector", "ctd"),
            ("patch_budget", 256),
        ],
    )
    def test_an_old_key_is_refused_with_one_clear_sentence(self, key: str, value: object) -> None:
        with pytest.raises(ValueError) as excinfo:
            Config.from_dict({"ocr": {key: value}})
        message = str(excinfo.value)
        assert f"ocr.{key}" in message
        assert "ocr.generations" in message

    def test_the_message_names_every_old_key_present(self) -> None:
        with pytest.raises(ValueError) as excinfo:
            Config.from_dict({"ocr": {"engines": ["mokuro"], "patch_budget": 256}})
        assert "ocr.engines" in str(excinfo.value)
        assert "ocr.patch_budget" in str(excinfo.value)


class TestRemovedCharMapField:
    """A row still carrying ``char_map`` is refused, never quietly dropped.

    The character-map system is gone, not moved: there is no mode to fall
    back to and no field to migrate the value into. Ignoring the key would
    start a run that does not do what the stored row says, and -- because
    the field was part of what a row's output depends on -- the operator
    would be left believing the placement setting they saved is in force.
    """

    def test_a_row_carrying_it_names_the_row_and_the_field(self) -> None:
        with pytest.raises(GenerationConfigError) as excinfo:
            parse_generation_list(
                [dict(MOKURO_ROW), {"name": "x", "engine": "hayai-nova", "char_map": "attn"}]
            )
        assert (excinfo.value.row, excinfo.value.field) == (1, "char_map")
        assert "char_map was removed with the character-map system" in str(excinfo.value)

    def test_a_null_value_is_refused_too(self) -> None:
        # A `null` means "not set" for the fields that survive, so an admin
        # UI that still sends every key would sail past a `None` check and
        # keep shipping a field the server no longer has. The key's PRESENCE
        # is what is refused.
        with pytest.raises(GenerationConfigError) as excinfo:
            parse_generation_list([{**MOKURO_ROW, "char_map": None}])
        assert (excinfo.value.row, excinfo.value.field) == (0, "char_map")

    def test_a_bench_spec_carrying_it_is_refused_with_no_row(self) -> None:
        # A spec is not a row in a list, so it never gets an index -- the
        # same shape every other bench-spec refusal has.
        with pytest.raises(GenerationConfigError) as excinfo:
            parse_bench_spec({"engine": "hayai-nova", "char_map": "attn"})
        assert excinfo.value.row is None
        assert excinfo.value.field == "char_map"

    def test_the_field_is_gone_from_a_parsed_row_and_its_stored_shape(self) -> None:
        # Nothing reads it back, so nothing can quietly reintroduce it: not
        # the dataclass, not what is written to config, and not the tuple a
        # live settings change compares to decide whether a running job dies.
        (row,) = parse_generation_list([dict(MOKURO_ROW)])
        assert not hasattr(row, "char_map")
        assert "char_map" not in row.to_dict()
        # engine, effective detector, patch budget (None: mokuro has no knob).
        assert row.output_affecting() == ("mokuro", "ppocr-manga", None)


class TestSidecarSiblings:
    """What goes with an archive when it is deleted."""

    def test_every_layer_beside_the_archive_is_swept(self, tmp_path) -> None:
        stem = tmp_path / "Volume 01"
        for name in (
            "Volume 01.cbz",
            "Volume 01.mokuro",
            "Volume 01.hayai-nova.mokuro",
            "Volume 01.paddle-manga.mokuro.gz",
            "Volume 01.tr-en.mokuro",
            "Volume 01.webp",
            "Volume 01.nocover",
        ):
            (tmp_path / name).write_text("{}", encoding="utf-8")
        # Files that are NOT this volume's, or not a layer id the reader reads.
        (tmp_path / "Volume 02.mokuro").write_text("{}", encoding="utf-8")
        (tmp_path / "Volume 01.backup.v1.mokuro").write_text("{}", encoding="utf-8")

        swept = {path.name for path in sidecar_siblings(stem.with_suffix(".cbz"))}
        assert "Volume 01.mokuro" in swept
        assert "Volume 01.hayai-nova.mokuro" in swept
        assert "Volume 01.paddle-manga.mokuro.gz" in swept
        # A translation layer a reader pushed is still this volume's file.
        assert "Volume 01.tr-en.mokuro" in swept
        assert "Volume 01.webp" in swept
        assert "Volume 01.nocover" in swept
        assert "Volume 02.mokuro" not in swept
        assert "Volume 01.backup.v1.mokuro" not in swept

    def test_a_custom_generation_name_is_swept_without_any_registry(self, tmp_path) -> None:
        # The whole point: nothing here knows what is configured.
        (tmp_path / "Vol 1.cbz").write_text("", encoding="utf-8")
        (tmp_path / "Vol 1.my-own-name.mokuro").write_text("{}", encoding="utf-8")
        swept = {path.name for path in sidecar_siblings(tmp_path / "Vol 1.cbz")}
        assert "Vol 1.my-own-name.mokuro" in swept

    def test_the_bare_sidecar_is_not_a_layer(self) -> None:
        assert layer_id_of_sidecar("Vol 1.mokuro", "Vol 1") is None
        assert layer_id_of_sidecar("Vol 1.gcv.mokuro", "Vol 1") == "gcv"
        assert layer_id_of_sidecar("Vol 1.gcv.mokuro.gz", "Vol 1") == "gcv"
        assert layer_id_of_sidecar("Vol 1.Cap.mokuro", "Vol 1") is None
        assert layer_id_of_sidecar("Other.gcv.mokuro", "Vol 1") is None


class TestLayerOrAnotherVolumesPrimary:
    """`<stem>.<id>.mokuro` is ambiguous by name alone; an archive settles it."""

    def test_a_decimal_volumes_primary_is_not_a_layer_of_the_whole_numbered_one(
        self, tmp_path: Path
    ) -> None:
        from mokuro_bunko.ocr.generations import sidecar_siblings

        series = tmp_path / "Series"
        series.mkdir()
        for name in (
            "Volume 01.cbz",
            "Volume 01.mokuro",
            "Volume 01.hayai-nova-ctd.mokuro",
            "Volume 01.5.cbz",
            "Volume 01.5.mokuro",  # reads as layer "5" of Volume 01 -- it is NOT
            "Volume 01.5.hayai-nova-ctd.mokuro",
        ):
            (series / name).write_text("{}", encoding="utf-8")

        doomed = {p.name for p in sidecar_siblings(series / "Volume 01.cbz") if p.exists()}

        assert doomed == {"Volume 01.mokuro", "Volume 01.hayai-nova-ctd.mokuro"}
        # and the other way round still sweeps everything that IS volume 1.5's
        doomed_half = {p.name for p in sidecar_siblings(series / "Volume 01.5.cbz") if p.exists()}
        assert doomed_half == {"Volume 01.5.mokuro", "Volume 01.5.hayai-nova-ctd.mokuro"}

    def test_without_that_archive_the_same_name_is_a_layer(self, tmp_path: Path) -> None:
        from mokuro_bunko.ocr.generations import sidecar_siblings

        series = tmp_path / "Series"
        series.mkdir()
        for name in ("Volume 01.cbz", "Volume 01.mokuro", "Volume 01.5.mokuro"):
            (series / name).write_text("{}", encoding="utf-8")
        doomed = {p.name for p in sidecar_siblings(series / "Volume 01.cbz") if p.exists()}
        assert doomed == {"Volume 01.mokuro", "Volume 01.5.mokuro"}

    def test_archive_extensions_match_the_processors(self) -> None:
        from mokuro_bunko.ocr.generations import VOLUME_ARCHIVE_EXTENSIONS
        from mokuro_bunko.ocr.processor import SUPPORTED_EXTENSIONS

        assert set(VOLUME_ARCHIVE_EXTENSIONS) == set(SUPPORTED_EXTENSIONS)


class TestNullMeansNotSet:
    """A client has nothing to put in a field its row's engine does not use."""

    def test_a_null_patch_budget_is_the_default_not_an_error(self) -> None:
        from mokuro_bunko.ocr.generations import parse_generation_list

        rows = parse_generation_list(
            [
                {"name": "mokuro", "engine": "mokuro", "primary": True},
                {
                    "name": "paddle-manga-ctd",
                    "engine": "paddle-manga",
                    "detector": "ctd",
                    "patch_budget": None,
                },
            ]
        )
        assert [row.patch_budget for row in rows] == [512, 512]

    def test_a_wrong_value_is_still_refused(self) -> None:
        from mokuro_bunko.ocr.generations import GenerationConfigError, parse_generation_list

        with pytest.raises(GenerationConfigError) as caught:
            parse_generation_list(
                [
                    {"name": "mokuro", "engine": "mokuro", "primary": True},
                    {"name": "nova", "engine": "hayai-nova", "detector": "ctd", "patch_budget": 300},
                ]
            )
        assert caught.value.field == "patch_budget"
        assert caught.value.row == 1


class TestBenchSpec:
    """`parse_bench_spec`: a benchmark's spec, validated like a saved row.

    ``name``/``primary``/``enabled``/``id`` are irrelevant to a benchmark and
    are ignored even when sent; everything else is refused for exactly the
    reasons a `PUT` row would be, with ``row`` always None (a spec is not a
    row in a list).
    """

    def test_a_minimal_spec_seeds_its_own_name_and_ignores_identity_fields(self) -> None:
        spec = parse_bench_spec(
            {
                "engine": "hayai-nova",
                "detector": "ctd",
                "name": "ignored",
                "primary": True,
                "enabled": False,
                "id": "ignored-too",
            }
        )
        assert spec.engine == "hayai-nova"
        assert spec.effective_detector == "ctd"
        # Seeded like a fresh row's default name would be -- never the
        # identity fields the caller sent, which a benchmark has no use for.
        assert spec.name != "ignored"
        assert spec.primary is False
        assert spec.enabled is True
        assert name_rejection(spec.name) is None

    def test_patch_budget_defaults_and_validates_like_a_row(self) -> None:
        spec = parse_bench_spec({"engine": "hayai-nova"})
        assert spec.patch_budget == 512
        with pytest.raises(GenerationConfigError) as excinfo:
            parse_bench_spec({"engine": "paddle-manga", "patch_budget": 1024})
        assert excinfo.value.row is None
        assert excinfo.value.field == "patch_budget"

    def test_a_null_patch_budget_means_not_set(self) -> None:
        spec = parse_bench_spec({"engine": "hayai-nova", "patch_budget": None})
        assert spec.patch_budget == 512

    def test_pools_are_validated_against_the_spec_s_own_road(self) -> None:
        spec = parse_bench_spec(
            {"engine": "hayai-nova", "detector": "ctd", "pools": {"stage_workers": {"detect": 4}}}
        )
        assert spec.pools.stage_workers == {"detect": 4}
        with pytest.raises(GenerationConfigError) as excinfo:
            parse_bench_spec(
                {"engine": "hayai-nova", "pools": {"stage_workers": {"not-a-stage": 2}}}
            )
        assert excinfo.value.row is None
        assert excinfo.value.field == "pools"

    def test_a_mokuro_spec_needs_only_the_engine(self) -> None:
        spec = parse_bench_spec({"engine": "mokuro"})
        assert spec.served is True
        assert spec.monolithic is False
        assert spec.stage_keys == ("feed", "mokuro", "post")

    def test_an_unknown_engine_or_detector_names_its_field_and_no_row(self) -> None:
        with pytest.raises(GenerationConfigError) as excinfo:
            parse_bench_spec({"engine": "not-a-real-engine"})
        assert excinfo.value.row is None
        assert excinfo.value.field == "engine"
        with pytest.raises(GenerationConfigError) as excinfo:
            parse_bench_spec({"engine": "hayai-nova", "detector": "not-a-real-detector"})
        assert excinfo.value.row is None
        assert excinfo.value.field == "detector"

    def test_a_non_mapping_spec_is_refused(self) -> None:
        with pytest.raises(GenerationConfigError):
            parse_bench_spec(["engine", "hayai-nova"])
        with pytest.raises(GenerationConfigError):
            parse_bench_spec(None)


class TestAdminStagesMatchTheRunner:
    """The admin's derived stages are what the runner will actually run."""

    def test_a_gpu_detector_is_shown_on_the_card_at_width_one(self) -> None:
        from mokuro_bunko.admin.api import _stage_rows
        from mokuro_bunko.ocr.generations import parse_generation_list

        (row,) = parse_generation_list(
            [{"name": "hayai-nova-ctd", "engine": "hayai-nova", "detector": "ctd", "primary": True}]
        )
        with_gpu = {s["key"]: s for s in _stage_rows(row, budget=3, gpu=True)}
        without = {s["key"]: s for s in _stage_rows(row, budget=3, gpu=False)}
        # ctd puts its model on the card when there is one: one process, not
        # a CPU pool of three -- which is what the page showed while the
        # runner's own log said "detect (gpu x1)".
        assert with_gpu["detect"]["device"] == "gpu:0"
        assert with_gpu["detect"]["derived_workers"] == 1
        assert without["detect"]["device"] == "cpu"
        assert without["detect"]["derived_workers"] >= 1
        # No stage but `engine` holds a model on any road: post assembles the
        # page and writes JSON, so it stays on the CPU even when the detector
        # took the card.
        assert with_gpu["post"]["device"] == "cpu"

    def test_a_cpu_detector_road_is_unchanged_by_the_card(self) -> None:
        from mokuro_bunko.admin.api import _stage_rows
        from mokuro_bunko.ocr.generations import parse_generation_list

        (row,) = parse_generation_list(
            [{"name": "ppocr-manga", "engine": "ppocr-manga", "primary": True}]
        )
        assert [s["device"] for s in _stage_rows(row, budget=3, gpu=True)] == ["cpu", "cpu"]
