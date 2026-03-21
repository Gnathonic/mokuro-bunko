"""WsgiDAV server factory for mokuro-bunko."""

from __future__ import annotations

import atexit
from collections.abc import Callable
from pathlib import Path
from typing import TYPE_CHECKING, Any

from wsgidav.wsgidav_app import WsgiDAVApp

from mokuro_bunko.account.api import AccountAPI
from mokuro_bunko.admin.api import AdminAPI
from mokuro_bunko.catalog.api import CatalogAPI
from mokuro_bunko.config import Config, get_default_config_path
from mokuro_bunko.database import Database
from mokuro_bunko.dyndns import DynDNSService
from mokuro_bunko.home.api import HomePageAPI
from mokuro_bunko.library_index import LibraryIndexCache
from mokuro_bunko.login.api import LoginAPI
from mokuro_bunko.middleware.auth import AuthMiddleware
from mokuro_bunko.middleware.cors import CorsMiddleware
from mokuro_bunko.middleware.fs_watcher import LibraryWatcher
from mokuro_bunko.middleware.propfind_cache import PropfindCacheMiddleware
from mokuro_bunko.middleware.request_log import RequestLogMiddleware
from mokuro_bunko.queue.api import QueueAPI
from mokuro_bunko.registration.api import RegistrationAPI
from mokuro_bunko.setup.api import SetupWizardAPI
from mokuro_bunko.static import StaticMiddleware
from mokuro_bunko.tunnel import TunnelService
from mokuro_bunko.webdav.provider import MokuroDAVProvider

if TYPE_CHECKING:
    pass


def _assert_writable_dir(path: Path, label: str) -> None:
    """Raise ValueError if directory cannot be created and written."""
    try:
        path.mkdir(parents=True, exist_ok=True)
        probe_path = path / ".write-check"
        with probe_path.open("wb") as handle:
            handle.write(b"ok")
        probe_path.unlink(missing_ok=True)
    except OSError as exc:
        raise ValueError(f"{label} is not writable: {path}") from exc


def _validate_startup_environment(config: Config) -> None:
    """Validate critical runtime prerequisites before starting the server."""
    storage_base = config.storage.base_path
    config.storage.ensure_directories()
    _assert_writable_dir(storage_base, "storage.base_path")
    _assert_writable_dir(config.storage.library_path, "storage.library_path")
    _assert_writable_dir(config.storage.inbox_path, "storage.inbox_path")
    _assert_writable_dir(config.storage.users_path, "storage.users_path")

    if not config.ssl.enabled:
        return

    if config.ssl.auto_cert:
        from mokuro_bunko.ssl import get_default_cert_paths

        cert_path, key_path = get_default_cert_paths()
        _assert_writable_dir(cert_path.parent, "ssl auto-cert directory")
        _assert_writable_dir(key_path.parent, "ssl auto-key directory")
        return

    cert_path = Path(config.ssl.cert_file).expanduser()
    key_path = Path(config.ssl.key_file).expanduser()

    if not cert_path.exists() or not cert_path.is_file():
        raise ValueError(f"SSL certificate file not found: {cert_path}")
    if not key_path.exists() or not key_path.is_file():
        raise ValueError(f"SSL private key file not found: {key_path}")

    try:
        with cert_path.open("rb"):
            pass
        with key_path.open("rb"):
            pass
    except OSError as exc:
        raise ValueError("SSL certificate/key files are not readable") from exc

    from mokuro_bunko.ssl import validate_certificate_pair

    ssl_errors, _ssl_warnings = validate_certificate_pair(cert_path, key_path)
    if ssl_errors:
        raise ValueError(ssl_errors[0])


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


def create_app(
    config: Config,
    config_path: Path | None = None,
    ocr_runtime: dict[str, Any] | None = None,
) -> Callable[..., Any]:
    """Create the full WSGI application stack.

    Args:
        config: Server configuration.
        config_path: Path to config file for runtime config saving.
        ocr_runtime: Pre-built OCR runtime status dict (avoids subprocess spawns).

    Returns:
        Complete WSGI application with all middleware.
    """
    # Resolve config_path
    if config_path is None:
        config_path = get_default_config_path()

    # Ensure storage directories exist
    config.storage.ensure_directories()

    # Create database
    db_path = config.storage.base_path / "mokuro.db"
    database = Database(db_path)
    database.configure_connection(
        connect_timeout_seconds=config.database.connect_timeout_seconds,
        busy_timeout_ms=config.database.busy_timeout_ms,
        connect_retries=config.database.connect_retries,
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
    # 4. AuthMiddleware (sets mokuro.role in environ)
    # 5. CatalogAPI (public catalog, no auth required)
    # 6. RegistrationAPI (handles /api/register without auth)
    # 7. LoginAPI (login page + /login/api/me)
    # 8. AccountAPI (account page + /api/account/*)
    # 9. HomePageAPI (serves welcome page at / for browsers)
    # 10. SetupWizardAPI (intercepts / -> /setup on first run)
    # 11. StaticMiddleware (serves shared CSS/JS)
    # 12. CorsMiddleware (handles CORS)
    # 13. RequestLogMiddleware (MOKURO_DEBUG=1, outermost)

    app: Callable[..., Any] = dav_app

    # Wrap with PROPFIND cache (caches Depth:infinity responses + gzip)
    propfind_cache = PropfindCacheMiddleware(app, ttl=120.0)
    app = propfind_cache
    library_index = LibraryIndexCache(config.storage.library_path, ttl=30.0)

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
        )

    # Wrap with auth middleware (sets role for admin API to check)
    app = AuthMiddleware(
        app,
        database,
        realm="mokuro-bunko",
        registration_config=config.registration,
        quota_config=config.quota,
        admin_path=config.admin.path,
    )

    # Wrap with catalog API (public catalog page)
    app = CatalogAPI(
        app,
        storage_base_path=str(config.storage.library_path),
        catalog_config=config.catalog,
        library_index=library_index,
    )

    # Wrap with queue status page (public)
    app = QueueAPI(
        app,
        storage_base_path=str(config.storage.base_path),
        ocr_backend=config.ocr.backend,
        database=database,
        queue_config=config.queue,
        library_index=library_index,
    )

    # Wrap with registration API (handles unauthenticated registration)
    app = RegistrationAPI(app, database, config.registration)

    # Wrap with login page
    app = LoginAPI(app, database, nav_config=config)

    # Wrap with account page
    app = AccountAPI(app, database, storage_path=config.storage.base_path)

    # Wrap with home page middleware (serves welcome page for browsers)
    app = HomePageAPI(
        app,
        catalog_config=config.catalog,
        database=database,
        library_index=library_index,
    )

    # Wrap with setup wizard (intercepts / -> /setup when no admin exists)
    app = SetupWizardAPI(app, database, config, config_path)

    # Wrap with static file middleware (serves shared CSS/JS at /_static/)
    app = StaticMiddleware(app)

    # Wrap with CORS middleware (outermost to handle OPTIONS before auth)
    if config.cors.enabled:
        app = CorsMiddleware(app, config.cors)

    # Wrap with request logging (outermost; enabled by MOKURO_DEBUG=1)
    app = RequestLogMiddleware(app)

    # Attach propfind cache for startup warming
    app._propfind_cache = propfind_cache  # type: ignore[attr-defined]
    app._library_index = library_index  # type: ignore[attr-defined]

    # Warm the PROPFIND cache in a background thread
    print("Warming PROPFIND cache...")
    propfind_cache.warm()

    def on_library_change() -> None:
        library_index.invalidate()
        propfind_cache.invalidate()
        propfind_cache.schedule_refresh(delay=1.0)

    # Start filesystem watcher for out-of-band changes (OCR sidecars, thumbnails)
    library_watcher = LibraryWatcher(
        watch_path=config.storage.library_path,
        on_change=on_library_change,
    )
    library_watcher.start()
    app._library_watcher = library_watcher  # type: ignore[attr-defined]
    app._ocr_worker = None  # type: ignore[attr-defined]

    def _cleanup_background_services() -> None:
        watcher = getattr(app, "_library_watcher", None)
        if watcher is not None:
            try:
                watcher.stop(skip_observer_shutdown=True)
            except Exception:
                pass
        cache = getattr(app, "_propfind_cache", None)
        if cache is not None:
            try:
                cache.stop()
            except Exception:
                pass

    atexit.register(_cleanup_background_services)
    app._cleanup_background_services = _cleanup_background_services  # type: ignore[attr-defined]

    return app


def create_ssl_server(
    config: Config,
    config_path: Path | None = None,
    ocr_runtime: dict[str, Any] | None = None,
) -> Any:
    """Create an SSL-enabled server.

    Args:
        config: Server configuration with SSL enabled.
        config_path: Path to config file.
        ocr_runtime: Pre-built OCR runtime status dict.

    Returns:
        Configured cheroot WSGIServer with SSL.
    """
    from cheroot.ssl.builtin import BuiltinSSLAdapter
    from cheroot.wsgi import Server as WSGIServer

    from mokuro_bunko.ssl import generate_self_signed_cert, get_default_cert_paths

    app = create_app(config, config_path, ocr_runtime=ocr_runtime)

    server = WSGIServer(
        (config.server.host, config.server.port),
        app,
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


def run_server(config: Config, config_path: Path | None = None) -> None:
    """Run the WebDAV server.

    Args:
        config: Server configuration.
        config_path: Path to config file.
    """
    from mokuro_bunko.ocr.installer import (
        OCRBackend,
        OCRInstaller,
        detect_hardware,
        get_backend_unavailable_reasons,
        get_recommended_backend,
        get_supported_backends,
    )
    from mokuro_bunko.ocr.watcher import OCRWorker
    from mokuro_bunko.ssl import get_ssl_info

    try:
        _validate_startup_environment(config)
    except ValueError as exc:
        print(f"Startup validation failed: {exc}")
        raise SystemExit(2) from exc

    ocr_worker: OCRWorker | None = None
    selected_backend = None
    ocr_runtime: dict[str, Any] | None = None

    # Determine protocol for display
    protocol = "https" if config.ssl.enabled else "http"

    print(f"Starting mokuro-bunko server on {protocol}://{config.server.host}:{config.server.port}")
    print(f"Storage path: {config.storage.base_path}")
    if config.ssl.enabled:
        print(f"SSL: {get_ssl_info(config.ssl)}")
    if config.ocr.backend != "skip":
        installer = OCRInstaller(output_callback=lambda msg: print(f"[OCR-INSTALL] {msg}"))
        hardware = detect_hardware()
        supported_backends = get_supported_backends(hardware=hardware)
        unavailable = get_backend_unavailable_reasons(hardware=hardware)

        configured_backend = config.ocr.backend
        if configured_backend == "auto":
            selected_backend = get_recommended_backend(
                hardware=hardware,
                supported_backends=supported_backends,
            )
            print(f"OCR backend auto-selected: {selected_backend.value}")
        else:
            selected_backend = OCRBackend(configured_backend)
            if selected_backend not in supported_backends:
                reason = unavailable.get(selected_backend, "Unsupported backend")
                print(f"OCR backend '{selected_backend.value}' unavailable: {reason}")
                print("Falling back to CPU backend.")
                selected_backend = OCRBackend.CPU

        if not installer.is_installed():
            print(f"OCR environment not found. Installing backend={selected_backend.value}...")
            ok = installer.install_with_fallback(selected_backend, force=False)
            if not ok:
                print("OCR installation failed; OCR worker will be disabled.")
                selected_backend = OCRBackend.SKIP
        else:
            print(f"OCR environment found at {installer.env_path}")

        # Build OCR runtime status from already-computed values (no extra subprocesses)
        installed_backend = installer.get_installed_backend()
        ocr_runtime = {
            "available": True,
            "launch_only": True,
            "configured_backend": config.ocr.backend,
            "installed": installer.is_installed(),
            "installed_backend": installed_backend.value if installed_backend else None,
            "env_path": str(installer.env_path),
            "supported_backends": [b.value for b in supported_backends],
            "unavailable_backends": {k.value: v for k, v in unavailable.items()},
            "cli_hint": "Use `mokuro-bunko serve --ocr <auto|cuda|rocm|cpu|skip>` and "
            "`mokuro-bunko install-ocr --list-backends`.",
            "driver_hint": "CUDA/ROCm drivers/toolkits must be installed on the host for GPU backends.",
        }

    # Create server (with SSL if enabled)
    server = create_ssl_server(config, config_path, ocr_runtime=ocr_runtime)

    # Start thread pool watchdog (works around cheroot thread death on Windows)
    from mokuro_bunko.cheroot_watchdog import ThreadPoolWatchdog
    watchdog = ThreadPoolWatchdog(server)
    watchdog.start()

    if config.ocr.backend != "skip" and selected_backend != OCRBackend.SKIP:
        import mokuro_bunko.ocr.watcher as watcher

        ocr_worker = OCRWorker(
            storage_path=config.storage.base_path,
            poll_interval=float(config.ocr.poll_interval),
            processor_timeouts={
                "hard_timeout_seconds": int(config.ocr.hard_timeout_seconds),
                "no_progress_timeout_seconds": int(config.ocr.no_progress_timeout_seconds),
                "finalizing_timeout_seconds": int(config.ocr.finalizing_timeout_seconds),
            },
            status_callback=lambda msg: print(f"[OCR] {msg}"),
        )
        ocr_worker.start(background=True)
        watcher.CURRENT_OCR_WORKER = ocr_worker
        # expose ocr worker for API / queue status control
        if hasattr(server, "wsgi_app") and hasattr(server.wsgi_app, "_ocr_worker"):
            server.wsgi_app._ocr_worker = ocr_worker  # type: ignore[attr-defined]
        print(
            "OCR worker enabled "
            f"(configured={config.ocr.backend}, active={selected_backend.value}, "
            f"interval={config.ocr.poll_interval}s)"
        )
    print("Press Ctrl+C to stop")

    try:
        _start_server_resilient(server)
    except KeyboardInterrupt:
        print("\nShutting down...")
    finally:
        watchdog.stop()
        if ocr_worker:
            try:
                ocr_worker.stop()
            except Exception:
                pass
        import mokuro_bunko.ocr.watcher as watcher

        watcher.CURRENT_OCR_WORKER = None
        # Stop filesystem watcher and cancel pending cache refresh timers
        wsgi_app = server.wsgi_app  # type: ignore[attr-defined]
        if hasattr(wsgi_app, "_library_watcher"):
            try:
                wsgi_app._library_watcher.stop()
            except Exception:
                pass
        if hasattr(wsgi_app, "_propfind_cache"):
            try:
                wsgi_app._propfind_cache.stop()
            except Exception:
                pass
        try:
            server.stop()
        except Exception:
            pass
