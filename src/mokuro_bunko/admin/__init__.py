"""Admin module for mokuro-bunko."""

from mokuro_bunko.admin.api import AdminAPI
from mokuro_bunko.admin.cli import admin_group

__all__ = ["AdminAPI", "admin_group"]
