"""WsgiDAV server factory for mokuro-bunko."""

from __future__ import annotations

import logging
import os
from collections.abc import Callable, Iterable
from pathlib import Path
from typing import TYPE_CHECKING, Any

from wsgidav.wsgidav_app import WsgiDAVApp

from mokuro_bunko.account.api import AccountAPI
from mokuro_bunko.admin.api import AdminAPI
from mokuro_bunko.catalog.api import CatalogAPI
from mokuro_bunko.catalog.community import CommunityFetcher
from mokuro_bunko.config import Config, get_default_config_path
from mokuro_bunko.database import Database
from mokuro_bunko.dyndns import DynDNSService
from mokuro_bunko.home.api import HomePageAPI
from mokuro_bunko.library_index import LibraryIndexCache
from mokuro_bunko.login.api import LoginAPI
from mokuro_bunko.metadata.compiler import cached_page_count, missing_pages_now
from mokuro_bunko.metadata.middleware import MetadataAPI
from mokuro_bunko.metadata.service import MetadataService
from mokuro_bunko.middleware.auth import AuthMiddleware
from mokuro_bunko.middleware.cors import CorsMiddleware
from mokuro_bunko.middleware.fs_watcher import LibraryWatcher, classify_change
from mokuro_bunko.middleware.path_case import PathCaseMiddleware
from mokuro_bunko.middleware.propfind_cache import PropfindCacheMiddleware
from mokuro_bunko.middleware.queue_file import QueueFileMiddleware
from mokuro_bunko.middleware.request_log import RequestLogMiddleware
from mokuro_bunko.middleware.security_headers import SecurityHeadersMiddleware
from mokuro_bunko.middleware.upload import UploadMiddleware
from mokuro_bunko.ocr.control import OcrControl
from mokuro_bunko.ocr.remote.library_api import ProcessorAPI
from mokuro_bunko.ocr.remote.profiles import ProcessorProfiles
from mokuro_bunko.ocr.remote.registry import ProcessorRegistry
from mokuro_bunko.queue.api import QueueAPI
from mokuro_bunko.registration.api import RegistrationAPI
from mokuro_bunko.security import set_trusted_proxies
from mokuro_bunko.setup.api import SetupWizardAPI
from mokuro_bunko.static import StaticMiddleware
from mokuro_bunko.tunnel import TunnelService
from mokuro_bunko.webdav.provider import MokuroDAVProvider

if TYPE_CHECKING:
    pass


def _assert_writable_dir(path: Path, label: str) -> None:
    """Raise ValueError unless ``path`` is an existing, writable directory."""
    if not path.exists():
        raise ValueError(f"Required directory does not exist ({label}): {path}")
    if not path.is_dir():
        raise ValueError(f"Required path is not a directory ({label}): {path}")
    probe = path / ".mokuro-write-test"
    try:
        probe.write_text("ok", encoding="utf-8")
        probe.unlink()
    except OSError as exc:
        raise ValueError(f"Directory is not writable ({label}): {path}") from exc


def _validate_startup_environment(config: Config) -> None:
    """Validate critical runtime prerequisites before starting the server.

    Raises ValueError if storage directories aren't writable or, when SSL is
    enabled, the certificate/key are missing, unreadable, or invalid.
    """
    config.storage.ensure_directories()
    _assert_writable_dir(config.storage.base_path, "storage.base_path")
    _assert_writable_dir(config.storage.library_path, "storage.library_path")
    _assert_writable_dir(config.storage.inbox_path, "storage.inbox_path")
    _assert_writable_dir(config.storage.users_path, "storage.users_path")

    if not config.ssl.enabled:
        return

    if config.ssl.auto_cert:
        from mokuro_bunko.ssl import get_default_cert_paths

        cert_path, key_path = get_default_cert_paths()
        # First run: the certs dir won't exist until generate_self_signed_cert
        # creates it, so create it here before checking writability.
        cert_path.parent.mkdir(parents=True, exist_ok=True)
        key_path.parent.mkdir(parents=True, exist_ok=True)
        _assert_writable_dir(cert_path.parent, "ssl auto-cert directory")
        _assert_writable_dir(key_path.parent, "ssl auto-key directory")
        return

    cert_path = Path(config.ssl.cert_file).expanduser()
    key_path = Path(config.ssl.key_file).expanduser()
    if not cert_path.is_file():
        raise ValueError(f"SSL certificate file not found: {cert_path}")
    if not key_path.is_file():
        raise ValueError(f"SSL private key file not found: {key_path}")

    from mokuro_bunko.ssl import validate_certificate_pair

    errors, _warnings = validate_certificate_pair(cert_path, key_path)
    if errors:
        raise ValueError(errors[0])


def create_wsgidav_app(config: Config) -> WsgiDAVApp:
    """Create the WsgiDAV application.

    Args:
        config: Server configuration.

    Returns:
        Configured WsgiDAV application.
    """
    # Create provider
    provider = MokuroDAVProvider(config.storage.base_path)

    # WsgiDAV configuration
    dav_config: dict[str, Any] = {
        "provider_mapping": {
            "/": provider,
        },
        "verbose": 1,
        "logging": {
            "enable_loggers": [],
        },
        # Disable built-in authentication (we use our own)
        "http_authenticator": {
            "domain_controller": None,
            "accept_basic": False,
            "accept_digest": False,
            "default_to_digest": False,
        },
        # Allow anonymous access (auth handled by our middleware)
        "simple_dc": {
            "user_mapping": {
                "*": True,  # Allow all
            },
        },
        # Disable the directory browser (we serve our own welcome page)
        "dir_browser": {
            "enable": False,
        },
        # Lock manager
        "lock_storage": True,
        # Property manager
        "property_manager": True,
        # MIME types
        "add_header_MS_Author_Via": True,
    }

    return WsgiDAVApp(dav_config)


def processes_locally(config: Config) -> bool:
    """Whether THIS machine runs OCR itself.

    ``ocr.local_processing: false`` says so directly, and ``ocr.backend:
    skip`` -- the natural setting for a box that must not OCR -- says the
    same thing. Either way the box still owns the queue: it holds until a
    processor logs in, and then hands that processor the work.
    """
    return bool(config.ocr.local_processing) and config.ocr.backend != "skip"


def create_app(
    config: Config,
    config_path: Path | None = None,
    ocr_runtime: dict[str, Any] | None = None,
    ocr_control: OcrControl | None = None,
) -> Callable[..., Any]:
    """Create the full WSGI application stack.

    Args:
        config: Server configuration.
        config_path: Path to config file for runtime config saving.
        ocr_runtime: Pre-built OCR runtime status dict (avoids subprocess spawns).
        ocr_control: Live OCR settings handle shared with the admin API; the
            queue page is bound to it here, the worker later by run_server.

    Returns:
        Complete WSGI application with all middleware.
    """
    # Resolve config_path
    if config_path is None:
        config_path = get_default_config_path()

    # Whose proxy headers are believed, for every rate limit and local check.
    set_trusted_proxies(config.server.trusted_proxies)

    # Ensure storage directories exist
    config.storage.ensure_directories()

    # Create database
    db_path = config.storage.base_path / "mokuro.db"
    database = Database(db_path)
    database.configure_connection(
        busy_timeout_ms=config.database.busy_timeout_ms,
        lock_retries=config.database.lock_retries,
        retry_initial_delay_seconds=config.database.retry_initial_delay_seconds,
    )

    # Create tunnel and DynDNS services
    tunnel_service = TunnelService(config, config_path)
    dyndns_service = DynDNSService(config.dyndns)

    # Start DynDNS if enabled
    if config.dyndns.enabled:
        dyndns_service.start()

    # Create WsgiDAV app
    dav_app = create_wsgidav_app(config)

    # Middleware stack (inside to outside):
    # 1. dav_app (innermost)
    # 2. PropfindCacheMiddleware (caches Depth:infinity + gzip)
    # 3. AdminAPI (handles /_admin, needs role from environ)
    # 4. ProcessorAPI (handles /_processor, needs role from environ)
    # 5. MetadataAPI (intercepts series.json PUTs as update requests)
    # 6. AuthMiddleware (sets mokuro.role in environ)
    # 6b. UploadMiddleware (queues a written .cbz for OCR at once; PUT headers)
    # 6c. QueueFileMiddleware (the virtual /mokuro-reader/.mokuro-queue.json)
    # 6d. PathCaseMiddleware (library paths resolve case-insensitively, as on NTFS)
    # 7. CatalogAPI (public catalog, no auth required)
    # 8. QueueAPI (public queue status page)
    # 9. RegistrationAPI (handles /api/register without auth)
    # 10. LoginAPI (login page + /login/api/me)
    # 11. AccountAPI (account page + /api/account/*)
    # 12. HomePageAPI (serves welcome page at / for browsers)
    # 13. SetupWizardAPI (intercepts / -> /setup on first run)
    # 14. StaticMiddleware (serves shared CSS/JS)
    # 15. CorsMiddleware (handles CORS, when enabled)
    # 16. SecurityHeadersMiddleware (adds the security response headers)
    # 17. RequestLogMiddleware (MOKURO_DEBUG=1, outermost)

    app: Callable[..., Iterable[bytes]] = dav_app

    # When running behind nginx (MOKURO_NGINX_ACCEL=1), flag each request so
    # MokuroFileResource offloads library downloads via X-Accel-Redirect
    # instead of streaming bytes through a WSGI worker thread.
    if os.environ.get("MOKURO_NGINX_ACCEL") == "1":
        _inner = app

        def _nginx_accel_flag(
            environ: dict[str, Any], start_response: Callable[..., Any]
        ) -> Any:
            environ["mokuro.nginx_accel"] = True
            return _inner(environ, start_response)

        app = _nginx_accel_flag

    # Wrap with PROPFIND cache (caches Depth:infinity responses + gzip)
    propfind_cache = PropfindCacheMiddleware(app, ttl=120.0)
    app = propfind_cache
    library_index = LibraryIndexCache(config.storage.library_path, ttl=30.0)

    def on_metadata_published() -> None:
        """Compiled files changed on disk: refresh the listings that show them.

        Deliberately does NOT schedule another regeneration — that would feed
        itself forever. (The filesystem watcher ignores `.json` for the same
        reason: `_RELEVANT_SUFFIXES` has no `.json` entry.)
        """
        library_index.invalidate()
        propfind_cache.schedule_refresh(delay=5.0)
        # A compile is what records a volume as short of pages: have the
        # queue page re-read its missing-pages list on its next refresh.
        queue_api = ocr_control.queue_api if ocr_control is not None else None
        if queue_api is not None:
            queue_api.invalidate_skipped()

    metadata_service = MetadataService(
        config.storage.library_path,
        database,
        on_published=on_metadata_published,
    )

    # Wrap with admin API (innermost, after dav_app)
    if config.admin.enabled:
        app = AdminAPI(
            app,
            database,
            config.admin,
            full_config=config,
            config_path=config_path,
            tunnel_service=tunnel_service,
            dyndns_service=dyndns_service,
            ocr_runtime=ocr_runtime,
            ocr_control=ocr_control,
            library_index=library_index,
        )

    # The registry is created here so the admin API, the queue page and the
    # worker can hold it. Mounted UNCONDITIONALLY, unlike AdminAPI above: a
    # server with its admin panel off still has processors.
    registry = ProcessorRegistry(
        local_name="this server" if processes_locally(config) else None
    )
    if ocr_control is not None:
        ocr_control.remote = registry
    app = ProcessorAPI(
        app,
        registry,
        profiles=ProcessorProfiles(config.storage.base_path),
        samples_dir=config.storage.base_path / ".processing",
        # Asked at every heartbeat and every events body, so a processor
        # account that is disabled, deleted, re-roled or given a new
        # password is cut off while it is connected (spec section 7).
        account_check=database.processor_account_stamp,
    )

    # Wrap with metadata API (intercepts series.json PUTs as update requests).
    # Inside AuthMiddleware so the actor is known; outside the DAV app so the
    # PUT never opens a writer.
    app = MetadataAPI(app, service=metadata_service)

    # Wrap with auth middleware (sets role for admin API to check)
    auth_middleware = AuthMiddleware(
        app,
        database,
        realm="mokuro-bunko",
        registration_config=config.registration,
        # A refused processor login never reaches ProcessorAPI, so the
        # admin panel learns about it here (spec section 6).
        on_processor_login_refused=lambda user, ip: registry.record_failed_login(
            user, f"invalid credentials from {ip}"
        ),
        storage_base_path=config.storage.base_path,
    )
    # Wrap with upload middleware (a .cbz written over WebDAV is queued for
    # OCR at once; a PUT says where its manifest is and when to recheck).
    app = UploadMiddleware(
        auth_middleware,
        storage_base_path=config.storage.base_path,
        ocr_control=ocr_control,
    )
    # The virtual /mokuro-reader/.mokuro-queue.json (read like a library file,
    # never written).
    app = QueueFileMiddleware(
        app,
        storage_base_path=config.storage.base_path,
        ocr_control=ocr_control,
        read_gate=auth_middleware,
    )
    # Library paths resolve as on NTFS: a request spelled `kingdom/` reaches
    # the `Kingdom/` already on disk, so no layer above the filesystem ever
    # sees -- or creates -- a second spelling of one folder.
    app = PathCaseMiddleware(app, config.storage.library_path)

    # Wrap with catalog API (public catalog page). Its volume manifest is read
    # with the archive's own rules, so it is handed the auth gate itself.
    app = CatalogAPI(
        app,
        storage_base_path=str(config.storage.library_path),
        catalog_config=config.catalog,
        library_index=library_index,
        database=database,
        read_gate=auth_middleware,
        layer_order=lambda: [spec.name for spec in config.ocr.generations if not spec.primary],
        ocr_control=ocr_control,
    )

    # Wrap with queue status page (public)
    app = QueueAPI(
        app,
        storage_base_path=str(config.storage.base_path),
        ocr_backend=config.ocr.backend,
        database=database,
        queue_config=config.queue,
        library_index=library_index,
        generations=config.ocr.generations,
        # The pending list is the OCR worker's own queue, read through this
        # handle (the worker is bound to it once it has started).
        ocr_control=ocr_control,
    )
    if ocr_control is not None:
        ocr_control.queue_api = app

    # Wrap with registration API (handles unauthenticated registration)
    app = RegistrationAPI(app, database, config.registration)

    # Wrap with login page
    app = LoginAPI(
        app,
        database,
        nav_config=config,
        # A processor asks for its token here first: a refusal is reported
        # to the admin panel exactly as a refused `/_processor/` request is.
        on_processor_login_refused=lambda user, ip: registry.record_failed_login(
            user, f"invalid credentials from {ip}"
        ),
    )

    # Wrap with account page
    app = AccountAPI(app, database, storage_path=config.storage.base_path)

    # Wrap with home page middleware (serves welcome page for browsers)
    app = HomePageAPI(
        app,
        catalog_config=config.catalog,
        database=database,
        library_index=library_index,
        storage_path=config.storage.base_path,
        ocr_backend=config.ocr.backend,
        ocr_poll_interval=config.ocr.poll_interval,
    )

    # Wrap with setup wizard (intercepts / -> /setup when no admin exists)
    app = SetupWizardAPI(app, database, config, config_path)

    # Wrap with static file middleware (serves shared CSS/JS at /_static/)
    app = StaticMiddleware(app)

    # Wrap with CORS middleware (outermost to handle OPTIONS before auth)
    if config.cors.enabled:
        app = CorsMiddleware(app, config.cors)

    # Wrap with security headers (outside CORS so it covers every response)
    app = SecurityHeadersMiddleware(app)

    # Wrap with request logging (outermost; enabled by MOKURO_DEBUG=1)
    app = RequestLogMiddleware(app)

    # Attach propfind cache for startup warming
    app._propfind_cache = propfind_cache  # type: ignore[attr-defined]
    app._library_index = library_index  # type: ignore[attr-defined]

    # Warm the PROPFIND cache in a background thread
    print("Warming PROPFIND cache...")
    propfind_cache.warm()

    def on_library_change(path: str) -> None:
        library_index.invalidate()
        propfind_cache.schedule_refresh(delay=5.0)
        # Route the metadata work by what changed: a file inside a series
        # folder (a client uploading volumes, an OCR sidecar landing)
        # recompiles just that series; folder-level changes take the full
        # pass, which also prunes deleted series from the catalog.
        kind, series_title = classify_change(config.storage.library_path, path)
        if kind == "series" and series_title is not None:
            metadata_service.schedule_series_regeneration(series_title)
        elif kind == "library":
            metadata_service.schedule_regeneration()

    # Start filesystem watcher for out-of-band changes (OCR sidecars, thumbnails)
    library_watcher = LibraryWatcher(
        watch_path=config.storage.library_path,
        on_change=on_library_change,
    )
    library_watcher.start()
    app._library_watcher = library_watcher  # type: ignore[attr-defined]

    app._metadata_service = metadata_service  # type: ignore[attr-defined]
    app._dyndns_service = dyndns_service  # type: ignore[attr-defined]
    # First compilation runs after startup settles (PROPFIND warm on a large
    # library is already competing for the disk).
    metadata_service.schedule_regeneration(delay=20.0)
    # Periodic full re-sync: catches anything the watcher missed and keeps
    # the materialized catalog honest even on a quiet server.
    metadata_service.start_periodic_rescan(6 * 3600.0)

    # Background AniList/MAL enrichment for linked series (ratings, tags,
    # genres) — feeds the catalog's rating sort and tag filters.
    if config.catalog.enabled and config.catalog.enrich_community:
        community_fetcher = CommunityFetcher(database)
        # A metadata PUT that introduces/changes an external id fetches that
        # series' details within seconds instead of waiting for the hourly
        # sweep (which stays as the refresh/catch-all).
        metadata_service.on_external_ids_changed = community_fetcher.request_fetch
        community_fetcher.start()
        app._community_fetcher = community_fetcher  # type: ignore[attr-defined]

    return app


def shutdown_app(app: Any) -> None:
    """Stop every background service :func:`create_app` started on ``app``.

    The filesystem watcher, the community fetcher, the metadata service (its
    pending pass, series timers and periodic rescan), the PROPFIND cache's
    debounce timer and DynDNS. Safe to call more than once, and on an app
    that never got some of them. Left running, their daemon timers fire
    after the process has begun to exit and write to a closing stderr.
    """
    if hasattr(app, "_library_watcher"):
        app._library_watcher.stop()
    if hasattr(app, "_community_fetcher"):
        app._community_fetcher.stop()
    # The metadata service BEFORE the PROPFIND cache (Task 10 review F3):
    # MetadataService.stop() does not wait for a just-finished pass's
    # deferred on_published() call (it fires after _pass_lock is released,
    # by design -- see service.py). Stopping propfind_cache first would
    # guarantee any such late on_published -> schedule_refresh arms a timer
    # nothing can ever cancel; stopping metadata_service first lets
    # propfind_cache.stop() still catch it.
    if hasattr(app, "_metadata_service"):
        app._metadata_service.stop()
    if hasattr(app, "_propfind_cache"):
        app._propfind_cache.stop()
    if hasattr(app, "_dyndns_service"):
        app._dyndns_service.stop()


def create_ssl_server(
    config: Config,
    config_path: Path | None = None,
    ocr_runtime: dict[str, Any] | None = None,
    ocr_control: OcrControl | None = None,
) -> Any:
    """Create an SSL-enabled server.

    Args:
        config: Server configuration with SSL enabled.
        config_path: Path to config file.
        ocr_runtime: Pre-built OCR runtime status dict.
        ocr_control: Live OCR settings handle (see create_app).

    Returns:
        Configured cheroot WSGIServer with SSL.
    """
    from cheroot.ssl.builtin import BuiltinSSLAdapter
    from cheroot.wsgi import Server as WSGIServer

    from mokuro_bunko.ssl import generate_self_signed_cert, get_default_cert_paths

    app = create_app(config, config_path, ocr_runtime=ocr_runtime, ocr_control=ocr_control)

    server = WSGIServer(
        (config.server.host, config.server.port),
        app,
        numthreads=int(os.environ.get("MOKURO_THREADS", "50")),
    )

    # Configure SSL
    if config.ssl.enabled:
        if config.ssl.auto_cert:
            cert_path, key_path = get_default_cert_paths()
            if not cert_path.exists() or not key_path.exists():
                generate_self_signed_cert(cert_path, key_path)
            cert_file = str(cert_path)
            key_file = str(key_path)
        else:
            cert_file = config.ssl.cert_file
            key_file = config.ssl.key_file

        server.ssl_adapter = BuiltinSSLAdapter(cert_file, key_file)

    return server


def _start_server_resilient(server: Any) -> None:
    """Start the cheroot server with resilience to worker thread death.

    Cheroot worker threads can die from unhandled exceptions in socket
    cleanup code (especially on Windows). When a thread dies, it sets
    server.interrupt which causes the serve() loop to exit. This function
    wraps the serve loop to automatically recover by clearing the interrupt
    flag and continuing.

    See: https://github.com/cherrypy/cheroot/issues/375
    """
    import sys
    import threading

    server.prepare()

    # Start the unservicable-connection handler thread (cheroot's own)
    threading.Thread(
        target=server._serve_unservicable,
        name="UnservicableHandler",
        daemon=True,
    ).start()

    while server.ready:
        try:
            server._connections.run(server.expiration_interval)
        except (KeyboardInterrupt, SystemExit):
            raise
        except Exception:
            server.error_log(
                "Error in HTTPServer.serve",
                level=40,  # logging.ERROR
                traceback=True,
            )

        # If a worker thread died and set the interrupt flag, recover.
        interrupt = server.interrupt
        if interrupt is not None:
            print(
                f"[WATCHDOG] Worker thread set interrupt: {interrupt!r}. "
                "Recovering (clearing flag and continuing).",
                file=sys.stderr,
                flush=True,
            )
            server.interrupt = None


def run_server(config: Config, config_path: Path | None = None, verbose: bool = False) -> None:
    """Run the WebDAV server.

    Args:
        config: Server configuration.
        config_path: Path to config file.
        verbose: Enable DEBUG console logging.
    """
    # Fail fast on a misconfigured environment (unwritable storage, missing or
    # invalid SSL cert) before doing any expensive startup work.
    try:
        _validate_startup_environment(config)
    except ValueError as exc:
        print(f"Startup validation failed: {exc}")
        raise SystemExit(2) from exc

    # Storage is validated writable; persist logs there from here on.
    from mokuro_bunko.logging_setup import setup_logging

    log_file = setup_logging(config.storage.base_path, verbose=verbose)
    logger = logging.getLogger("mokuro_bunko.server")
    ocr_logger = logging.getLogger("mokuro_bunko.ocr")

    from mokuro_bunko.ocr.engines import GpuUse, backend_is_gpu, uses_mokuro_env
    from mokuro_bunko.ocr.generations import (
        ENV_ENGINES,
        ENV_MOKURO,
        detector_env_key,
        enabled_generations,
        primary_generation,
        required_detectors,
        required_engines,
    )
    from mokuro_bunko.ocr.installer import (
        OCR_CLI_HINT,
        OCR_DRIVER_HINT,
        OCR_NO_LOCAL_HINT,
        EnginesInstaller,
        OCRBackend,
        OCRInstaller,
        detect_hardware,
        get_backend_unavailable_reasons,
        get_recommended_backend,
        get_supported_backends,
    )
    from mokuro_bunko.ocr.watcher import OCRWorker
    from mokuro_bunko.ssl import get_ssl_info

    ocr_worker: OCRWorker | None = None
    selected_backend = None
    gpu_use: GpuUse | None = None
    ocr_runtime: dict[str, Any] | None = None
    # The rows that can really run. A row is dropped as each environment
    # it needs fails to install -- and a row may be dropped for a reason its
    # engine-mates survive: its detector's extras failed while another row
    # on the same engine uses one that is present.
    active_generations = list(config.ocr.generations)
    # What THIS box could not install, for the rows that need it: they leave
    # this server's slots, never the queue (`local_environment_problem`).
    local_problems: dict[str, str] = {}
    # The rows the WORKER schedules, when that is more than what runs here:
    # every configured row, with `local_problems` keeping this server's own
    # slots off the ones it could not install for.
    worker_generations: list[Any] | None = None
    primary_missing_here = False
    engines_installer: EnginesInstaller | None = None
    installer: OCRInstaller | None = None
    ocr_control = OcrControl()
    # Whether this box runs OCR itself. When it does not -- local
    # processing off, or backend `skip` -- nothing below installs, probes or
    # narrows anything for it: every configured row stays active and each
    # processor's own catalog decides which rows it is offered.
    local_ocr = processes_locally(config)

    # Determine protocol for display
    protocol = "https" if config.ssl.enabled else "http"

    logger.info(
        "Starting mokuro-bunko server on %s://%s:%s",
        protocol,
        config.server.host,
        config.server.port,
    )
    logger.info("Storage path: %s", config.storage.base_path)
    if log_file is not None:
        logger.info("Server log: %s", log_file)
    if config.ssl.enabled:
        logger.info("SSL: %s", get_ssl_info(config.ssl))
    if not local_ocr:
        ocr_runtime = {
            "available": True,
            "launch_only": True,
            "configured_backend": config.ocr.backend,
            "local_processing": False,
            "generations": [row.to_dict() for row in config.ocr.generations],
            "active_generations": [row.to_dict() for row in active_generations],
            "detectors": list(required_detectors(active_generations)),
            "cli_hint": OCR_NO_LOCAL_HINT,
            "driver_hint": "",
        }
    elif config.ocr.backend != "skip":
        installer = OCRInstaller(
            output_callback=lambda msg: ocr_logger.info("[install] %s", msg)
        )
        hardware = detect_hardware()
        supported_backends = get_supported_backends(hardware=hardware)
        unavailable = get_backend_unavailable_reasons(hardware=hardware)

        configured_backend = config.ocr.backend
        if configured_backend == "auto":
            selected_backend = get_recommended_backend(
                hardware=hardware,
                supported_backends=supported_backends,
            )
            logger.info("OCR backend auto-selected: %s", selected_backend.value)
        else:
            selected_backend = OCRBackend(configured_backend)
            if selected_backend not in supported_backends:
                reason = unavailable.get(selected_backend, "Unsupported backend")
                logger.warning(
                    "OCR backend '%s' unavailable: %s", selected_backend.value, reason
                )
                logger.warning("Falling back to CPU backend.")
                selected_backend = OCRBackend.CPU

        active_engines = required_engines(active_generations)
        needs_mokuro_env = any(uses_mokuro_env(e) for e in active_engines)
        extra_engines = [e for e in active_engines if not uses_mokuro_env(e)]

        if needs_mokuro_env and installer.is_installed() and installer.needs_rebuild_for(
            selected_backend
        ):
            # A CPU-only torch in an environment on a machine whose GPU we
            # can use (typical after `auto` first ran without the driver, or
            # after a GPU was added): rebuild so `auto` really means the GPU.
            logger.warning(
                "OCR environment at %s holds a CPU build of torch but the %s backend is "
                "available; rebuilding it for %s...",
                installer.env_path,
                selected_backend.value,
                selected_backend.value,
            )
            if not installer.rebuild_for(selected_backend):
                logger.error(
                    "OCR environment rebuild for %s failed; running on CPU. "
                    "Delete %s to retry.",
                    selected_backend.value,
                    installer.env_path / installer._REBUILD_FAILED_MARKER,
                )
        if needs_mokuro_env and not installer.is_installed():
            logger.info(
                "OCR environment not found. Installing backend=%s...",
                selected_backend.value,
            )
            ok = installer.install_with_fallback(selected_backend, force=False)
            if not ok:
                logger.error(
                    "OCR installation failed; the mokuro engine will not run on this "
                    "server. Run 'mokuro-bunko doctor' to diagnose."
                )
                local_problems[ENV_MOKURO] = "the mokuro environment failed to install here"
                active_generations = [
                    row for row in active_generations if not row.mokuro_env
                ]
        elif needs_mokuro_env:
            logger.info("OCR environment found at %s", installer.env_path)

        if extra_engines:
            engines_installer = EnginesInstaller(
                output_callback=lambda msg: ocr_logger.info("[install-engines] %s", msg),
                detectors=required_detectors(active_generations),
            )
            if engines_installer.is_installed() and engines_installer.needs_rebuild_for(
                selected_backend
            ):
                logger.warning(
                    "OCR engines environment at %s holds a CPU build of torch but the %s "
                    "backend is available; rebuilding it for %s...",
                    engines_installer.env_path,
                    selected_backend.value,
                    selected_backend.value,
                )
                if not engines_installer.rebuild_for(selected_backend):
                    logger.error(
                        "OCR engines environment rebuild for %s failed; running on CPU. "
                        "Delete %s to retry.",
                        selected_backend.value,
                        engines_installer.env_path / engines_installer._REBUILD_FAILED_MARKER,
                    )
            if engines_installer.is_installed() and not engines_installer.has_detector():
                logger.info(
                    "OCR detector '%s' extras missing from the engines environment; installing...",
                    ", ".join(engines_installer.detectors),
                )
                # Per detector, not all-or-nothing: with a detector per row,
                # one failing detector must only drop the rows that need it.
                for detector_id in engines_installer.detectors:
                    if engines_installer.has_detector(detector_id):
                        continue
                    if engines_installer.install_detector(detector_id):
                        continue
                    dropped = [
                        row.name
                        for row in active_generations
                        if not row.mokuro_env and row.effective_detector == detector_id
                    ]
                    logger.error(
                        "Detector '%s' installation failed; generations %s will not run on "
                        "this server.",
                        detector_id,
                        ", ".join(dropped) or "(none)",
                    )
                    local_problems[detector_env_key(detector_id)] = (
                        f"the {detector_id} detector failed to install here"
                    )
                    active_generations = [
                        row
                        for row in active_generations
                        if row.mokuro_env or row.effective_detector != detector_id
                    ]
            if not engines_installer.is_installed():
                logger.info(
                    "OCR engines environment not found. Installing backend=%s for %s...",
                    selected_backend.value,
                    ", ".join(extra_engines),
                )
                ok = engines_installer.install_with_fallback(selected_backend, force=False)
                if not ok:
                    logger.error(
                        "OCR engines installation failed; engines %s will not run on this "
                        "server. Run 'mokuro-bunko install-ocr --engines %s' to retry.",
                        ", ".join(extra_engines),
                        ",".join(extra_engines),
                    )
                    local_problems[ENV_ENGINES] = "the engines environment failed to install here"
                    active_generations = [
                        row for row in active_generations if row.mokuro_env
                    ]
            else:
                logger.info("OCR engines environment found at %s", engines_installer.env_path)

        # Which environments really run on a GPU. `selected_backend` is only
        # what was asked for: an install above may have fallen back to a CPU
        # torch (install_with_fallback), and a CPU host must not rank its
        # engines with GPU costs. Decided once, here, and shared through
        # ocr_control by the worker and the queue page.
        installed_backend = installer.get_installed_backend() if needs_mokuro_env else None
        ocr_control.selected_backend = selected_backend.value
        ocr_control.mokuro_installer = installer
        ocr_control.engines_installer = engines_installer
        if installed_backend is not None:
            ocr_control.record_env_backend("mokuro", installed_backend.value)
        gpu_use = ocr_control.resolve_gpu(required_engines(active_generations))
        if (
            gpu_use is not None
            and backend_is_gpu(selected_backend.value)
            and not (gpu_use.mokuro_env and gpu_use.engines_env)
        ):
            logger.warning(
                "OCR backend %s was selected but is not what every environment runs on "
                "(mokuro environment on GPU: %s, engines environment on GPU: %s); "
                "a generation whose environment fell back to the CPU will simply be "
                "slower, wherever it sits in the list.",
                selected_backend.value,
                gpu_use.mokuro_env,
                gpu_use.engines_env,
            )

        if not enabled_generations(active_generations):
            # Nothing runs HERE -- but a processor may still log in and run
            # every configured row on its own hardware, so the worker is
            # built all the same, with no local slots.
            logger.error(
                "No OCR generation is usable on this host; the queue waits for a "
                "processor (`mokuro-bunko processor serve`)."
            )
            local_ocr = False
            active_generations = list(config.ocr.generations)
        elif not any(row.primary for row in enabled_generations(active_generations)):
            # Said once the processor channels exist (below): with them, the
            # primary row stays in the queue for a processor.
            primary_missing_here = True

        # Build OCR runtime status from already-computed values (no extra subprocesses)
        ocr_runtime = {
            "available": True,
            "launch_only": True,
            "configured_backend": config.ocr.backend,
            "installed": installer.is_installed() if needs_mokuro_env else None,
            "installed_backend": installed_backend.value if installed_backend else None,
            "env_path": str(installer.env_path),
            "generations": [row.to_dict() for row in config.ocr.generations],
            "active_generations": [row.to_dict() for row in active_generations],
            "detectors": list(required_detectors(active_generations)),
            "engines_env_path": str(engines_installer.env_path) if engines_installer else None,
            "supported_backends": [b.value for b in supported_backends],
            "unavailable_backends": {k.value: v for k, v in unavailable.items()},
            "cli_hint": OCR_CLI_HINT,
            "driver_hint": OCR_DRIVER_HINT,
            # False when nothing turned out usable here: processors only.
            "local_processing": local_ocr,
        }

    # Create server (with SSL if enabled)
    server = create_ssl_server(
        config, config_path, ocr_runtime=ocr_runtime, ocr_control=ocr_control
    )

    if local_ocr and local_problems and ocr_control.remote is not None:
        # Spec section 0: this box is just another processor entry. A row it
        # could not install for leaves THIS server's slots -- never the
        # queue, which a processor whose catalog can run it serves.
        here = {row.id for row in active_generations}
        elsewhere = [
            row.name for row in enabled_generations(config.ocr.generations) if row.id not in here
        ]
        worker_generations = list(config.ocr.generations)
        logger.warning(
            "Generations %s cannot run on this server (%s); a connected processor that "
            "can run them is offered them.",
            ", ".join(elsewhere) or "(none)",
            "; ".join(dict.fromkeys(local_problems.values())),
        )
        primary = primary_generation(config.ocr.generations)
        if primary is not None and primary.name in elsewhere:
            # The primary layer stays first for every volume (the bare
            # <Volume>.mokuro carries the uuid every other layer takes): the
            # rows after it wait for a machine that can write it.
            logger.warning(
                "The primary OCR generation %s cannot run on this server; each volume "
                "waits for its primary layer until a processor that can run it connects, "
                "or this server's environment installs on restart.",
                primary.name,
            )
    elif primary_missing_here:
        # Every remaining row writes a NAMED sidecar: nothing writes the bare
        # <Volume>.mokuro readers count characters from, and every layer gets
        # a uuid of its own.
        logger.warning(
            "The primary OCR generation is not usable on this host; volumes will "
            "download image-only until it is."
        )

    # Start thread pool watchdog (works around cheroot thread death on Windows)
    from mokuro_bunko.cheroot_watchdog import ThreadPoolWatchdog
    watchdog = ThreadPoolWatchdog(server)
    watchdog.start()

    # Always the full worker: even a box that runs no OCR of its own owns the
    # queue, holds it while no processor is connected, and hands it out when
    # one logs in. (Covers are generated by its thumbnail loop either way.)
    # The worker's own handle on the app's database (a connection per call,
    # so a second handle on the same file is safe): it READS the metadata
    # pass's cached volume entries, and writes who produced each sidecar and
    # the audit event of every result delivered (`ocr.provenance`).
    ocr_database = Database(config.storage.base_path / "mokuro.db")
    ocr_database.configure_connection(
        busy_timeout_ms=config.database.busy_timeout_ms,
        lock_retries=config.database.lock_retries,
        retry_initial_delay_seconds=config.database.retry_initial_delay_seconds,
    )
    ocr_worker = OCRWorker(
        storage_path=config.storage.base_path,
        poll_interval=float(config.ocr.poll_interval),
        status_callback=ocr_logger.info,
        generations=worker_generations if worker_generations is not None else active_generations,
        concurrency=config.ocr.concurrency,
        sessions=config.ocr.sessions,
        # Volumes whose supplied `.mokuro` names pages the archive lacks get
        # no additional OCR layers until the file is replaced: the metadata
        # pass's cached verdict, or -- for a volume it has not reached yet --
        # the same check made at once, before any layer can be claimed.
        missing_pages_lookup=lambda cbz: missing_pages_now(
            ocr_database, Path(config.storage.base_path) / "library", cbz
        ),
        # How long each queued volume is, from the same cache the catalog
        # is compiled from. It is what turns "17 volumes pending" into a
        # clock time; a volume the pass has not compiled yet answers None
        # and the queue prediction says that item's number is rough.
        page_count_lookup=lambda cbz: cached_page_count(
            ocr_database, Path(config.storage.base_path) / "library", cbz
        ),
        # Connected processors join the queue as slots while they are
        # logged in; with local processing off they are the ONLY slots.
        remote=ocr_control.remote,
        local_processing=local_ocr,
        autobench=config.ocr.autobench,
        local_unavailable=local_problems if local_ocr else {},
        database=ocr_database,
    )
    # Wired BEFORE the worker starts, so a processor that drops during
    # the first scan still gets its claims returned.
    if ocr_control.remote is not None:
        ocr_control.remote.on_drop = ocr_worker.processor_disconnected
    if local_ocr:
        # Probed BEFORE the first claim: this server's slots decline a row
        # pinned to a card only a processor has (`OCRProcessor.can_run`),
        # and until something probes, the catalog is the fallback that
        # "knows" every gpu:<n> -- so after a restart the local slot would
        # claim such a row and fail its session on a card it has not got.
        # Nothing else probes until an admin opens the generations page.
        from mokuro_bunko.ocr.bench import probe_devices
        from mokuro_bunko.ocr.devices import set_cached_catalog

        set_cached_catalog(probe_devices(ocr_worker.processor.device_probe_python()))
    # Auto-bench asks the admin API's BenchService, built on first use;
    # a server whose admin panel is off has none, and then nothing is
    # benchmarked automatically (a pair runs on the row's own table).
    ocr_worker.bench_service = ocr_control.bench_service
    ocr_worker.start(background=True)
    ocr_control.worker = ocr_worker
    logger.info("OCR local processing: %s", "on" if local_ocr else "off")
    ocr_control.mokuro_installer = installer
    if local_ocr:
        ocr_control.engines_installer = engines_installer or EnginesInstaller(
            output_callback=lambda msg: ocr_logger.info("[install-engines] %s", msg),
            detectors=required_detectors(active_generations),
        )
    ocr_control.runtime = ocr_runtime
    logger.info(
        "OCR worker enabled (configured=%s, active=%s, generations=%s, interval=%ss)",
        config.ocr.backend,
        selected_backend.value if local_ocr and selected_backend is not None
        else "processors only",
        ",".join(row.name for row in enabled_generations(active_generations)),
        config.ocr.poll_interval,
    )
    print("Press Ctrl+C to stop")

    try:
        _start_server_resilient(server)
    except KeyboardInterrupt:
        print("\nShutting down...")
    finally:
        watchdog.stop()
        if ocr_worker:
            ocr_worker.stop()
        # After the worker, so the cancel/close_session ops its stop sent a
        # processor are queued ahead of the sentinel. Without this a
        # connected processor keeps its op stream -- a worker thread
        # yielding heartbeats to a reader that never stops reading -- alive
        # forever, and the process never exits.
        if ocr_control.remote is not None:
            ocr_control.remote.drop_all("the library server is shutting down")
        shutdown_app(server.wsgi_app)
        server.stop()
