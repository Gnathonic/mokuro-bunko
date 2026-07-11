<#
.SYNOPSIS
    Build the self-contained Windows portable folder distribution.

.DESCRIPTION
    Assembles dist\mokuro-bunko-portable-windows-x64.zip containing:
      app\      - the mokuro-bunko source tree
      bin\      - a bundled uv.exe (downloads Python 3.12 into the folder
                  on first run)
      run.bat / doctor.bat / README.txt

    On first run the folder bootstraps its own Python, virtualenv, and OCR
    environment - all inside the folder. See scripts\portable\README.txt.

.PARAMETER OutDir
    Output directory for the zip (default: dist).

.PARAMETER UvVersion
    uv release to bundle (default: 0.11.28).
#>
[CmdletBinding()]
param(
    [string]$OutDir = "dist",
    [string]$UvVersion = "0.11.28"
)

$ErrorActionPreference = "Stop"
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

$repoRoot = Split-Path -Parent $PSScriptRoot
$stageRoot = Join-Path $repoRoot "$OutDir\portable-stage"
$stage = Join-Path $stageRoot "mokuro-bunko"
$zipPath = Join-Path $repoRoot "$OutDir\mokuro-bunko-portable-windows-x64.zip"

Write-Host "==> Staging portable folder at $stage"
if (Test-Path $stageRoot) { Remove-Item -Recurse -Force $stageRoot }
New-Item -ItemType Directory -Force -Path $stage | Out-Null

# --- app\ : source tree, excluding dev/build artifacts -------------------
Write-Host "==> Copying application source"
$appDir = Join-Path $stage "app"
$excludeDirs = @(
    ".git", ".venv", ".ocr-env", "dist", "build", "node_modules",
    "__pycache__", ".pytest_cache", ".mypy_cache", ".ruff_cache",
    ".github", "tests"
)
robocopy $repoRoot $appDir /E /NFL /NDL /NJH /NJS /NP `
    /XD @($excludeDirs | ForEach-Object { Join-Path $repoRoot $_ }) `
    /XF "*.pyc" ".coverage" "uv.lock" | Out-Null
if ($LASTEXITCODE -ge 8) { throw "robocopy failed with exit code $LASTEXITCODE" }
# robocopy copied scripts\ too; the portable templates don't belong in app\.
Remove-Item -Recurse -Force (Join-Path $appDir "scripts\portable") -ErrorAction SilentlyContinue

# --- bin\ : bundled uv.exe ------------------------------------------------
Write-Host "==> Downloading uv $UvVersion"
$binDir = Join-Path $stage "bin"
New-Item -ItemType Directory -Force -Path $binDir | Out-Null
$uvZip = Join-Path $env:TEMP "uv-$UvVersion-win64.zip"
$uvUrl = "https://github.com/astral-sh/uv/releases/download/$UvVersion/uv-x86_64-pc-windows-msvc.zip"
Invoke-WebRequest -Uri $uvUrl -OutFile $uvZip -UseBasicParsing
$uvExtract = Join-Path $env:TEMP "uv-extract"
if (Test-Path $uvExtract) { Remove-Item -Recurse -Force $uvExtract }
Expand-Archive -Path $uvZip -DestinationPath $uvExtract -Force
$uvExe = Get-ChildItem $uvExtract -Recurse -Filter "uv.exe" | Select-Object -First 1
if (-not $uvExe) { throw "uv.exe not found in downloaded archive" }
Copy-Item $uvExe.FullName (Join-Path $binDir "uv.exe")
Remove-Item $uvZip, $uvExtract -Recurse -Force -ErrorAction SilentlyContinue

# uv license notice (MIT/Apache-2.0 dual license)
@"
This folder bundles uv.exe from https://github.com/astral-sh/uv
uv is dual-licensed under MIT and Apache-2.0:
  https://github.com/astral-sh/uv/blob/main/LICENSE-MIT
  https://github.com/astral-sh/uv/blob/main/LICENSE-APACHE
"@ | Set-Content -Path (Join-Path $stage "LICENSE-uv.txt") -Encoding Ascii

# --- launchers + docs ------------------------------------------------------
Write-Host "==> Adding launchers"
Copy-Item (Join-Path $repoRoot "scripts\portable\run.bat") $stage
Copy-Item (Join-Path $repoRoot "scripts\portable\doctor.bat") $stage
Copy-Item (Join-Path $repoRoot "scripts\portable\_env.cmd") $stage
Copy-Item (Join-Path $repoRoot "scripts\portable\README.txt") $stage

# --- zip -------------------------------------------------------------------
Write-Host "==> Creating $zipPath"
if (Test-Path $zipPath) { Remove-Item -Force $zipPath }
New-Item -ItemType Directory -Force -Path (Split-Path -Parent $zipPath) | Out-Null
Compress-Archive -Path $stage -DestinationPath $zipPath -CompressionLevel Optimal

$sizeMb = [math]::Round((Get-Item $zipPath).Length / 1MB, 1)
Write-Host ""
Write-Host "Portable build complete: $zipPath ($sizeMb MB)" -ForegroundColor Green
Write-Host "Stage folder (for local testing): $stage"
