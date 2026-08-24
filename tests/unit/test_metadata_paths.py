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
