@echo off
rem Shared environment for the Mokuro Bunko launchers (run.bat, doctor.bat).
rem
rem Portable mode (PORTABLE.txt is next to this file, as in the release zip):
rem everything - config, database, library, logs, OCR models - lives in the
rem data\ folder here, and nothing is written to AppData or the registry.
rem
rem Installed mode (scripts\install.ps1 removes PORTABLE.txt): the default
rem locations are used, i.e. %LOCALAPPDATA%\mokuro-bunko, the same place
rem mokuro-bunko 0.5 kept its library, so an existing library is picked up.
rem
rem Pre-set MOKURO_CONFIG / MOKURO_STORAGE before launching to override either.

set "MB_ROOT=%~dp0"
set "MOKURO_LAUNCHER=run.bat"
rem The server may replace mokuro-bunko.exe and mokuro-bunko-cli.exe itself (admin
rem panel > update).
if not defined MOKURO_INSTALL_KIND set "MOKURO_INSTALL_KIND=self"

if exist "%MB_ROOT%PORTABLE.txt" (
    if not defined MOKURO_CONFIG set "MOKURO_CONFIG=%MB_ROOT%data\config.yaml"
    if not defined MOKURO_STORAGE set "MOKURO_STORAGE=%MB_ROOT%data"
    if not exist "%MB_ROOT%data" mkdir "%MB_ROOT%data"
)
