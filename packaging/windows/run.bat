@echo off
setlocal
title Mokuro Bunko
call "%~dp0_env.cmd"

echo ============================================================
echo  Mokuro Bunko @VERSION@ (@FLAVOR@)
echo.
if exist "%MB_ROOT%PORTABLE.txt" (
echo  Portable mode: library, config and logs live in
echo      %MB_ROOT%data
) else (
echo  Library, config and logs live in %LOCALAPPDATA%\mokuro-bunko
)
echo  Close this window to stop the server.
echo ============================================================
echo.

rem Open the browser once the server answers (gives up after 2 minutes).
start "" powershell -NoProfile -WindowStyle Hidden -Command "for($i=0;$i -lt 120;$i++){try{$r=Invoke-WebRequest -Uri 'http://127.0.0.1:8080' -UseBasicParsing -TimeoutSec 2; if($r.StatusCode -lt 500){Start-Process 'http://127.0.0.1:8080'; break}}catch{}; Start-Sleep -Seconds 1}"

:serve
"%~dp0mokuro-bunko-cli.exe" serve %*
rem Exit code 75 = "restart me" (after an update from the admin panel).
if %ERRORLEVEL% EQU 75 (
    echo Restarting after an update...
    goto serve
)
echo.
echo Server stopped (exit code %ERRORLEVEL%).
pause
