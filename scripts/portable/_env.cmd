@echo off
rem Shared environment for the Mokuro Bunko portable folder.
rem Everything (Python, packages, OCR models, config, library, logs)
rem lives inside this folder - nothing is written to AppData or the registry.
rem Pre-set any of the UV_/PIP_/HF_ variables before launching to override
rem (e.g. to share caches between installs).

set "MB_ROOT=%~dp0"

if not defined UV_PYTHON_INSTALL_DIR set "UV_PYTHON_INSTALL_DIR=%MB_ROOT%runtime\python"
if not defined UV_CACHE_DIR set "UV_CACHE_DIR=%MB_ROOT%runtime\uv-cache"
if not defined UV_PROJECT_ENVIRONMENT set "UV_PROJECT_ENVIRONMENT=%MB_ROOT%runtime\venv"
if not defined PIP_CACHE_DIR set "PIP_CACHE_DIR=%MB_ROOT%runtime\pip-cache"
if not defined HF_HOME set "HF_HOME=%MB_ROOT%runtime\hf-cache"

set "MOKURO_CONFIG=%MB_ROOT%data\config.yaml"
set "MOKURO_STORAGE=%MB_ROOT%data"
set "MOKURO_BUNKO_OCR_ENV=%MB_ROOT%runtime\ocr-env"

if not exist "%MB_ROOT%data" mkdir "%MB_ROOT%data"
if not exist "%MB_ROOT%runtime" mkdir "%MB_ROOT%runtime"
