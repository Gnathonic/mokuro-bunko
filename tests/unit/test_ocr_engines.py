"""Unit tests for the OCR engine registry.

**No engine owns a file name.** What a sidecar is called comes from the
GENERATION that produced it, so everything about naming lives in
``test_ocr_generations.py``; what is left here is the registry itself --
which engines exist, what each one brings with it, and what may be asked of
it. The one naming rule that IS the registry's is kept below: an engine id
seeds the name of a row created on it, so an id that could not be a name
would mint sidecars no reader can see.
"""

from __future__ import annotations

import pytest

from mokuro_bunko.ocr.engines import (
    DEFAULT_PATCH_BUDGET,
    DETECTOR_IDS,
    ENGINE_IDS,
    ENGINES,
    PATCH_BUDGETS,
    get_engine,
    get_patch_budget,
    uses_mokuro_env,
    uses_patch_budget,
)
from mokuro_bunko.ocr.generations import name_rejection


class TestRegistry:
    def test_known_engines(self) -> None:
        assert ENGINE_IDS == (
            "mokuro",
            "hayai-nova",
            "paddle-manga",
            "ppocr-manga",
        )
        assert get_engine("mokuro").uses_mokuro_env
        assert not get_engine("hayai-nova").uses_mokuro_env
        assert uses_mokuro_env("mokuro") and not uses_mokuro_env("ppocr-manga")

    def test_every_spec_is_filed_under_its_own_id(self) -> None:
        # The id is what a generation stores and what every lookup goes
        # through; a spec filed under another key would be unreachable.
        assert all(spec.id == engine_id for engine_id, spec in ENGINES.items())

    def test_every_engine_id_is_a_usable_generation_name(self) -> None:
        """A new row is seeded with its engine's id (``seed_generation_name``).

        A name outside the reader's layer grammar makes every sidecar written
        under it an orphan -- silently, forever -- so an engine id that could
        not be a name would break the default row the admin panel creates.
        """
        for engine_id in ENGINE_IDS:
            assert name_rejection(engine_id) is None, engine_id

    def test_a_builtin_detector_is_a_registered_one(self) -> None:
        # ``effective_detector`` returns it verbatim and the installer is
        # asked for its extras by that id: an unregistered one would raise
        # from deep inside a live settings change.
        for spec in ENGINES.values():
            assert spec.detector is None or spec.detector in DETECTOR_IDS

    def test_ppocr_manga_engine(self) -> None:
        spec = get_engine("ppocr-manga")
        assert spec.label == "PP-OCRv6 manga (CTC, CPU)"
        assert spec.recognizer == "Kellenok/PP-OCRv6_manga"
        # It brings its own detector and never borrows the mokuro environment.
        assert (spec.detector, spec.uses_mokuro_env) == ("ppocr-manga", False)

    def test_hayai_nova_is_the_only_hayai_engine(self) -> None:
        """The id keeps its ``-nova`` now that v2 is gone.

        Tidying it to plain ``hayai`` would rename the seed of every row
        created on it, and a renamed row writes a different file: every
        ``Volume.hayai-nova.mokuro`` already in a library would be orphaned
        and the whole library re-OCR'd under the new name.
        """
        nova = get_engine("hayai-nova")
        assert nova.recognizer == "JustANormalTinkerer/hayai-ocr-v2.5-nova"
        assert not nova.uses_mokuro_env
        assert [e for e in ENGINE_IDS if "hayai" in e] == ["hayai-nova"]

    def test_patch_budget_applies_to_nova_only(self) -> None:
        """The dial reaches exactly one engine, and must not pretend otherwise."""
        assert uses_patch_budget("hayai-nova")
        assert not any(uses_patch_budget(e) for e in ENGINE_IDS if e != "hayai-nova")

    def test_patch_budget_values(self) -> None:
        assert PATCH_BUDGETS == (256, 384, 512)
        # 512, not the card's own default of 384: at 384 v2.5 reads WORSE
        # than v2.1 on the author's benchmark (CER 3.65% vs 3.23%).
        assert DEFAULT_PATCH_BUDGET == 512
        assert get_patch_budget("384") == 384
        assert get_patch_budget(512) == 512
        for bad in ("128", "1024", "400", "", "auto", None):
            with pytest.raises(ValueError):
                get_patch_budget(bad)

    def test_unknown_engine_raises(self) -> None:
        with pytest.raises(ValueError, match="Unknown OCR engine 'nope'"):
            get_engine("nope")
