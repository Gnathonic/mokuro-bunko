@echo off
setlocal
title Mokuro Bunko - diagnostics
call "%~dp0_env.cmd"

"%~dp0bin\uv.exe" run --directory "%~dp0app" mokuro-bunko doctor
echo.
pause
