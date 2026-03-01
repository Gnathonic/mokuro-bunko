"""Custom DAV provider for mokuro-bunko.

Compatible with mokuro-reader's expected WebDAV structure.
The reader expects a /mokuro-reader/ folder containing:
  - volume-data.json, profiles.json (per-user, isolated)
  - {SeriesTitle}/{Volume}.cbz (shared library)
"""

from __future__ import annotations

from pathlib import Path
from typing import TYPE_CHECKING, Any, Optional

from wsgidav.dav_provider import DAVProvider

from mokuro_bunko.webdav.resources import (
    MokuroFileResource,
    MokuroFolderResource,
    PathMapper,
)

if TYPE_CHECKING:
    from wsgidav.dav_provider import DAVCollection, DAVNonCollection


class MokuroDAVProvider(DAVProvider):
    """WebDAV provider compatible with mokuro-reader.

    Provides a virtual filesystem where:
    - /mokuro-reader/ shows shared manga library + per-user progress
    """

    def __init__(self, storage_base: Path) -> None:
        """Initialize provider.

        Args:
            storage_base: Base path for storage directory.
        """
        super().__init__()
        self.storage_base = Path(storage_base)
        self.path_mapper = PathMapper(storage_base)
        self.path_mapper.ensure_directories()

    def get_resource_inst(
        self,
        path: str,
        environ: dict[str, Any],
    ) -> Optional[DAVCollection | DAVNonCollection]:
        """Get resource instance for a path.

        Args:
            path: Virtual WebDAV path.
            environ: WSGI environ dict.

        Returns:
            DAV resource or None if not found.
        """
        # Normalize path
        path = "/" + path.strip("/")

        # Get current user info from environ (set by auth middleware)
        username = None
        user_data = environ.get("mokuro.user")
        if user_data:
            username = user_data.get("username")

        # Root folder (virtual)
        if path == "/":
            return MokuroFolderResource(
                "/",
                environ,
                None,
                self.path_mapper,
                is_virtual=True,
            )

        # /mokuro-reader root (virtual, merged view)
        if path == f"/{PathMapper.READER_ROOT}":
            return MokuroFolderResource(
                f"/{PathMapper.READER_ROOT}",
                environ,
                None,
                self.path_mapper,
                is_virtual=True,
            )

        # /mokuro-reader/* paths
        if path.startswith(f"/{PathMapper.READER_ROOT}/"):
            relative = path[len(f"/{PathMapper.READER_ROOT}/"):]

            # Per-user files (volume-data.json, profiles.json)
            if relative in PathMapper.PER_USER_FILES:
                if username:
                    physical_path = self.path_mapper.get_user_file_path(username, relative)
                    if physical_path is None:
                        return None
                    if physical_path.exists():
                        return MokuroFileResource(path, environ, physical_path)
                    # File doesn't exist yet - return None
                    # PUT will use parent's create_empty_resource()
                    return None
                return None  # Anonymous can't access per-user files

            # Shared library content
            physical_path = self.path_mapper.virtual_to_physical(path, username)
            if physical_path is None:
                return None
            if physical_path.is_dir():
                return MokuroFolderResource(
                    path,
                    environ,
                    physical_path,
                    self.path_mapper,
                )
            elif physical_path.exists():
                return MokuroFileResource(path, environ, physical_path)
            return None

        return None

    def is_readonly(self) -> bool:
        """Return False to allow writes."""
        return False
