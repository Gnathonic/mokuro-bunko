@echo off
setlocal
title Mokuro Bunko (portable)
call "%~dp0_env.cmd"

echo ============================================================
echo  Mokuro Bunko (portable)
echo.
echo  The FIRST run downloads Python and ~2 GB of OCR components
echo  into this folder. This can take several minutes.
echo  Later starts are fast.
echo.
echo  Your library, config and logs live in:  %MB_ROOT%data
echo  Close this window to stop the server.
echo ============================================================
echo.

rem Open the browser once the server answers (waits up to 15 min for
rem a first-run OCR install; exits silently if the server never starts).
start "" powershell -NoProfile -WindowStyle Hidden -Command "for($i=0;$i -lt 900;$i++){try{$r=Invoke-WebRequest -Uri 'http://127.0.0.1:8080' -UseBasicParsing -TimeoutSec 2; if($r.StatusCode -eq 200){Start-Process 'http://127.0.0.1:8080'; break}}catch{}; Start-Sleep -Seconds 1}"

"%~dp0bin\uv.exe" run --directory "%~dp0app" mokuro-bunko serve
echo.
echo Server stopped.
pause
