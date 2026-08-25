"""Contract §1: compiled metadata files are library files, never progress."""

from __future__ import annotations

from pathlib import Path

from mokuro_bunko.metadata.paths import (
    is_catalog_file_path,
    is_compiled_metadata_path,
    is_series_file_path,
    series_title_from_series_file_path,
)
from mokuro_bunko.middleware.auth import is_library_path, is_progress_file
from mokuro_bunko.webdav.resources import PathMapper


class TestSeriesFilePaths:
    def test_series_sidecar_is_recognised(self) -> None:
        assert is_series_file_path("/mokuro-reader/Dr Stone/series.json")
        assert series_title_from_series_file_path(
            "/mokuro-reader/Dr Stone/series.json"
        ) == "Dr Stone"

    def test_basename_match_is_case_insensitive(self) -> None:
        assert is_series_file_path("/mokuro-reader/Dr Stone/Series.JSON")

    def test_nested_deeper_than_one_folder_is_not_a_sidecar(self) -> None:
        assert not is_series_file_path("/mokuro-reader/Dr Stone/extras/series.json")

    def test_root_series_json_is_not_a_sidecar(self) -> None:
        assert not is_series_file_path("/mokuro-reader/series.json")

    def test_other_json_in_a_series_folder_is_not_a_sidecar(self) -> None:
        assert not is_series_file_path("/mokuro-reader/Dr Stone/volume-data.json")


class TestCatalogFilePaths:
    def test_root_catalog_is_recognised(self) -> None:
        assert is_catalog_file_path("/mokuro-reader/catalog.json")
        assert is_compiled_metadata_path("/mokuro-reader/catalog.json")

    def test_nested_catalog_is_somebody_elses_file(self) -> None:
        assert not is_catalog_file_path("/mokuro-reader/Dr Stone/catalog.json")


class TestPathAliasNormalization:
    """F1 regression: the matcher must recognize every legal alias spelling
    the real resolver (`security.safe_resolve_under` -> `Path.resolve()`)
    also lands on the same file, and must never match a spelling that
    escapes the library root (which the resolver refuses to resolve at
    all). Verified against the reviewer's empirical repro."""

    ALIASES = [
        "/mokuro-reader/Dr Stone//series.json",
        "/mokuro-reader/Dr Stone/./series.json",
        "/mokuro-reader/./Dr Stone/series.json",
        "/mokuro-reader/Dr Stone/../Dr Stone/series.json",
    ]

    def test_alias_spellings_are_recognised(self) -> None:
        for path in self.ALIASES:
            assert is_series_file_path(path), path
            assert series_title_from_series_file_path(path) == "Dr Stone", path

    def test_a_catalog_alias_is_also_recognised(self) -> None:
        assert is_catalog_file_path("/mokuro-reader/./catalog.json")
        assert is_catalog_file_path("/mokuro-reader/Dr Stone/../catalog.json")

    def test_traversal_escaping_the_library_root_never_matches(self) -> None:
        assert not is_series_file_path("/mokuro-reader/../etc/series.json")
        assert not is_compiled_metadata_path("/mokuro-reader/../catalog.json")
        assert not is_compiled_metadata_path("/mokuro-reader/..")

    def test_a_per_user_file_alias_is_still_excluded(self) -> None:
        assert not is_compiled_metadata_path("/mokuro-reader/./volume-data.json")
        assert not is_compiled_metadata_path("/mokuro-reader/Dr Stone/../volume-data.json")


class TestBoundaryDoubleSlashBypass:
    """Final whole-branch review, F1: `PUT /mokuro-reader//catalog.json` (and
    the other boundary double-slash spellings) used to bypass every helper
    here, because `_library_relative` only normalized the tail AFTER the
    `/mokuro-reader/` prefix test — a library-relative part beginning with
    `/` was refused outright on a "never reachable" theory that turned out
    to be empirically false (wsgidav's own resolver absorbs the extra slash
    via `"/" + path.strip("/")` before `safe_resolve_under` is ever asked).
    These are the reviewer's differential: exactly the 3 boundary spellings
    below flip from unmatched to matched; every `..`-escape, every per-user
    file alias, and every already-matching alias (`TestPathAliasNormalization`
    above) must keep its current answer — pinned together in
    `test_the_rest_of_the_differential_is_unchanged` below.
    """

    def test_boundary_double_slash_after_the_reader_root_is_recognised(self) -> None:
        assert is_catalog_file_path("/mokuro-reader//catalog.json")
        assert is_compiled_metadata_path("/mokuro-reader//catalog.json")

    def test_a_third_slash_is_also_recognised(self) -> None:
        assert is_catalog_file_path("/mokuro-reader///catalog.json")

    def test_boundary_double_slash_before_a_series_file_is_recognised(self) -> None:
        assert is_series_file_path("/mokuro-reader//Dr Stone/series.json")
        assert series_title_from_series_file_path(
            "/mokuro-reader//Dr Stone/series.json"
        ) == "Dr Stone"

    def test_the_rest_of_the_differential_is_unchanged(self) -> None:
        """Every spelling NOT in the 3-item flip set above keeps its old
        answer. `False` means "does not match a compiled path", which is
        the correct/refused answer for every one of these — a `..`-escape,
        a per-user file alias, a depth violation, or a plain non-metadata
        path."""
        unaffected_non_matches = [
            "/mokuro-reader/../etc/series.json",
            "/mokuro-reader/../catalog.json",
            "/mokuro-reader/..",
            "/mokuro-reader/Dr Stone/../../catalog.json",
            "/mokuro-reader/../../catalog.json",
            "/mokuro-reader/./volume-data.json",
            "/mokuro-reader/Dr Stone/../volume-data.json",
            "/mokuro-reader/Dr Stone/extras//series.json",
        ]
        for path in unaffected_non_matches:
            assert not is_compiled_metadata_path(path), path

        unaffected_matches = [
            "/mokuro-reader/Dr Stone//series.json",
            "/mokuro-reader/Dr Stone/./series.json",
            "/mokuro-reader/./Dr Stone/series.json",
            "/mokuro-reader/Dr Stone/../Dr Stone/series.json",
            "/mokuro-reader/./catalog.json",
            "/mokuro-reader/Dr Stone/../catalog.json",
        ]
        for path in unaffected_matches:
            assert is_compiled_metadata_path(path), path


class TestPartitioning:
    """The regression the contract asks for: metadata is never progress."""

    METADATA_PATHS = [
        "/mokuro-reader/Dr Stone/series.json",
        "/mokuro-reader/catalog.json",
    ]

    def test_metadata_paths_are_never_progress_files(self) -> None:
        for path in self.METADATA_PATHS:
            assert not is_progress_file(path)

    def test_metadata_paths_are_library_paths(self) -> None:
        for path in self.METADATA_PATHS:
            assert is_library_path(path)

    def test_metadata_paths_map_into_the_shared_library(self, tmp_path: Path) -> None:
        mapper = PathMapper(tmp_path)
        for path in self.METADATA_PATHS:
            physical = mapper.virtual_to_physical(path, username="alice")
            assert physical is not None
            assert physical.is_relative_to(mapper.library_path.resolve())
            assert not physical.is_relative_to(mapper.users_path.resolve())
            assert mapper.get_path_type(path) == "library"
            assert not mapper.is_per_user_file(path)

    def test_stale_root_series_metadata_json_is_an_ordinary_library_file(
        self, tmp_path: Path
    ) -> None:
        """Retired file: inert junk, never progress, never compiled."""
        mapper = PathMapper(tmp_path)
        path = "/mokuro-reader/series-metadata.json"
        assert not is_progress_file(path)
        assert not is_compiled_metadata_path(path)
        physical = mapper.virtual_to_physical(path, username="alice")
        assert physical is not None
        assert physical.is_relative_to(mapper.library_path.resolve())
