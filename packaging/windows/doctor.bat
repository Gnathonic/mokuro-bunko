@echo off
setlocal
title Mokuro Bunko - diagnostics
call "%~dp0_env.cmd"

"%~dp0mokuro-bunko.exe" doctor
echo.
pause
