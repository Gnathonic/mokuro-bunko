"""Server-side compilation of the reader's `series.json` / `catalog.json`."""

from mokuro_bunko.metadata.paths import (
    CATALOG_FILE_NAME,
    SERIES_FILE_NAME,
    is_catalog_file_path,
    is_compiled_metadata_path,
    is_series_file_path,
    series_title_from_series_file_path,
)
from mokuro_bunko.metadata.service import MetadataService

__all__ = [
    "CATALOG_FILE_NAME",
    "SERIES_FILE_NAME",
    "MetadataService",
    "is_catalog_file_path",
    "is_compiled_metadata_path",
    "is_series_file_path",
    "series_title_from_series_file_path",
]
