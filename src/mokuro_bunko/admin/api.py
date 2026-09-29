"""Admin REST API for mokuro-bunko."""

from __future__ import annotations

import functools
import json
import logging
import re
import shutil
import threading
import time
from collections.abc import Callable, Iterable
from dataclasses import dataclass
from pathlib import Path
from typing import Any, cast
from urllib.parse import parse_qs

from mokuro_bunko.config import QUEUE_DISPLAY_LEVELS, AdminConfig, Config, save_config
from mokuro_bunko.database import INVITABLE_ROLES, AuditQueryError, Database
from mokuro_bunko.library_index import LibraryIndexCache
from mokuro_bunko.metadata.compiler import cached_missing_pages, cached_page_count
from mokuro_bunko.ocr.bench import BenchError, BenchService, probe_devices
from mokuro_bunko.ocr.congestion import CongestionHistory, average_runs
from mokuro_bunko.ocr.control import OcrControl
from mokuro_bunko.ocr.devices import (
    DeviceCatalog,
    cached_catalog,
    catalog_from_processor,
    merge_catalogs,
    set_cached_catalog,
    stage_devices_allowed,
    stage_lock_reason,
)
from mokuro_bunko.ocr.engine_runner import (
    DEVICE_AUTO,
    ORT_GPU_DETECTORS,
    PRECISION_ENGINES,
    ROAD_SERVED,
    STAGE_ENGINE,
    STAGE_MOKURO,
    device_is_gpu,
    host_worker_budget,
    resolve_device,
    road_specs,
    stage_capacities,
    stage_widths,
)
from mokuro_bunko.ocr.engines import (
    DETECTORS,
    ENGINES,
    OFFERED_DETECTOR_IDS,
    PATCH_BUDGETS,
    backend_is_gpu,
    uses_mokuro_env,
)
from mokuro_bunko.ocr.generations import (
    GENERATION_NAME_RE,
    RESERVED_NAMES,
    RESERVED_PREFIXES,
    GenerationConfigError,
    GenerationSpec,
    parse_bench_spec,
    parse_generation_list,
    required_detectors,
    required_engines,
)
from mokuro_bunko.ocr.installer import (
    OCR_CLI_HINT,
    OCR_DRIVER_HINT,
    EnginesInstaller,
    OCRInstaller,
    detect_hardware,
    get_backend_unavailable_reasons,
    get_supported_backends,
)
from mokuro_bunko.ocr.precision import (
    engine_precision_modes,
    model_device,
    precision_catalog,
    precision_on,
)
from mokuro_bunko.ocr.provenance import attribute_volumes
from mokuro_bunko.ocr.remote.profiles import (
    LOCAL_PROFILE,
    POOL_AUTO,
    ProcessorProfiles,
    machine_pools,
    runner_pools,
)
from mokuro_bunko.ocr.throughput import profile_throughput, records_throughput
from mokuro_bunko.registration.invites import InviteManager
from mokuro_bunko.security import is_within_path
from mokuro_bunko.validation import validate_password, validate_username

logger = logging.getLogger(__name__)

# Static files directory
STATIC_DIR = Path(__file__).parent / "web"

# MIME types for static files
MIME_TYPES = {
    ".html": "text/html; charset=utf-8",
    ".js": "application/javascript; charset=utf-8",
    ".css": "text/css; charset=utf-8",
    ".json": "application/json",
    ".png": "image/png",
    ".jpg": "image/jpeg",
    ".jpeg": "image/jpeg",
    ".webp": "image/webp",
    ".ico": "image/x-icon",
    ".svg": "image/svg+xml",
}

MAX_JSON_BODY_BYTES = 64 * 1024

# "Threadripper 7960X (24 cores)" -> 24: how a processor's host line says
# how many cores it has (`bench.cpu_label`).
_CORES_RE = re.compile(r"\((\d+) cores?\)")


def _hardware(host: Any) -> dict[str, Any] | None:
    """A machine's CPU and GPU for the Processors table, or None.

    Just those two, as text: a host dict also carries the backend, and one a
    processor sent is whatever that machine said. None for nothing to show --
    a profile written before hardware was kept, or a probe not answered yet.
    """
    if not isinstance(host, dict):
        return None
    found = {
        key: (str(host[key]) if host.get(key) else None) for key in ("cpu", "gpu")
    }
    return found if any(found.values()) else None


def _device_short(device: str | None) -> str:
    """``CPU`` / ``GPU <n>``: what a resolved device id is, in the select's words."""
    text = str(device or "cpu")
    if not device_is_gpu(text):
        return "CPU" if text == "cpu" else text
    _, colon, index = text.partition(":")
    return f"GPU {index}" if colon and index.isdigit() else "GPU"


def _device_options(
    allowed: list[str], auto: str | None, labels: DeviceCatalog
) -> list[dict[str, str]]:
    """The Device select's options: ``{"id", "label"}`` in ``allowed`` order.

    Every label is the MACHINE's own (``labels``, `DeviceCatalog.label_for`),
    and ``auto`` says what it resolves to there -- worked out with the stage
    left on ``auto``, so it stays true while another choice is selected.
    """
    return [
        {
            "id": device_id,
            "label": (
                f"Auto → {_device_short(auto)}"
                if device_id == DEVICE_AUTO
                else labels.label_for(device_id)
            ),
        }
        for device_id in allowed
    ]


def _stage_rows(
    row: GenerationSpec,
    budget: int,
    gpu: bool | None,
    devices: DeviceCatalog | None = None,
    labels: DeviceCatalog | None = None,
) -> list[dict[str, Any]]:
    """One entry a stage of this row's road, with the derived default sizes.

    A SERVED engine is an ordinary row here: its pages go through the runner
    like any other, so it has the three stages of the served road, with the
    Device select on the ``mokuro`` stage (which IS the serve process) and
    ``feed``/``post`` as the CPU pools either side of it. The keys, the names,
    the devices and the structural ceilings are the RUNNER's, resolved the way
    the runner resolves them for a run (``road_specs`` with this row's
    ``pools.stage_device``) -- never the bare declared graph, which showed a
    detect stage as "CPU x3" that the runner then ran as one process on the
    GPU.

    ``derived_workers`` / ``derived_capacity`` are what the runner would use
    with this row's ``pools`` as they stand, so the UI can show what an empty
    box will actually do. ``devices_allowed`` is what that stage's Device
    select may offer and ``device_locked_reason`` says why it is locked, so
    the table can be built from this alone. ``gpu`` is the backend the server
    is running on; None lets the runner's own probe decide.

    ``device_options`` is that select's options with the machine's own
    labels: ``labels`` names them (the machine the table is for), where
    ``devices`` decides which ids may be chosen. They differ only for this
    server's table, which is every machine's default and so may name any
    machine's card -- by its index, never by another machine's name.

    A MONOLITHIC engine -- one that reads a whole volume behind its own
    command line and has no road at all -- gets the one-stage table of Addendum
    7 instead: the ``mokuro`` stage is the fork's own page pipeline, its
    Workers cell is ``--num_workers`` (no ceiling of ours to declare) and it
    has no queue to give a capacity to.
    """
    catalog = devices if devices is not None else cached_catalog()
    named = labels if labels is not None else catalog
    road = row.road
    detector = row.effective_detector
    if road is None:
        on_gpu = bool(gpu) if gpu is not None else catalog.has_gpu
        allowed = stage_devices_allowed(None, STAGE_MOKURO, engine=row.engine, catalog=catalog)
        return [
            {
                "key": STAGE_MOKURO,
                "name": "the fork's page pipeline",
                "device": resolve_device(
                    row.pools.stage_device.get(STAGE_MOKURO, DEVICE_AUTO), gpu=on_gpu
                ),
                "max_workers": None,
                "derived_workers": None,
                "derived_capacity": None,
                "devices_allowed": allowed,
                "device_options": _device_options(
                    allowed, resolve_device(DEVICE_AUTO, gpu=on_gpu), named
                ),
                "device_locked_reason": stage_lock_reason(None, STAGE_MOKURO, engine=row.engine),
                # The fork's own ``--num_workers``, like the served road's
                # stage of the same name -- never a pool of ours.
                "workers_means": "engine",
            }
        ]
    specs = road_specs(
        road,
        detector=detector,
        engine=row.engine,
        gpu=gpu,
        devices=dict(row.pools.stage_device),
        # What the machine's onnxruntime can reach, as it reported it: the
        # runner there resolves an onnxruntime detector by the same answer.
        ort_gpu=catalog.ort_gpu,
    )
    widths = stage_widths(
        row.engine,
        road,
        budget=budget,
        workers=dict(row.pools.stage_workers),
        specs=specs,
    )
    caps = stage_capacities(
        road, widths, capacities=dict(row.pools.queue_capacity), specs=specs
    )
    # Where each stage would sit with nothing chosen: what its "Auto" means.
    auto = {
        spec.key: spec.device
        for spec in road_specs(
            road, detector=detector, engine=row.engine, gpu=gpu, devices={},
            ort_gpu=catalog.ort_gpu,
        )
    }
    rows: list[dict[str, Any]] = []
    for spec, width, capacity in zip(specs, widths, caps, strict=True):
        allowed = stage_devices_allowed(
            road, spec.key, engine=row.engine, detector=detector, catalog=catalog
        )
        rows.append(
            {
                "key": spec.key,
                "name": spec.name,
                "device": spec.device,
                "max_workers": spec.max_workers,
                "derived_workers": width,
                "derived_capacity": capacity,
                "devices_allowed": allowed,
                "device_options": _device_options(allowed, auto.get(spec.key), named),
                "device_locked_reason": stage_lock_reason(
                    road, spec.key, engine=row.engine, detector=detector, catalog=catalog
                ),
                # What this stage's Workers cell MEANS. "pool" everywhere but:
                # the served road's engine stage, whose one worker is structural
                # (one process, one model) while the number in the cell is the
                # engine's OWN pipeline width; and a recognizer's engine stage on
                # a card, where the number is how many COPIES of the model the
                # session runs there (engine processes, ``RecognizerPool``). Both
                # cells are editable although the stage is device-bound.
                "workers_means": _workers_means(road, spec),
            }
        )
    return rows


@dataclass(frozen=True)
class _Machine:
    """What one processor's pools table is worked out with (`AdminAPI._machine`)."""

    devices: DeviceCatalog
    budget: int
    gpu: bool | None


def _host_budget(host: Any, max_sessions: Any) -> int:
    """A machine's share of ITS cores, as `host_worker_budget` reckons.

    Its host line names its core count ("... (48 cores)"); its sessions are
    the jobs that share them.
    """
    facts = host if isinstance(host, dict) else {}
    match = _CORES_RE.search(str(facts.get("cpu") or ""))
    cpus = int(match.group(1)) if match else None
    try:
        jobs = max(1, int(max_sessions or 1))
    except (TypeError, ValueError):
        jobs = 1
    return host_worker_budget(cpus, jobs=jobs)


def _workers_means(road: str, spec: Any) -> str:
    if road == ROAD_SERVED and spec.key == STAGE_MOKURO:
        return "engine"
    if spec.key == STAGE_ENGINE and device_is_gpu(spec.device):
        return "copies"
    return "pool"


def build_ocr_runtime_status(full_config: Any | None) -> dict[str, Any]:
    """Build OCR runtime status dict (spawns subprocesses — call sparingly)."""
    if not full_config:
        return {"available": False}

    installer = OCRInstaller(output_callback=lambda msg: None)
    hardware = detect_hardware()
    supported = get_supported_backends(hardware=hardware)
    unavailable = get_backend_unavailable_reasons(hardware=hardware)
    installed = installer.is_installed()
    installed_backend = installer.get_installed_backend()
    extra_engines = [e for e in required_engines(full_config.ocr.generations) if not uses_mokuro_env(e)]
    detectors = required_detectors(full_config.ocr.generations)
    engines_installer = EnginesInstaller(
        output_callback=lambda msg: None,
        detectors=detectors,
    )
    engines_installed = engines_installer.is_installed() if extra_engines else None
    detector_ready = engines_installer.has_detector() if engines_installed else None

    return {
        "available": True,
        "launch_only": True,
        "configured_backend": full_config.ocr.backend,
        "installed": installed,
        "installed_backend": installed_backend.value if installed_backend else None,
        "env_path": str(installer.env_path),
        "generations": [row.to_dict() for row in full_config.ocr.generations],
        "detectors": list(detectors),
        "engines_env_path": str(engines_installer.env_path),
        "engines_installed": engines_installed,
        "detector_ready": detector_ready,
        "supported_backends": [b.value for b in supported],
        "unavailable_backends": {k.value: v for k, v in unavailable.items()},
        "cli_hint": OCR_CLI_HINT,
        "driver_hint": OCR_DRIVER_HINT,
    }


class AdminAPI:
    """WSGI middleware for admin panel and API."""

    def __init__(
        self,
        app: Callable[..., Iterable[bytes]],
        database: Database,
        config: AdminConfig,
        full_config: Config | None = None,
        config_path: Path | None = None,
        tunnel_service: Any | None = None,
        dyndns_service: Any | None = None,
        ocr_runtime: dict[str, Any] | None = None,
        ocr_control: OcrControl | None = None,
        library_index: LibraryIndexCache | None = None,
    ) -> None:
        self.app = app
        self.db = database
        self.config = config
        self.full_config = full_config
        self.config_path = config_path
        self.tunnel_service = tunnel_service
        self.dyndns_service = dyndns_service
        self._ocr_runtime_cache = ocr_runtime or {"available": False}
        # Pushes saved OCR settings into the running worker (None: restart applies them).
        self.ocr_control = ocr_control
        # The shared library scan, for the per-generation "31 of 34 volumes"
        # counts. None on a server that has none: the counts are then 0 and
        # the table simply does not show progress.
        self.library_index = library_index
        self.invites = InviteManager(database)
        self.admin_path = config.path.rstrip("/")
        self._config_lock = threading.Lock()
        self._start_time = time.time()
        # Per-row benchmarks: one at a time, holding the OCR queue while it
        # runs. Built lazily, because it needs a storage path that only a
        # server with a full config has.
        self._bench: BenchService | None = None
        # This server's own hardware for the Processors table: probed ONCE, in
        # the background (the GPU is asked of torch in the engines env, which
        # takes seconds), by the first request that wants it.
        self._local_host_lock = threading.Lock()
        self._local_host_thread: threading.Thread | None = None
        self._local_host_value: dict[str, Any] | None = None
        # The worker's auto-bench builds it through the control handle, so it
        # needs no admin page to have been opened first.
        if ocr_control is not None:
            ocr_control.bench_factory = self._bench_service

    def __call__(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> Iterable[bytes]:
        """Handle WSGI request."""
        if not self.config.enabled:
            return self.app(environ, start_response)

        path = environ.get("PATH_INFO", "")
        method = environ.get("REQUEST_METHOD", "GET")

        # Check if this is an admin path
        if not path.startswith(self.admin_path):
            return self.app(environ, start_response)

        # Strip admin prefix from path
        sub_path = path[len(self.admin_path) :]
        if not sub_path:
            sub_path = "/"

        # Serve static files without auth check (JS handles redirect)
        if not sub_path.startswith("/api/"):
            return self._handle_static(environ, start_response, sub_path)

        # Check admin authorization for API endpoints only
        role = environ.get("mokuro.role", "anonymous")
        if not self._can_access_api(role, sub_path):
            error = "Admin access required"
            if sub_path == "/api/invites" or sub_path.startswith("/api/invites/"):
                error = "Admin or inviter access required"
            return self._json_response(
                start_response,
                403,
                {"error": error},
            )

        # Route to appropriate API handler
        return self._handle_api(environ, start_response, sub_path, method)

    @staticmethod
    def _can_access_api(role: str, path: str) -> bool:
        """Check role access for a given admin API path."""
        if path == "/api/invites" or path.startswith("/api/invites/"):
            return role in ("admin", "inviter")
        return role == "admin"

    @staticmethod
    def _actor_username(environ: dict[str, Any]) -> str | None:
        user = environ.get("mokuro.user")
        if isinstance(user, dict):
            username = user.get("username")
            if isinstance(username, str):
                return username
        username = environ.get("mokuro.username")
        if isinstance(username, str):
            return username
        return None

    def _handle_api(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
        path: str,
        method: str,
    ) -> Iterable[bytes]:
        """Handle API requests."""
        # Users endpoints
        if path == "/api/users" and method == "GET":
            return self._list_users(environ, start_response)
        elif path == "/api/users" and method == "POST":
            return self._create_user(environ, start_response)
        elif path.startswith("/api/users/") and method == "DELETE":
            username = path[len("/api/users/") :]
            return self._delete_user(environ, start_response, username)
        elif path.endswith("/notes") and method == "PUT":
            username = path[len("/api/users/") : -len("/notes")]
            return self._update_user_notes(environ, start_response, username)
        elif path.endswith("/role") and method == "PUT":
            username = path[len("/api/users/") : -len("/role")]
            return self._change_role(environ, start_response, username)
        elif path.endswith("/approve") and method == "POST":
            username = path[len("/api/users/") : -len("/approve")]
            return self._approve_user(environ, start_response, username)
        elif path.endswith("/disable") and method == "POST":
            username = path[len("/api/users/") : -len("/disable")]
            return self._disable_user(environ, start_response, username)

        # Invites endpoints
        elif path == "/api/invites" and method == "GET":
            return self._list_invites(environ, start_response)
        elif path == "/api/invites" and method == "POST":
            return self._create_invite(environ, start_response)
        elif path.startswith("/api/invites/") and method == "DELETE":
            code = path[len("/api/invites/") :]
            return self._delete_invite(environ, start_response, code)
        elif path == "/api/audit" and method == "GET":
            return self._list_audit(environ, start_response)

        # Settings endpoints
        elif path == "/api/settings" and method == "GET":
            return self._get_settings(environ, start_response)
        elif path == "/api/settings/registration" and method == "PUT":
            return self._update_registration(environ, start_response)
        elif path == "/api/settings/cors" and method == "PUT":
            return self._update_cors(environ, start_response)
        elif path == "/api/settings/catalog" and method == "PUT":
            return self._update_catalog(environ, start_response)
        elif path == "/api/settings/queue" and method == "PUT":
            return self._update_queue(environ, start_response)
        elif path == "/api/settings/ocr" and method == "PUT":
            return self._update_ocr(environ, start_response)
        elif path == "/api/ocr/generations" and method == "GET":
            return self._get_generations(environ, start_response)
        elif path == "/api/ocr/generations" and method == "PUT":
            return self._update_generations(environ, start_response)
        elif path == "/api/ocr/generations/derive" and method == "POST":
            return self._derive_generation(environ, start_response)
        elif path == "/api/ocr/devices/refresh" and method == "POST":
            return self._refresh_devices(environ, start_response)
        elif (
            path.startswith("/api/ocr/generations/")
            and path.endswith("/pools")
            and method == "PUT"
        ):
            return self._update_processor_pools(
                environ, start_response,
                path[len("/api/ocr/generations/") : -len("/pools")],
            )
        elif path.startswith("/api/ocr/generations/") and path.endswith("/bench"):
            bench_key = path[len("/api/ocr/generations/") : -len("/bench")]
            return self._handle_bench(environ, start_response, bench_key, method)
        elif path == "/api/settings/dyndns" and method == "PUT":
            return self._update_dyndns_settings(environ, start_response)

        # Status endpoint
        elif path == "/api/status" and method == "GET":
            return self._get_status(environ, start_response)
        elif path == "/api/processors" and method == "GET":
            return self._list_processors(start_response)

        # Tunnel endpoints
        elif path == "/api/tunnel/status" and method == "GET":
            return self._get_tunnel_status(environ, start_response)
        elif path == "/api/tunnel/start" and method == "POST":
            return self._start_tunnel(environ, start_response)
        elif path == "/api/tunnel/stop" and method == "POST":
            return self._stop_tunnel(environ, start_response)

        # DynDNS endpoints
        elif path == "/api/dyndns/status" and method == "GET":
            return self._get_dyndns_status(environ, start_response)
        elif path == "/api/dyndns/start" and method == "POST":
            return self._start_dyndns(environ, start_response)
        elif path == "/api/dyndns/stop" and method == "POST":
            return self._stop_dyndns(environ, start_response)
        elif path == "/api/dyndns/test" and method == "POST":
            return self._test_dyndns(environ, start_response)

        # Not found
        return self._json_response(
            start_response,
            404,
            {"error": "API endpoint not found"},
        )

    def _handle_static(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
        path: str,
    ) -> Iterable[bytes]:
        """Serve static files."""
        # Default to index.html
        if path == "/" or path == "":
            path = "/index.html"

        # Security: prevent directory traversal
        file_path = (STATIC_DIR / path.lstrip("/")).resolve()
        if not is_within_path(file_path, STATIC_DIR):
            return self._error_response(start_response, 403, "Forbidden")

        if not file_path.exists() or not file_path.is_file():
            # Return index.html for SPA routing
            file_path = STATIC_DIR / "index.html"
            if not file_path.exists():
                return self._error_response(start_response, 404, "Not found")

        # Determine content type
        ext = file_path.suffix.lower()
        content_type = MIME_TYPES.get(ext, "application/octet-stream")

        # Read and serve file
        try:
            content = file_path.read_bytes()
            headers = [
                ("Content-Type", content_type),
                ("Content-Length", str(len(content))),
                ("Cache-Control", "no-cache"),
            ]
            start_response("200 OK", headers)
            return [content]
        except OSError:
            return self._error_response(start_response, 500, "Error reading file")

    # User API handlers

    def _list_users(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """List all users."""
        users = self.db.list_users()
        return self._json_response(start_response, 200, {"users": users})

    def _create_user(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """Create a new user."""
        try:
            data = self._parse_json_body(environ)
        except ValueError as e:
            return self._json_response(start_response, 400, {"error": str(e)})

        username = data.get("username", "").strip()
        password = data.get("password", "")
        role = data.get("role", "registered")
        notes = data.get("notes", "")

        if not username:
            return self._json_response(start_response, 400, {"error": "Username is required"})
        username_error = validate_username(username)
        if username_error:
            return self._json_response(start_response, 400, {"error": username_error})
        if not password:
            return self._json_response(start_response, 400, {"error": "Password is required"})
        password_error = validate_password(password)
        if password_error:
            return self._json_response(start_response, 400, {"error": password_error})

        try:
            self.db.create_user(username, password, role, notes=notes)
            user = self.db.get_user(username)
            self.db.log_audit_event(
                action="admin_create_user",
                actor_username=self._actor_username(environ),
                target_type="user",
                target_username=username,
                details={"role": role},
            )
            return self._json_response(start_response, 201, {"success": True, "user": user})
        except ValueError as e:
            status = 409 if "already exists" in str(e).lower() else 400
            return self._json_response(start_response, status, {"error": str(e)})

    def _delete_user(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
        username: str,
    ) -> list[bytes]:
        """Delete a user."""
        if self.db.delete_user(username):
            self._drop_processor_account(username, "its account was deleted")
            self.db.log_audit_event(
                action="admin_delete_user",
                actor_username=self._actor_username(environ),
                target_type="user",
                target_username=username,
            )
            return self._json_response(
                start_response, 200, {"success": True, "message": f"User '{username}' deleted"}
            )
        return self._json_response(start_response, 404, {"error": f"User '{username}' not found"})

    def _change_role(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
        username: str,
    ) -> list[bytes]:
        """Change a user's role."""
        try:
            data = self._parse_json_body(environ)
        except ValueError as e:
            return self._json_response(start_response, 400, {"error": str(e)})

        role = data.get("role")
        if not role:
            return self._json_response(start_response, 400, {"error": "Role is required"})

        valid_roles = ["registered", "uploader", "inviter", "editor", "admin", "processor"]
        if role not in valid_roles:
            return self._json_response(
                start_response, 400, {"error": f"Invalid role. Must be one of: {valid_roles}"}
            )

        if self.db.update_user_role(username, role):
            if role != "processor":
                self._drop_processor_account(
                    username, f"its account's role was changed to {role}"
                )
            user = self.db.get_user(username)
            self.db.log_audit_event(
                action="admin_change_role",
                actor_username=self._actor_username(environ),
                target_type="user",
                target_username=username,
                details={"role": role},
            )
            return self._json_response(start_response, 200, {"success": True, "user": user})
        return self._json_response(start_response, 404, {"error": f"User '{username}' not found"})

    def _approve_user(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
        username: str,
    ) -> list[bytes]:
        """Approve a pending user."""
        if self.db.approve_user(username):
            user = self.db.get_user(username)
            self.db.log_audit_event(
                action="admin_approve_user",
                actor_username=self._actor_username(environ),
                target_type="user",
                target_username=username,
            )
            return self._json_response(start_response, 200, {"success": True, "user": user})
        return self._json_response(
            start_response, 404, {"error": f"User '{username}' not found or not pending"}
        )

    def _disable_user(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
        username: str,
    ) -> list[bytes]:
        """Disable a user."""
        if self.db.disable_user(username):
            self._drop_processor_account(username, "its account was disabled")
            user = self.db.get_user(username)
            self.db.log_audit_event(
                action="admin_disable_user",
                actor_username=self._actor_username(environ),
                target_type="user",
                target_username=username,
            )
            return self._json_response(start_response, 200, {"success": True, "user": user})
        return self._json_response(start_response, 404, {"error": f"User '{username}' not found"})

    def _drop_processor_account(self, username: str, reason: str) -> None:
        """Cut off any processor this account has connected, NOW.

        The library also re-checks every connected processor's account at
        heartbeat pace (``ProcessorAPI._account_revoked``), which is what
        catches a change made elsewhere -- the CLI, the account page -- but
        an edit made here need not wait a heartbeat for it.
        """
        registry = getattr(self.ocr_control, "remote", None)
        if registry is None:
            return
        try:
            registry.drop_account(username, reason)
        except Exception:  # pragma: no cover - a registry never fails an admin edit
            pass

    def _update_user_notes(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
        username: str,
    ) -> list[bytes]:
        """Update a user's admin notes."""
        try:
            data = self._parse_json_body(environ)
        except ValueError as e:
            return self._json_response(start_response, 400, {"error": str(e)})

        notes = data.get("notes", "")
        if not isinstance(notes, str):
            return self._json_response(start_response, 400, {"error": "notes must be a string"})

        if self.db.update_user_notes(username, notes):
            user = self.db.get_user(username)
            self.db.log_audit_event(
                action="admin_update_notes",
                actor_username=self._actor_username(environ),
                target_type="user",
                target_username=username,
            )
            return self._json_response(start_response, 200, {"success": True, "user": user})
        return self._json_response(start_response, 404, {"error": f"User '{username}' not found"})

    # Invite API handlers

    def _list_invites(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """List all invites."""
        invites = self.invites.list_all()
        return self._json_response(start_response, 200, {"invites": invites})

    def _create_invite(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """Create a new invite."""
        try:
            data = self._parse_json_body(environ)
        except ValueError as e:
            return self._json_response(start_response, 400, {"error": str(e)})

        role = data.get("role", "registered")
        expires = data.get("expires", "7d")

        valid_roles = sorted(INVITABLE_ROLES)
        if role not in valid_roles:
            return self._json_response(
                start_response, 400, {"error": f"Invalid role. Must be one of: {valid_roles}"}
            )

        try:
            code = self.invites.create_invite(
                role=role,
                expires=expires,
                invited_by=self._actor_username(environ),
            )
            info = self.invites.get_info(code)
            return self._json_response(start_response, 201, {"success": True, "invite": info})
        except ValueError as e:
            return self._json_response(start_response, 400, {"error": str(e)})

    def _delete_invite(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
        code: str,
    ) -> list[bytes]:
        """Delete an invite."""
        if self.invites.delete(code):
            self.db.log_audit_event(
                action="invite_deleted",
                actor_username=self._actor_username(environ),
                target_type="invite",
                target_path=code,
            )
            return self._json_response(
                start_response, 200, {"success": True, "message": "Invite deleted"}
            )
        return self._json_response(start_response, 404, {"error": "Invite not found"})

    def _list_audit(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """One page of audit events, newest first (`Database.query_audit_events`).

        Query: ``actor``; ``action`` and ``target_type`` (repeated or
        comma-separated, any of); ``since`` (inclusive) and ``until``
        (exclusive), a date or ISO date-time; ``q`` (substring search);
        ``include_progress`` (reading-progress sync, left out otherwise);
        ``cursor`` (a page's ``next_cursor``); ``limit`` (50, at most 200).
        A first page (no cursor) also carries ``total`` and the ``facets``
        the filters are chosen from.
        """
        query = parse_qs(str(environ.get("QUERY_STRING") or ""))

        def one(name: str) -> str | None:
            return (query.get(name) or [""])[0].strip() or None

        def many(name: str) -> list[str]:
            return [part.strip() for raw in query.get(name, []) for part in raw.split(",")
                    if part.strip()]

        try:
            limit = int(one("limit") or Database.AUDIT_PAGE_SIZE)
        except ValueError:
            return self._json_response(start_response, 400, {"error": "limit is not a number"})
        cursor = one("cursor")
        try:
            page = self.db.query_audit_events(
                actor=one("actor"),
                actions=many("action"),
                target_types=many("target_type"),
                since=one("since"),
                until=one("until"),
                search=one("q"),
                include_progress=(one("include_progress") or "").lower() in ("1", "true", "yes"),
                cursor=cursor,
                limit=limit,
            )
        except AuditQueryError as e:
            return self._json_response(start_response, 400, {"error": str(e)})
        body: dict[str, Any] = dict(page)
        if not cursor:
            body["facets"] = self.db.audit_facets()
        return self._json_response(start_response, 200, body)

    # Settings API handlers

    def _get_settings(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """Return full config with masked token."""
        if not self.full_config:
            return self._json_response(start_response, 500, {"error": "Config not available"})

        data = self.full_config.to_dict()
        # Mask the DynDNS token
        if data.get("dyndns", {}).get("token"):
            data["dyndns"]["token"] = "****"
        data["ocr_runtime"] = self._ocr_runtime_cache
        return self._json_response(start_response, 200, data)

    def _update_registration(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """Update registration settings."""
        if not self.full_config:
            return self._json_response(start_response, 500, {"error": "Config not available"})

        try:
            data = self._parse_json_body(environ)
        except ValueError as e:
            return self._json_response(start_response, 400, {"error": str(e)})

        with self._config_lock:
            if "mode" in data:
                valid_modes = ["disabled", "self", "invite", "approval"]
                if data["mode"] not in valid_modes:
                    return self._json_response(
                        start_response,
                        400,
                        {"error": f"Invalid mode. Must be one of: {valid_modes}"},
                    )
                self.full_config.registration.mode = data["mode"]
            if "default_role" in data:
                valid_roles = ["registered", "uploader", "inviter", "editor"]
                if data["default_role"] not in valid_roles:
                    return self._json_response(
                        start_response,
                        400,
                        {"error": f"Invalid default_role. Must be one of: {valid_roles}"},
                    )
                self.full_config.registration.default_role = data["default_role"]
            if "allow_anonymous_browse" in data:
                self.full_config.registration.allow_anonymous_browse = bool(
                    data["allow_anonymous_browse"]
                )
            if "allow_anonymous_download" in data:
                self.full_config.registration.allow_anonymous_download = bool(
                    data["allow_anonymous_download"]
                )
            # Backward compatibility for older admin clients
            if "require_login" in data:
                require_login = bool(data["require_login"])
                self.full_config.registration.allow_anonymous_browse = not require_login
                self.full_config.registration.allow_anonymous_download = not require_login
            self._save_config()

        return self._json_response(
            start_response,
            200,
            {
                "success": True,
                "registration": {
                    "mode": self.full_config.registration.mode,
                    "default_role": self.full_config.registration.default_role,
                    "allow_anonymous_browse": self.full_config.registration.allow_anonymous_browse,
                    "allow_anonymous_download": self.full_config.registration.allow_anonymous_download,
                    # Legacy compatibility key
                    "require_login": (
                        (not self.full_config.registration.allow_anonymous_browse)
                        and (not self.full_config.registration.allow_anonymous_download)
                    ),
                },
            },
        )

    def _update_cors(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """Update CORS settings."""
        if not self.full_config:
            return self._json_response(start_response, 500, {"error": "Config not available"})

        try:
            data = self._parse_json_body(environ)
        except ValueError as e:
            return self._json_response(start_response, 400, {"error": str(e)})

        with self._config_lock:
            if "enabled" in data:
                self.full_config.cors.enabled = bool(data["enabled"])
            if "allowed_origins" in data:
                if not isinstance(data["allowed_origins"], list):
                    return self._json_response(
                        start_response, 400, {"error": "allowed_origins must be a list"}
                    )
                self.full_config.cors.allowed_origins = data["allowed_origins"]
            self._save_config()

        return self._json_response(
            start_response,
            200,
            {
                "success": True,
                "cors": {
                    "enabled": self.full_config.cors.enabled,
                    "allowed_origins": self.full_config.cors.allowed_origins,
                },
            },
        )

    def _update_catalog(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """Update catalog settings."""
        if not self.full_config:
            return self._json_response(start_response, 500, {"error": "Config not available"})

        try:
            data = self._parse_json_body(environ)
        except ValueError as e:
            return self._json_response(start_response, 400, {"error": str(e)})

        with self._config_lock:
            if "enabled" in data:
                self.full_config.catalog.enabled = bool(data["enabled"])
            if "reader_url" in data:
                url = data["reader_url"].strip().rstrip("/")
                if url:
                    self.full_config.catalog.reader_url = url
            if "use_as_homepage" in data:
                self.full_config.catalog.use_as_homepage = bool(data["use_as_homepage"])
            self._save_config()

        return self._json_response(
            start_response,
            200,
            {
                "success": True,
                "catalog": {
                    "enabled": self.full_config.catalog.enabled,
                    "reader_url": self.full_config.catalog.reader_url,
                    "use_as_homepage": self.full_config.catalog.use_as_homepage,
                },
            },
        )

    def _update_queue(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """Update queue settings."""
        if not self.full_config:
            return self._json_response(start_response, 500, {"error": "Config not available"})

        try:
            data = self._parse_json_body(environ)
        except ValueError as e:
            return self._json_response(start_response, 400, {"error": str(e)})

        display = data.get("display")
        if display is not None and display not in QUEUE_DISPLAY_LEVELS:
            return self._json_response(
                start_response,
                400,
                {
                    "error": "display must be one of: " + ", ".join(QUEUE_DISPLAY_LEVELS),
                    "field": "display",
                },
            )

        with self._config_lock:
            if "show_in_nav" in data:
                self.full_config.queue.show_in_nav = bool(data["show_in_nav"])
            if "public_access" in data:
                self.full_config.queue.public_access = bool(data["public_access"])
            if display is not None:
                # The queue page reads this same object on every poll: the
                # new level applies to the next one, no restart.
                self.full_config.queue.display = display
            self._save_config()
        control = self.ocr_control
        if control is not None:
            control.queue_state.bump()

        return self._json_response(
            start_response,
            200,
            {
                "success": True,
                "queue": {
                    "show_in_nav": self.full_config.queue.show_in_nav,
                    "public_access": self.full_config.queue.public_access,
                    "display": self.full_config.queue.display,
                },
            },
        )

    def _update_ocr(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """Update the OCR settings that are not the generations list.

        Which is ``poll_interval`` and nothing else. The recipes moved to
        ``PUT /api/ocr/generations``, and a body that still names the old
        scalars is refused there by name rather than silently ignored.
        """
        if not self.full_config:
            return self._json_response(start_response, 500, {"error": "Config not available"})

        try:
            data = self._parse_json_body(environ)
        except ValueError as e:
            return self._json_response(start_response, 400, {"error": str(e)})

        with self._config_lock:
            if "backend" in data:
                return self._json_response(
                    start_response,
                    400,
                    {
                        "error": "OCR backend is launch-only. Use CLI flags/config file to change it."
                    },
                )
            if "char_map" in data:
                return self._json_response(
                    start_response,
                    400,
                    {
                        "error": (
                            "char_map was removed with the character-map system (no "
                            "per-character placement mode produced output worth using; "
                            "readers lay characters on a uniform grid) -- delete the key"
                        )
                    },
                )
            moved = [key for key in ("engines", "detector", "patch_budget") if key in data]
            if moved:
                return self._json_response(
                    start_response,
                    400,
                    {
                        "error": (
                            f"{', '.join(moved)} moved into the generations list; "
                            "PUT them to /api/ocr/generations"
                        )
                    },
                )
            changed = False
            if "poll_interval" in data:
                try:
                    interval = int(data["poll_interval"])
                    if interval < 1:
                        raise ValueError
                    changed = interval != self.full_config.ocr.poll_interval
                    self.full_config.ocr.poll_interval = interval
                except (ValueError, TypeError):
                    return self._json_response(
                        start_response, 400, {"error": "poll_interval must be a positive integer"}
                    )
            self._save_config()

            outcome: dict[str, Any] = {
                "applied": False,
                "installing": False,
                "restart_required": changed,
                "reason": "",
            }
            if changed and self.ocr_control is not None:
                outcome = self.ocr_control.apply(
                    self.full_config.ocr.generations,
                    poll_interval=float(self.full_config.ocr.poll_interval),
                )

        return self._json_response(
            start_response,
            200,
            {
                "success": True,
                "ocr": {
                    "backend": self.full_config.ocr.backend,
                    "poll_interval": self.full_config.ocr.poll_interval,
                    "concurrency": self.full_config.ocr.concurrency,
                },
                **outcome,
                "ocr_runtime": self._refresh_ocr_runtime_cache(),
            },
        )

    # --- OCR generations ------------------------------------------------

    def _get_generations(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """The configured OCR recipes, everything derived from them, and the
        catalog of what a row may be set to."""
        if not self.full_config:
            return self._json_response(start_response, 500, {"error": "Config not available"})
        return self._json_response(start_response, 200, self._generations_payload())

    def _update_generations(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """Replace the whole generations list, in the order it is sent.

        A full replacement, never a patch: the ORDER is the setting, so a
        partial update could not express it. A row without an ``id`` is new
        and gets one minted; an ``id`` that names no existing row is a 400,
        because it would silently detach that row's congestion history and
        its running job.
        """
        if not self.full_config:
            return self._json_response(start_response, 500, {"error": "Config not available"})
        try:
            data = self._parse_json_body(environ)
        except ValueError as e:
            return self._json_response(start_response, 400, {"error": str(e), "row": None, "field": None})
        if "generations" not in data:
            return self._json_response(
                start_response,
                400,
                {"error": "generations is required (a list of rows, in run order)",
                 "row": None, "field": None},
            )

        with self._config_lock:
            known = {row.id for row in self.full_config.ocr.generations}
            rows = data["generations"]
            if isinstance(rows, list):
                for index, row in enumerate(rows):
                    if not isinstance(row, dict):
                        continue
                    raw_id = row.get("id")
                    if raw_id is None or str(raw_id).strip() == "":
                        continue
                    if str(raw_id).strip() not in known:
                        return self._json_response(
                            start_response,
                            400,
                            {
                                "error": (
                                    f"ocr.generations[{index}]: id {str(raw_id)!r} is not a "
                                    "generation this server knows; leave id out for a new row"
                                ),
                                "row": index,
                                "field": "id",
                            },
                        )
            try:
                # With EVERY machine's cards -- this server's when it runs
                # OCR, and each processor's, connected or remembered in its
                # profile -- so a gpu:<n> no machine has is refused HERE,
                # where a person is watching, while a row pinned to a card
                # only a processor has is a valid setting (spec section 3
                # rule 2 then keeps it off the machines that cannot).
                parsed = parse_generation_list(rows, devices=self._settable_devices())
            except GenerationConfigError as e:
                return self._json_response(
                    start_response, 400, {"error": str(e), "row": e.row, "field": e.field}
                )
            except ValueError as e:
                return self._json_response(
                    start_response, 400, {"error": str(e), "row": None, "field": None}
                )

            changed = [row.to_dict() for row in parsed] != [
                row.to_dict() for row in self.full_config.ocr.generations
            ]
            self.full_config.ocr.generations = parsed
            self._save_config()

            outcome: dict[str, Any] = {
                "applied": False,
                "installing": False,
                "restart_required": changed,
                "reason": "",
            }
            if changed and self.ocr_control is not None:
                outcome = self.ocr_control.apply(
                    parsed, poll_interval=float(self.full_config.ocr.poll_interval)
                )
            if changed:
                # A deleted row's benchmark goes with the row, the same way
                # its congestion history does.
                bench = self._bench_service()
                if bench is not None:
                    bench.prune(row.id for row in parsed)
                # ...and so do its numbers on every processor.
                ProcessorProfiles(Path(self.full_config.storage.base_path)).prune(
                    row.id for row in parsed
                )
            payload = self._generations_payload()

        return self._json_response(
            start_response,
            200,
            {"success": True, **payload, **outcome, "ocr_runtime": self._refresh_ocr_runtime_cache()},
        )

    def _derive_generation(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """The stages a row WOULD have, for a spec that has not been saved.

        The same ``stages[]`` the GET shows, for the row as it is currently
        edited, so the table can follow an engine, detector or device change
        without a save and without the server guessing what the client meant.
        Read-only and cheap: no config is touched and nothing is started.
        """
        if not self.full_config:
            return self._json_response(start_response, 500, {"error": "Config not available"})
        try:
            body = self._parse_json_body(environ)
        except ValueError as e:
            return self._json_response(start_response, 400, {"error": str(e)})
        spec_body = body.get("spec") if isinstance(body, dict) else None
        processor_name = body.get("processor") if isinstance(body, dict) else None
        # The row's own table is every machine's default, so it may name any
        # card any of them has (`_settable_devices`) -- named as THIS server
        # names them, and by index where only another machine has one.
        devices = self._settable_devices()
        labels = cached_catalog()
        budget = host_worker_budget(jobs=self.full_config.ocr.concurrency)
        gpu = backend_is_gpu(getattr(self.ocr_control, "selected_backend", None))
        if isinstance(processor_name, str) and processor_name and processor_name != "local":
            # The stages as THAT machine would run them: its cards in the
            # Device select, its cores in the derived widths. An offline
            # machine answers by what it last reported.
            machine = self._machine(processor_name)
            if machine is None:
                return self._json_response(
                    start_response, 400,
                    {"error": f"no processor called {processor_name!r} is known"},
                )
            devices = labels = machine.devices
            budget = machine.budget
            gpu = machine.gpu
        try:
            spec = parse_bench_spec(spec_body, devices=devices)
        except GenerationConfigError as e:
            return self._json_response(
                start_response, 400, {"error": str(e), "row": e.row, "field": e.field}
            )
        return self._json_response(
            start_response,
            200,
            {"road": spec.road, "stages": _stage_rows(spec, budget, gpu, devices, labels)},
        )

    def _machine(self, name: str) -> _Machine | None:
        """One processor's devices, cores and backend, connected or not.

        Its registration while the registry holds it (the connected entry
        first, then one whose stream has closed); what it last registered
        with (``processors/<name>.json``) when the registry does not, or when
        what it sent now names no devices (re-registering mid-install). None
        for a name nobody knows. The catalog is named for the machine, with
        its own host line filling what the catalog left blank
        (`DeviceCatalog.for_machine`) -- never this server's hardware.
        """
        registry = getattr(self.ocr_control, "remote", None)
        entries = [
            entry
            for entry in (registry.entries() if registry is not None else [])
            if not entry.local and entry.name == name
        ]
        entries.sort(key=lambda entry: not entry.stream_open)
        entry = entries[0] if entries else None
        host: dict[str, Any] = dict(entry.host or {}) if entry is not None else {}
        sessions = int(entry.max_sessions or 1) if entry is not None else 1
        devices = catalog_from_processor(entry.catalog) if entry is not None else None
        if devices is None or not devices.probed:
            assert self.full_config is not None
            store = ProcessorProfiles(Path(self.full_config.storage.base_path))
            remembered = store.load(name)
            if remembered:
                recalled = catalog_from_processor(remembered.get("catalog"))
                if devices is None or recalled.probed:
                    devices = recalled
                    if not host and isinstance(remembered.get("host"), dict):
                        host = dict(remembered["host"])
        if devices is None:
            return None
        # Its backend decides where "auto" lands; a machine that never named
        # one but reported its cards lands on card 0 exactly when it has one.
        backend = host.get("backend")
        gpu = backend_is_gpu(backend) if backend else (devices.has_gpu if devices.probed else None)
        return _Machine(
            devices=devices.for_machine(name, host),
            budget=_host_budget(host, sessions),
            gpu=gpu,
        )

    def _settable_devices(self) -> DeviceCatalog:
        """Every device a saved row may be pinned to, on any machine.

        This server's own probed catalog when it runs OCR, merged with every
        processor's: the connected ones, and the ones remembered in a
        profile (a machine that is switched off keeps its cards). A
        processor that has not reported its devices (still installing) adds
        nothing, rather than making every id acceptable.
        """
        assert self.full_config is not None
        catalogs: list[DeviceCatalog] = []
        ocr = self.full_config.ocr
        if ocr.local_processing and ocr.backend != "skip":
            catalogs.append(self._device_catalog())
        seen: set[str] = set()
        registry = getattr(self.ocr_control, "remote", None)
        for entry in registry.entries() if registry is not None else []:
            if entry.local:
                continue
            seen.add(entry.name)
            reported = catalog_from_processor(entry.catalog)
            if reported.probed:
                catalogs.append(reported)
        store = ProcessorProfiles(Path(self.full_config.storage.base_path))
        for name in store.names():
            if name in seen:
                continue
            reported = catalog_from_processor(store.load(name).get("catalog"))
            if reported.probed:
                catalogs.append(reported)
        return merge_catalogs(catalogs) if catalogs else DeviceCatalog()

    def _update_processor_pools(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
        generation_id: str,
    ) -> list[bytes]:
        """One processor's pools for one row. Never touches the config.

        The row's own table in ``ocr.generations`` stays the default for a
        processor with no entry; this writes the override for the machine
        the UI's "for:" selector names, and it takes effect on that
        processor's NEXT session (pools are not output-affecting).
        """
        if self.full_config is None:
            return self._json_response(start_response, 500, {"error": "Config not available"})
        try:
            body = self._parse_json_body(environ)
        except ValueError as e:
            return self._json_response(start_response, 400, {"error": str(e)})
        name = body.get("processor")
        if not isinstance(name, str) or not name or name == "local":
            return self._json_response(
                start_response, 400,
                {"error": "processor must name a processor"},
            )
        row = next(
            (r for r in self.full_config.ocr.generations if r.id == generation_id), None
        )
        if row is None:
            return self._json_response(
                start_response, 400, {"error": f"there is no generation {generation_id!r}"}
            )
        # Held to THAT machine's devices, connected or not: a profile saved
        # for an offline machine is what its next session runs.
        machine = self._machine(name)
        if machine is None:
            return self._json_response(
                start_response, 400, {"error": f"no processor called {name!r} is known"}
            )
        pools_body = body.get("pools")
        if pools_body is not None and not isinstance(pools_body, dict):
            return self._json_response(
                start_response, 400, {"error": "pools must be an object", "field": "pools"}
            )
        # A width or capacity `POOL_AUTO` says "derived on this machine" where
        # the row pins one (a table left empty would run the row's pin there,
        # `profiles.machine_pools`). It is validated as a width of 1 -- so a
        # stage the row has not got is refused like any other -- and stored
        # as said.
        derived: dict[str, list[str]] = {"stage_workers": [], "queue_capacity": []}
        checked = dict(pools_body or {})
        for table in derived:
            values = checked.get(table)
            if isinstance(values, dict):
                derived[table] = [str(k) for k, v in values.items() if v == POOL_AUTO]
                checked[table] = {k: (1 if v == POOL_AUTO else v) for k, v in values.items()}
        try:
            measured = parse_bench_spec(
                {**row.to_dict(), "pools": checked}, devices=machine.devices
            )
        except GenerationConfigError as e:
            return self._json_response(
                start_response, 400, {"error": str(e), "row": None, "field": e.field}
            )
        pools = measured.pools.to_dict()
        for table, keys in derived.items():
            pools[table].update({key.strip(): POOL_AUTO for key in keys})
        # No precision here: a row's precision mode is one for every machine
        # (``precision`` on the row), and a machine's pools never carry one.
        ProcessorProfiles(Path(self.full_config.storage.base_path)).set_pools(
            name, generation_id, pools, recipe=row.output_affecting()
        )
        return self._json_response(start_response, 200, {"success": True, "pools": pools})

    def _refresh_devices(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """Ask the engines environment again what cards this host has."""
        catalog = self._device_catalog(refresh=True)
        return self._json_response(
            start_response, 200, {"success": True, "devices": catalog.entries()}
        )

    def _generations_payload(self) -> dict[str, Any]:
        """The GET body: every row with what is derived from it, plus the catalog."""
        assert self.full_config is not None
        rows = list(self.full_config.ocr.generations)
        history = CongestionHistory(self.full_config.storage.base_path).load()
        counts = self._generation_volume_counts(rows)
        skipped = self._generation_skipped_counts(rows)
        by_machine = self._generation_machine_counts(rows)
        budget = host_worker_budget(jobs=self.full_config.ocr.concurrency)
        bench = self._bench_service()
        # The last FINISHED benchmark of each row, without its trials: the
        # table shows one line of it (pages a second, and whether there is
        # anything to apply), and the trial-by-trial evidence is a click away
        # at the row's own endpoint.
        benches = bench.saved_summaries(rows) if bench is not None else {}
        # The backend this server runs on decides whether a detector sits on
        # the card; None lets the runner's own probe decide.
        # `getattr`, not an attribute: a control handle that has not selected
        # a backend yet (or a test double standing in for one) answers None,
        # which lets the runner's own probe decide -- it must never make the
        # generations page itself fail.
        gpu = backend_is_gpu(getattr(self.ocr_control, "selected_backend", None))
        devices = self._settable_devices()
        registry = getattr(self.ocr_control, "remote", None)
        processors = [e for e in (registry.entries() if registry else []) if not e.local]
        store = ProcessorProfiles(Path(self.full_config.storage.base_path))
        # Read ONCE per (processor, row) rather than once per field: each
        # `row()` is a fresh json.loads off disk. An entry measured for
        # another recipe reads as absent -- it describes another pipeline.
        remote_catalogs = {entry.name: catalog_from_processor(entry.catalog) for entry in processors}
        local_catalog = cached_catalog()
        per_processor = {
            entry.name: {
                row.id: store.row(
                    entry.name, row.id, recipe=row.output_affecting(), mode=row.precision,
                    supported=remote_catalogs[entry.name].supported_for(model_device(row, None)),
                )
                for row in rows
            }
            for entry in processors
        }
        # This server's own profile: what its auto-benchmark found for each
        # row nobody configured by hand (`OCRWorker._local_pools`).
        local = {
            row.id: store.row(
                LOCAL_PROFILE, row.id, recipe=row.output_affecting(), mode=row.precision,
                supported=local_catalog.supported_for(model_device(row, None)),
            )
            for row in rows
        }
        holds = (
            self.ocr_control.precision_holds()
            if self.ocr_control is not None and hasattr(self.ocr_control, "precision_holds")
            else {}
        )
        local_processing = bool(
            self.full_config.ocr.local_processing and self.full_config.ocr.backend != "skip"
        )
        # Where each machine's precision pick stands when it has none yet.
        autobench_on = bool(self.full_config.ocr.autobench)
        failed = (
            self.ocr_control.autobench_failed()
            if self.ocr_control is not None and hasattr(self.ocr_control, "autobench_failed")
            else set()
        )

        def unpicked_state(generation_id: str, machine: str) -> str:
            if not autobench_on:
                return "off"
            return "failed" if (machine, generation_id) in failed else "pending"

        def precision_machines(row: GenerationSpec) -> dict[str, Any]:
            """Every machine's catalog, placement and benchmark of the row."""
            machines: dict[str, Any] = {}
            if local_processing:
                found = local.get(row.id)
                machines["local"] = (
                    local_catalog, None, found.bench if found is not None else None
                )
            for entry in processors:
                found = per_processor[entry.name].get(row.id)
                pools = found.pools if found is not None and found.pools else {}
                machines[entry.name] = (
                    remote_catalogs[entry.name],
                    machine_pools(pools, row.pools.to_dict()).get("stage_device") or {},
                    found.bench if found is not None else None,
                )
            return machines

        # Each processor's own table, worked out here rather than on the
        # first switch to it: the Device select lists THAT machine's devices
        # from the start, and switching machines never waits on a derive.
        machines = {entry.name: self._machine(entry.name) for entry in processors}

        def processor_stages(row: GenerationSpec) -> dict[str, list[dict[str, Any]]]:
            out: dict[str, list[dict[str, Any]]] = {}
            for name, machine in machines.items():
                if machine is None:
                    continue
                found = per_processor[name].get(row.id)
                pools = machine_pools(
                    found.pools if found is not None and found.pools else {},
                    row.pools.to_dict(),
                )
                try:
                    spec = parse_bench_spec(
                        {**row.to_dict(), "pools": runner_pools(pools)},
                        devices=machine.devices,
                    )
                except GenerationConfigError:
                    # Pools that no longer fit the machine (a card it stopped
                    # reporting): the page asks for its table on the switch.
                    continue
                out[name] = _stage_rows(
                    spec, machine.budget, machine.gpu, machine.devices, machine.devices
                )
            return out

        return {
            "generations": [
                {
                    **self._generation_entry(
                        row, history.get(row.id, ()), counts, budget, gpu, devices,
                        labels=local_catalog,
                    ),
                    "processor_stages": processor_stages(row),
                    "volumes_skipped": skipped.get(row.id, 0),
                    # Per machine ('local' or a processor's name): this row's
                    # sidecars on disk now that it wrote, from the provenance
                    # table. Exact -- they sum to at most `volumes_done`; a
                    # sidecar with no record is nobody's.
                    "volumes_by_machine": by_machine.get(row.id, {}),
                    "bench": benches.get(row.id),
                    # The row's precision mode, what every mode would come to
                    # on every machine (the panel's resolution line, live
                    # before a save), and why no machine can run it, if so.
                    **(
                        {
                            "precision": row.precision,
                            "precision_on": precision_on(
                                row,
                                precision_machines(row),
                                unpicked=functools.partial(unpicked_state, row.id),
                            ),
                        }
                        if row.precision_applies
                        else {}
                    ),
                    "precision_hold": holds.get(row.id),
                    # Whether this server runs the row's own table as written
                    # (somebody configured it), or -- not configured -- its
                    # auto-benchmarked pools (``local_pools``, absent while
                    # it has none: derived defaults) and that benchmark.
                    "configured": not row.pools.is_empty(),
                    "local_pools": (
                        found.pools
                        if (found := local.get(row.id)) is not None and found.pools
                        else None
                    ),
                    "local_bench": (
                        found.bench
                        if (found := local.get(row.id)) is not None and found.bench
                        else None
                    ),
                    # This server's own lifetime count of the row (volumes,
                    # pages, pages a second), kept like a processor's
                    # ``processor_runs``; absent while it has read none.
                    "local_runs": (
                        found.runs
                        if (found := local.get(row.id)) is not None and found.runs
                        else None
                    ),
                    # Per connected processor. An absent entry means "no
                    # profile yet -- the row's own table is its default".
                    "processor_pools": {
                        name: found.pools
                        for name, by_row in per_processor.items()
                        if (found := by_row.get(row.id)) is not None and found.pools
                    },
                    "processor_bench": {
                        name: found.bench
                        for name, by_row in per_processor.items()
                        if (found := by_row.get(row.id)) is not None and found.bench
                    },
                    "processor_runs": {
                        name: found.runs
                        for name, by_row in per_processor.items()
                        if (found := by_row.get(row.id)) is not None and found.runs
                    },
                    # "Average congestion from the last few runs", per
                    # machine: the same averaging as the row's own column.
                    "processor_congestion": {
                        name: averaged
                        for name, by_row in per_processor.items()
                        if (found := by_row.get(row.id)) is not None
                        and (averaged := average_runs(found.runs.get("congestion") or []))
                    },
                }
                for row in rows
            ],
            "catalog": self._generations_catalog(devices),
            "processors": [entry.to_dict() for entry in processors],
            "local_processing": bool(
                self.full_config.ocr.local_processing
                and self.full_config.ocr.backend != "skip"
            ),
            "autobench": bool(self.full_config.ocr.autobench),
        }

    def _device_catalog(self, *, refresh: bool = False) -> DeviceCatalog:
        """The devices this server can place a model on, probed once.

        Probed in the ENGINES environment, because the server's own has no
        torch and the card that matters is the one the OCR can see. A server
        without that environment gets ``auto`` + ``cpu``, and refuses nothing:
        see :class:`DeviceCatalog`.
        """
        if refresh:
            set_cached_catalog(None)
        known = cached_catalog()
        if known.probed and not refresh:
            return known
        # Anything missing -- covers-only, OCR off, no environment yet, a
        # control handle that answers nothing -- leaves the fallback catalog,
        # which claims nothing about cards it cannot see. This must never be
        # what makes the generations page fail.
        probed = probe_devices(self._engines_python())
        set_cached_catalog(probed)
        return probed

    def _engines_python(self) -> Path | None:
        """The engines environment's interpreter, or None when there is none.

        Asked of the same worker the benchmark uses, for the same reason: it
        owns the processor that knows where that interpreter is.
        """
        try:
            worker = self._ocr_worker_for_bench()
            processor = getattr(worker, "processor", None) if worker is not None else None
            if processor is not None:
                return cast("Path | None", getattr(processor, "engines_python_path", None))
        except Exception:
            return None
        return None

    def _local_host(self) -> dict[str, Any] | None:
        """This server's CPU and GPU, as a processor names its own when it
        registers (`describe_host`) -- or None until the probe has answered.

        Probed once per server, in a background thread started by the first
        caller: the Processors poll never waits on it (the GPU is asked of
        torch in the engines environment, which takes seconds). The probe is
        the benchmark's own (`BenchService.host`), so the two share it.
        """
        with self._local_host_lock:
            if self._local_host_value is not None:
                return dict(self._local_host_value)
            if self._local_host_thread is None:
                self._local_host_thread = threading.Thread(
                    target=self._probe_local_host, name="local-host-probe", daemon=True
                )
                self._local_host_thread.start()
        return None

    def _probe_local_host(self) -> None:
        from mokuro_bunko.ocr import bench as bench_module

        try:
            engines_python = self._engines_python()
            service = self._bench_service()
            if service is not None:
                found = service.host(engines_python)
            else:
                backend = self.ocr_control.selected_backend if self.ocr_control else None
                found = bench_module.describe_host(backend, engines_python)
        except Exception:
            logger.debug("could not probe this server's hardware", exc_info=True)
            found = {"cpu": bench_module.cpu_label(), "gpu": None}
        with self._local_host_lock:
            self._local_host_value = _hardware(found) or {"cpu": None, "gpu": None}

    # --- per-row benchmark ------------------------------------------------

    def _bench_service(self) -> BenchService | None:
        """The benchmark runner, built once per server.

        Everything it needs that it cannot know for itself -- the live rows,
        the backend that was selected, whether a row's environment is
        installed, how many pages a row still owes the library -- is passed
        as a callable, so the service holds no stale copy of the config and
        needs no restart after an edit.
        """
        if self.full_config is None:
            return None
        if self._bench is None:
            self._bench = BenchService(
                Path(self.full_config.storage.base_path),
                worker=self._ocr_worker_for_bench,
                generations=lambda: (
                    list(self.full_config.ocr.generations) if self.full_config else []
                ),
                backend=lambda: (
                    self.ocr_control.selected_backend if self.ocr_control is not None else None
                ),
                environment_problem=self._bench_environment_problem,
                remaining_pages=self._remaining_pages_for,
                processors=lambda: (
                    registry.entries()
                    if (registry := getattr(self.ocr_control, "remote", None)) is not None
                    else ()
                ),
                profiles=ProcessorProfiles(Path(self.full_config.storage.base_path)),
            )
            # `QueueAPI` reads `paused_for_benchmark` off the SAME service
            # through the control handle they both share, the same way
            # `ocr_control.queue_api` links the other direction.
            if self.ocr_control is not None:
                self.ocr_control.bench = self._bench
        return self._bench

    def _ocr_worker_for_bench(self) -> Any:
        """The worker that really schedules OCR, or None (covers-only, or off)."""
        control = self.ocr_control
        worker = control.worker if control is not None else None
        if worker is None or worker.thumbnails_only:
            return None
        return worker

    def _bench_environment_problem(self, row: GenerationSpec) -> str | None:
        """Why this row cannot be benchmarked yet, naming the environment."""
        control = self.ocr_control
        if uses_mokuro_env(row.engine):
            installer = control.mokuro_installer if control is not None else None
            if installer is not None and not installer.is_installed():
                return (
                    f"the mokuro environment {row.name} needs is not installed yet; "
                    "it installs on restart"
                )
            return None
        missing = (
            f"the OCR engines environment {row.name} needs ({row.engine}) is not "
            "installed yet; it installs on restart"
        )
        installer = control.engines_installer if control is not None else None
        if installer is not None and not installer.is_installed():
            return missing
        worker = self._ocr_worker_for_bench()
        if worker is not None and worker.processor.engines_python_path is None:
            return missing
        return None

    def _remaining_pages_for(self, row: GenerationSpec) -> int | None:
        """Pages of this library still owing this row a sidecar, or None.

        From what the server ALREADY knows -- the library index for which
        volumes lack the row's file, the metadata cache for how many pages
        each of those has. No archive is opened: this sharpens an estimate,
        and opening every archive in a library to sharpen an estimate would
        cost more than the benchmark it decorates.
        """
        index = self.library_index
        if index is None or self.full_config is None:
            return None
        library = Path(self.full_config.storage.base_path) / "library"
        total = 0
        known = False
        for series in index.get_snapshot().series:
            for volume in series.volumes:
                if not volume.has_cbz:
                    continue
                if row.primary:
                    if volume.has_mokuro or volume.has_mokuro_gz:
                        continue
                elif row.name in volume.sidecars:
                    continue
                pages = cached_page_count(
                    self.db, library, library / series.name / f"{volume.name}.cbz"
                )
                if pages is None:
                    continue
                known = True
                total += pages
        return total if known else None

    def _handle_bench(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
        key: str,
        method: str,
    ) -> list[bytes]:
        """POST enqueues one, GET reads it, DELETE cancels it.

        ``key`` is a saved generation id or a client-minted draft key
        (``^draft-[a-z0-9-]{1,24}$``); see `BenchService.enqueue`.
        """
        bench = self._bench_service()
        if bench is None:
            return self._json_response(start_response, 500, {"error": "Config not available"})
        try:
            if method == "POST":
                try:
                    body = self._parse_json_body(environ)
                except ValueError:
                    body = {}
                spec = body.get("spec") if isinstance(body, dict) else None
                pages = body.get("pages") if isinstance(body, dict) else None
                # Spec section 5: the machine the UI chose; this server's
                # own hardware when none is named.
                processor = body.get("processor") if isinstance(body, dict) else None
                return self._json_response(
                    start_response, 202,
                    bench.enqueue(key, spec, pages, processor=str(processor or "local")),
                )
            # The machine whose benchmark to read or cancel (`?processor=`);
            # without one, this server's own -- the same default as a POST.
            query = parse_qs(str(environ.get("QUERY_STRING") or ""))
            asked = query.get("processor")
            named = asked[0] if asked else None
            if method == "GET":
                return self._json_response(start_response, 200, bench.get(key, named))
            if method == "DELETE":
                return self._json_response(start_response, 200, bench.cancel(key, named))
        except BenchError as e:
            return self._json_response(
                start_response, e.status, {"error": e.message, "row": e.row, "field": e.field}
            )
        return self._json_response(start_response, 404, {"error": "API endpoint not found"})

    @staticmethod
    def _generation_entry(
        row: GenerationSpec,
        runs: Any,
        counts: dict[str, tuple[int, int]],
        budget: int,
        gpu: bool | None = None,
        devices: DeviceCatalog | None = None,
        labels: DeviceCatalog | None = None,
    ) -> dict[str, Any]:
        done, total = counts.get(row.id, (0, 0))
        entry = row.to_dict()
        entry.setdefault("detector", None)
        entry.update(
            {
                "sidecar": f"<Volume>{row.sidecar_suffix}",
                "effective_detector": row.reported_detector,
                "detector_locked": row.detector_locked,
                "patch_budget_applies": row.patch_budget_applies,
                # Whether the row's precision mode reaches its engine (a
                # recognizer), so the pools table offers it only there.
                "precision_applies": row.precision_applies,
                "road": row.road,
                "stages": _stage_rows(row, budget, gpu, devices, labels),
                "volumes_done": done,
                "volumes_total": total,
                "congestion": average_runs(runs),
            }
        )
        return entry

    def _generation_volume_counts(
        self, rows: list[GenerationSpec]
    ) -> dict[str, tuple[int, int]]:
        """(volumes with this row's sidecar, volumes in the library) per row."""
        index = self.library_index
        if index is None:
            return {}
        snapshot = index.get_snapshot()
        volumes = [
            volume
            for series in snapshot.series
            for volume in series.volumes
            if volume.has_cbz
        ]
        total = len(volumes)
        counts: dict[str, tuple[int, int]] = {}
        for row in rows:
            if row.primary:
                done = sum(1 for v in volumes if v.has_mokuro or v.has_mokuro_gz)
            else:
                done = sum(1 for v in volumes if row.name in v.sidecars)
            counts[row.id] = (done, total)
        return counts

    def _generation_machine_counts(
        self, rows: list[GenerationSpec]
    ) -> dict[str, dict[str, int]]:
        """{row id: {machine: volumes}} of the sidecars on disk, by who wrote them.

        A volume counts for a row when the library scan finds the row's
        sidecar beside it (the same test as `_generation_volume_counts`),
        and for the machine of its newest provenance record
        (`ocr.provenance.attribute_volumes`).
        """
        index = self.library_index
        if index is None:
            return {}
        snapshot = index.get_snapshot()
        present: dict[str, set[str]] = {row.id: set() for row in rows}
        for series in snapshot.series:
            for volume in series.volumes:
                if not volume.has_cbz:
                    continue
                key = f"{series.name}/{volume.name}.cbz"
                for row in rows:
                    has = (
                        volume.has_mokuro or volume.has_mokuro_gz
                        if row.primary
                        else row.name in volume.sidecars
                    )
                    if has:
                        present[row.id].add(key)
        try:
            records = self.db.ocr_sidecar_producers()
        except Exception:  # noqa: BLE001 - a count is never worth the page
            logger.exception("could not read the OCR sidecar records")
            return {}
        return attribute_volumes(records, present)

    def _generation_skipped_counts(self, rows: list[GenerationSpec]) -> dict[str, int]:
        """Volumes each row will NOT process because they were uploaded short of pages.

        The same flag the OCR worker skips on (`cached_missing_pages`): a
        non-primary row gets no file for such a volume until it is replaced, so
        without this number the row reads as permanently unfinished. The
        primary row is never skipped on these grounds.
        """
        index = self.library_index
        if index is None or self.full_config is None:
            return {}
        library = Path(self.full_config.storage.base_path) / "library"
        snapshot = index.get_snapshot()
        # Only series the metadata pass found damage in are asked volume by
        # volume: each ask stats the archive and its sidecar and queries the
        # cache, and asking all 12k volumes of a large library made this page
        # take seconds. A series the pass has not compiled yet is asked too.
        try:
            catalog_rows = self.db.list_catalog_series()
        except Exception:  # noqa: BLE001 - a count is never worth the page
            logger.exception("could not read the catalog series rows")
            catalog_rows = []
        compiled = {row["folder_name"] for row in catalog_rows}
        damaged = {
            row["folder_name"]
            for row in catalog_rows
            if row["missing_pages"] > 0 or row["damaged_volumes"] > 0
        }
        short = [
            volume
            for series in snapshot.series
            if series.name in damaged or series.name not in compiled
            for volume in series.volumes
            if volume.has_cbz
            and cached_missing_pages(self.db, library, library / series.name / f"{volume.name}.cbz") > 0
        ]
        return {
            row.id: 0 if row.primary else sum(1 for v in short if row.name not in v.sidecars)
            for row in rows
        }

    @staticmethod
    def _generations_catalog(devices: DeviceCatalog | None = None) -> dict[str, Any]:
        """What a row may be set to: the engines, detectors, devices and fixed choices."""
        catalog = devices if devices is not None else cached_catalog()
        return {
            "engines": [
                {
                    "id": spec.id,
                    "label": spec.label,
                    # A row with no road at all: no stages, no pools. A
                    # served engine has all three, in its own environment.
                    "monolithic": spec.road is None and spec.uses_mokuro_env,
                    "served": spec.serve_module is not None,
                    "own_environment": spec.uses_mokuro_env,
                    "own_detector": spec.detector,
                    "patch_budget": spec.patch_budget,
                    # Whether a row's precision mode reaches this engine, and
                    # which modes it offers (mokuro runs no bf16).
                    "precision": spec.id in PRECISION_ENGINES,
                    "precision_modes": engine_precision_modes(spec.id),
                    # ``["cpu"]`` or ``"any"``: where this recognizer may run.
                    "devices": ["cpu"] if spec.cpu_only else "any",
                }
                for spec in ENGINES.values()
            ],
            "detectors": [
                {
                    "id": spec.id,
                    "label": spec.label,
                    # And ["cpu"] too for an onnxruntime detector on a host
                    # whose onnxruntime reported no GPU execution provider.
                    "devices": ["cpu"]
                    if spec.cpu_only
                    or (spec.id in ORT_GPU_DETECTORS and catalog.ort_gpu is False)
                    else "any",
                }
                # Only what a row may name: a disabled detector
                # (``DISABLED_DETECTORS``) is not a choice to render.
                for spec in (DETECTORS[d] for d in OFFERED_DETECTOR_IDS)
            ],
            "devices": catalog.entries(),
            "patch_budgets": list(PATCH_BUDGETS),
            **precision_catalog(),
            "name_pattern": GENERATION_NAME_RE.pattern,
            "reserved_names": list(RESERVED_NAMES),
            "reserved_prefixes": list(RESERVED_PREFIXES),
        }

    def _refresh_ocr_runtime_cache(self) -> dict[str, Any]:
        """Recompute and cache OCR runtime status."""
        status = build_ocr_runtime_status(self.full_config)
        live = self.ocr_control.runtime if self.ocr_control is not None else None
        if live is not None and "active_engines" in live:
            # What the worker is actually running (may lag the saved config
            # while an install is in progress or until a restart).
            status["active_engines"] = list(live["active_engines"])
        self._ocr_runtime_cache = status
        return self._ocr_runtime_cache

    def _update_dyndns_settings(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """Update DynDNS settings."""
        if not self.full_config:
            return self._json_response(start_response, 500, {"error": "Config not available"})

        try:
            data = self._parse_json_body(environ)
        except ValueError as e:
            return self._json_response(start_response, 400, {"error": str(e)})

        with self._config_lock:
            dyndns = self.full_config.dyndns
            if "enabled" in data:
                dyndns.enabled = bool(data["enabled"])
            if "provider" in data:
                if data["provider"] not in ("duckdns", "generic"):
                    return self._json_response(start_response, 400, {"error": "Invalid provider"})
                dyndns.provider = data["provider"]
            # Only update token if explicitly sent and not the masked value
            if "token" in data and data["token"] != "****":
                dyndns.token = data["token"]
            if "domain" in data:
                dyndns.domain = data["domain"]
            if "update_url" in data:
                dyndns.update_url = data["update_url"]
            if "interval" in data:
                try:
                    interval = int(data["interval"])
                    if interval < 30:
                        raise ValueError
                    dyndns.interval = interval
                except (ValueError, TypeError):
                    return self._json_response(
                        start_response, 400, {"error": "interval must be at least 30"}
                    )
            self._save_config()

            # Reconfigure the running service if available
            if self.dyndns_service:
                self.dyndns_service.configure(dyndns)

        result = {
            "enabled": dyndns.enabled,
            "provider": dyndns.provider,
            "domain": dyndns.domain,
            "update_url": dyndns.update_url,
            "interval": dyndns.interval,
            "token": "****" if dyndns.token else "",
        }
        return self._json_response(start_response, 200, {"success": True, "dyndns": result})

    # Status API handler

    def _get_status(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """Return server status info."""
        uptime = time.time() - self._start_time
        users = self.db.list_users()
        user_count = sum(1 for u in users if u["status"] != "deleted")

        # Disk usage
        storage_path = ""
        disk_total = 0
        disk_used = 0
        disk_free = 0
        volume_count = 0
        if self.full_config:
            storage_path = str(self.full_config.storage.base_path)
            try:
                usage = shutil.disk_usage(storage_path)
                disk_total = usage.total
                disk_used = usage.used
                disk_free = usage.free
            except OSError:
                pass
            # Count volumes in library
            lib_path = self.full_config.storage.library_path
            if lib_path.exists():
                volume_count = sum(1 for p in lib_path.iterdir() if p.is_dir())

        host = ""
        port = 0
        if self.full_config:
            host = self.full_config.server.host
            port = self.full_config.server.port

        return self._json_response(
            start_response,
            200,
            {
                "uptime": uptime,
                "host": host,
                "port": port,
                "storage_path": storage_path,
                "disk_total": disk_total,
                "disk_used": disk_used,
                "disk_free": disk_free,
                "user_count": user_count,
                "volume_count": volume_count,
                "stats": {},
            },
        )

    def _list_processors(self, start_response: Callable[..., Any]) -> list[bytes]:
        """Who is connected, who was refused, who left last -- and whether
        this server does any OCR of its own."""
        registry = getattr(self.ocr_control, "remote", None)
        ocr = self.full_config.ocr if self.full_config is not None else None
        local_processing = bool(
            ocr is None or (ocr.local_processing and ocr.backend != "skip")
        )
        control = self.ocr_control
        hold = None
        if control is not None and hasattr(control, "processing_hold"):
            try:
                hold = control.processing_hold()
            except Exception:  # pragma: no cover - a readout never fails the panel
                hold = None
        entries = registry.entries() if registry is not None else []
        # This server's hardware: the registry's local entry has none of its
        # own (nothing registered it), so the probe's answer rides on both.
        local_host = self._local_host() if local_processing else None
        speed = self._processor_speed(entries, local_processing, local_host)
        if registry is None:
            return self._json_response(
                start_response, 200,
                {"processors": [], "failed_logins": [], "last_disconnect": None,
                 "local_processing": local_processing, "processing_hold": hold,
                 "speed": speed},
            )
        last = registry.last_disconnect()
        backoffs = getattr(control, "start_backoffs", None)
        processors = []
        for entry in entries:
            row = entry.to_dict()
            if entry.local:
                row["host"] = local_host or {}
            if callable(backoffs):
                # Rows whose runner will not start on this machine, and until
                # when (the worker files this server's own under "local").
                row["cannot_start"] = backoffs("local" if entry.local else entry.name)
            processors.append(row)
        return self._json_response(
            start_response,
            200,
            {
                "processors": processors,
                "speed": speed,
                # Newest first, as the registry keeps them. The names are
                # untrusted text (whatever a refused Basic header carried):
                # the page escapes them.
                "failed_logins": [
                    {"username": f.username, "reason": f.reason, "at": f.at}
                    for f in registry.failures()
                ],
                "last_disconnect": (
                    {"name": last[0], "at": last[1]} if last is not None else None
                ),
                "local_processing": local_processing,
                "processing_hold": hold,
            },
        )

    def _processor_speed(
        self,
        entries: Iterable[Any],
        local_processing: bool,
        local_host: dict[str, Any] | None = None,
    ) -> list[dict[str, Any]]:
        """Per machine, per OCR generation it has run: what it really delivers.

        ADMIN ONLY -- the public queue is never sent a machine's own number.
        ``pages_per_minute`` is REAL throughput: the pages of its recent
        finished volumes over the wall seconds they took (`ocr.throughput`),
        never the ETA model's fitted slope. Beside it the volume count it is
        averaged over, the machine's own benchmark when one was saved, and
        when it last finished a volume of that layer.

        This server first (while it does OCR of its own), then every
        connected processor, then every processor with a stored profile that
        is not connected now (its numbers outlive the connection).

        Each machine carries its ``host`` (CPU and GPU, or None when not
        known): this server's from the probe (`_local_host`), a connected
        processor's from its registration, and an offline one's from what it
        last registered with, kept in ``processors/<name>.json``
        (`ProcessorProfiles.set_identity`).
        """
        if self.full_config is None:
            return []
        storage = Path(self.full_config.storage.base_path)
        rows = [row for row in self.full_config.ocr.generations if getattr(row, "enabled", True)]
        if not rows:
            return []
        out: list[dict[str, Any]] = []
        store = ProcessorProfiles(storage)
        if local_processing:
            history = CongestionHistory(storage).load()
            bench = self._bench_service()
            benches = bench.saved_summaries(rows) if bench is not None else {}
            layers = []
            for row in rows:
                found = records_throughput(history.get(row.id) or [])
                saved = benches.get(row.id) or {}
                measured_here = saved.get("processor") in (None, "", "local")
                if measured_here and saved:
                    layers.append(self._speed_layer(row, found, saved))
                    continue
                # No saved benchmark of the row: this server's own profile
                # (its auto-benchmark) is its Benchmark figure, as a
                # processor's profile is that processor's.
                mine = store.row(
                    LOCAL_PROFILE, row.id, recipe=row.output_affecting(), mode=row.precision,
                    supported=cached_catalog().supported_for(model_device(row, None)),
                )
                layers.append(
                    self._speed_layer(
                        row, found, mine.bench if mine is not None else None, bench_flat=True
                    )
                )
            out.append(
                {"name": "local", "local": True, "connected": True, "host": local_host,
                 "layers": [layer for layer in layers if layer is not None]}
            )
        live = [e for e in entries if not getattr(e, "local", False)]
        connected = [str(e.name) for e in live]
        live_hosts = {str(e.name): getattr(e, "host", None) for e in live}
        names = connected + sorted(n for n in store.names() if n not in connected)
        for name in names:
            layers = []
            for row in rows:
                profile = store.row(name, row.id, recipe=row.output_affecting(),
                                    mode=row.precision)
                if profile is None:
                    continue
                layers.append(
                    self._speed_layer(
                        row, profile_throughput(profile.runs), profile.bench, bench_flat=True
                    )
                )
            host = live_hosts[name] if name in live_hosts else store.load(name).get("host")
            out.append(
                {"name": name, "local": False, "connected": name in connected,
                 "host": _hardware(host),
                 "layers": [layer for layer in layers if layer is not None]}
            )
        return out

    @staticmethod
    def _speed_layer(
        row: Any, found: Any, bench: dict[str, Any] | None, *, bench_flat: bool = False
    ) -> dict[str, Any] | None:
        """One (machine, layer) line, or None when there is nothing to say."""
        bench_pps = None
        if isinstance(bench, dict):
            blocks = [bench] if bench_flat else [bench.get("best"), bench.get("baseline")]
            for block in blocks:
                value = block.get("pages_per_second") if isinstance(block, dict) else None
                if isinstance(value, (int, float)) and value > 0:
                    bench_pps = float(value)
                    break
        if found is None and bench_pps is None:
            return None
        return {
            "generation_id": row.id,
            "generation": row.name,
            "pages_per_minute": round(found.pages_per_minute, 1) if found else None,
            "volumes": found.volumes if found else 0,
            "last_at": found.last_at if found else None,
            "bench_pages_per_minute": round(bench_pps * 60.0, 1) if bench_pps else None,
        }

    # Tunnel API handlers

    def _get_tunnel_status(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """Return tunnel status."""
        if not self.tunnel_service:
            return self._json_response(
                start_response,
                200,
                {
                    "running": False,
                    "url": None,
                    "available": False,
                },
            )
        return self._json_response(start_response, 200, self.tunnel_service.status)

    def _start_tunnel(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """Start the cloudflare tunnel."""
        if not self.tunnel_service:
            return self._json_response(
                start_response, 500, {"error": "Tunnel service not available"}
            )
        try:
            self.tunnel_service.start()
            return self._json_response(
                start_response, 200, {"success": True, **self.tunnel_service.status}
            )
        except RuntimeError as e:
            return self._json_response(start_response, 400, {"error": str(e)})

    def _stop_tunnel(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """Stop the cloudflare tunnel."""
        if not self.tunnel_service:
            return self._json_response(
                start_response, 500, {"error": "Tunnel service not available"}
            )
        self.tunnel_service.stop()
        return self._json_response(
            start_response, 200, {"success": True, **self.tunnel_service.status}
        )

    # DynDNS API handlers

    def _get_dyndns_status(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """Return DynDNS service status."""
        if not self.dyndns_service:
            return self._json_response(
                start_response,
                200,
                {
                    "enabled": False,
                    "running": False,
                },
            )
        return self._json_response(start_response, 200, self.dyndns_service.status())

    def _start_dyndns(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """Start the DynDNS service."""
        if not self.dyndns_service:
            return self._json_response(
                start_response, 500, {"error": "DynDNS service not available"}
            )
        self.dyndns_service.start()
        return self._json_response(
            start_response, 200, {"success": True, **self.dyndns_service.status()}
        )

    def _stop_dyndns(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """Stop the DynDNS service."""
        if not self.dyndns_service:
            return self._json_response(
                start_response, 500, {"error": "DynDNS service not available"}
            )
        self.dyndns_service.stop()
        return self._json_response(
            start_response, 200, {"success": True, **self.dyndns_service.status()}
        )

    def _test_dyndns(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """Force an immediate DynDNS update."""
        if not self.dyndns_service:
            return self._json_response(
                start_response, 500, {"error": "DynDNS service not available"}
            )
        result = self.dyndns_service.update_now()
        return self._json_response(start_response, 200, result)

    # Helper methods

    def _save_config(self) -> None:
        """Save current config to disk."""
        if self.full_config and self.config_path:
            save_config(self.full_config, self.config_path)

    def _parse_json_body(self, environ: dict[str, Any]) -> dict[str, Any]:
        """Parse JSON request body."""
        try:
            content_length = int(environ.get("CONTENT_LENGTH", 0) or 0)
            if content_length == 0:
                return {}
            if content_length > MAX_JSON_BODY_BYTES:
                raise ValueError("Request body too large")
            body = environ["wsgi.input"].read(content_length)
            return cast("dict[str, Any]", json.loads(body.decode("utf-8")))
        except json.JSONDecodeError as e:
            raise ValueError("Invalid JSON body") from e
        except ValueError as e:
            raise ValueError(str(e)) from e

    def _json_response(
        self,
        start_response: Callable[..., Any],
        status_code: int,
        data: dict[str, Any],
    ) -> list[bytes]:
        """Return a JSON response."""
        status_messages = {
            200: "OK",
            201: "Created",
            400: "Bad Request",
            403: "Forbidden",
            404: "Not Found",
            409: "Conflict",
            500: "Internal Server Error",
        }
        status = f"{status_code} {status_messages.get(status_code, 'Unknown')}"

        body = json.dumps(data).encode("utf-8")
        headers = [
            ("Content-Type", "application/json"),
            ("Content-Length", str(len(body))),
        ]

        start_response(status, headers)
        return [body]

    def _error_response(
        self,
        start_response: Callable[..., Any],
        status_code: int,
        message: str,
    ) -> list[bytes]:
        """Return an error response."""
        status_messages = {
            403: "Forbidden",
            404: "Not Found",
            500: "Internal Server Error",
        }
        status = f"{status_code} {status_messages.get(status_code, 'Error')}"

        body = message.encode("utf-8")
        headers = [
            ("Content-Type", "text/plain"),
            ("Content-Length", str(len(body))),
        ]

        start_response(status, headers)
        return [body]
