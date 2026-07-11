<#
.SYNOPSIS
    One-command Windows setup for mokuro-bunko.

.DESCRIPTION
    Installs everything mokuro-bunko needs on Windows with no prerequisites:
      1. Downloads the mokuro-bunko source (no git required) - or uses the
         repo you are already in.
      2. Installs the uv Python manager (user-scope, no admin).
      3. Installs the server and its Python 3.12 runtime (uv sync).
      4. Installs the OCR engine (GPU/CUDA auto-detected, CPU fallback).
      5. Verifies the install with 'mokuro-bunko doctor'.
      6. Creates a start script + desktop shortcut, starts the server, and
         opens your browser to finish setup (create the admin account).

    Run from anywhere (downloads the source):
        powershell -c "irm https://raw.githubusercontent.com/Gnathonic/mokuro-bunko/main/scripts/setup-windows.ps1 | iex"

    Or from inside a cloned repo:
        .\scripts\setup-windows.ps1

.PARAMETER InstallDir
    Where to install when downloading the source (default:
    %LOCALAPPDATA%\Programs\mokuro-bunko). Ignored when run inside a repo.

.PARAMETER Ref
    Git branch or tag to download (default: main).

.PARAMETER Backend
    OCR backend to install: auto, cuda, rocm, or cpu (default: auto).

.PARAMETER SkipOcr
    Skip OCR engine installation (the server installs it on first launch).

.PARAMETER NoShortcut
    Don't create a desktop shortcut.

.PARAMETER NoStart
    Don't start the server or open the browser at the end.

.PARAMETER NonInteractive
    Automation mode: implies -NoStart and -NoShortcut.
#>
[CmdletBinding()]
param(
    [string]$InstallDir = "$env:LOCALAPPDATA\Programs\mokuro-bunko",
    [string]$Ref = "main",
    [ValidateSet("auto", "cuda", "rocm", "cpu")]
    [string]$Backend = "auto",
    [switch]$SkipOcr,
    [switch]$NoShortcut,
    [switch]$NoStart,
    [switch]$NonInteractive
)

$ErrorActionPreference = "Stop"
# PowerShell 5.1 defaults can exclude TLS 1.2, which GitHub requires.
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

if ($NonInteractive) {
    $NoStart = $true
    $NoShortcut = $true
}

$script:TranscriptPath = Join-Path $env:TEMP "mokuro-bunko-setup.log"
try { Start-Transcript -Path $script:TranscriptPath -Force | Out-Null } catch {}

function Write-Step([string]$Message) {
    Write-Host ""
    Write-Host "==> $Message" -ForegroundColor Cyan
}

function Write-Ok([string]$Message) {
    Write-Host "    $Message" -ForegroundColor Green
}

function Fail([string]$Message) {
    Write-Host ""
    Write-Host "SETUP FAILED: $Message" -ForegroundColor Red
    Write-Host "Full log: $script:TranscriptPath" -ForegroundColor Yellow
    Write-Host "For diagnostics, run: uv run mokuro-bunko doctor" -ForegroundColor Yellow
    try { Stop-Transcript | Out-Null } catch {}
    exit 1
}

try {
    Write-Host "Mokuro Bunko - Windows setup" -ForegroundColor White
    Write-Host "----------------------------"

    # --- 1. Locate or download the source -------------------------------
    Write-Step "Locating mokuro-bunko source"
    $repoDir = $null

    # Already inside a repo? (script run as scripts\setup-windows.ps1, or CWD is the repo)
    $candidates = @()
    if ($PSScriptRoot) { $candidates += (Split-Path -Parent $PSScriptRoot) }
    $candidates += (Get-Location).Path
    foreach ($candidate in $candidates) {
        if ((Test-Path (Join-Path $candidate "pyproject.toml")) -and
            (Test-Path (Join-Path $candidate "src\mokuro_bunko"))) {
            $repoDir = $candidate
            break
        }
    }

    if ($repoDir) {
        Write-Ok "Using existing repo: $repoDir"
    } elseif ((Test-Path (Join-Path $InstallDir "pyproject.toml")) -and
              (Test-Path (Join-Path $InstallDir "src\mokuro_bunko"))) {
        $repoDir = $InstallDir
        Write-Ok "Using existing install: $repoDir"
    } else {
        Write-Host "    Downloading mokuro-bunko ($Ref) to $InstallDir ..."
        $zipUrl = "https://codeload.github.com/Gnathonic/mokuro-bunko/zip/refs/heads/$Ref"
        $zipPath = Join-Path $env:TEMP "mokuro-bunko-$Ref.zip"
        $extractDir = Join-Path $env:TEMP "mokuro-bunko-extract"
        try {
            Invoke-WebRequest -Uri $zipUrl -OutFile $zipPath -UseBasicParsing
        } catch {
            # Branch download failed - try as a tag.
            $zipUrl = "https://codeload.github.com/Gnathonic/mokuro-bunko/zip/refs/tags/$Ref"
            Invoke-WebRequest -Uri $zipUrl -OutFile $zipPath -UseBasicParsing
        }
        if (Test-Path $extractDir) { Remove-Item -Recurse -Force $extractDir }
        Expand-Archive -Path $zipPath -DestinationPath $extractDir -Force
        $inner = Get-ChildItem $extractDir -Directory | Select-Object -First 1
        if (-not $inner) { Fail "Downloaded archive was empty." }
        New-Item -ItemType Directory -Force -Path (Split-Path -Parent $InstallDir) | Out-Null
        if (Test-Path $InstallDir) { Remove-Item -Recurse -Force $InstallDir }
        Move-Item -Path $inner.FullName -Destination $InstallDir
        Remove-Item $zipPath -Force -ErrorAction SilentlyContinue
        Remove-Item $extractDir -Recurse -Force -ErrorAction SilentlyContinue
        $repoDir = $InstallDir
        Write-Ok "Source ready: $repoDir"
    }

    # --- 2. Install uv ---------------------------------------------------
    Write-Step "Checking for uv (Python manager)"
    $uvBin = Join-Path $env:USERPROFILE ".local\bin"
    $env:Path = "$uvBin;$env:Path"
    $uv = Get-Command uv -ErrorAction SilentlyContinue
    if ($uv) {
        Write-Ok "uv found: $($uv.Source)"
    } else {
        Write-Host "    Installing uv (user-scope, no admin needed)..."
        Invoke-RestMethod https://astral.sh/uv/install.ps1 | Invoke-Expression
        $env:Path = "$uvBin;$env:Path"
        $uv = Get-Command uv -ErrorAction SilentlyContinue
        if (-not $uv) { Fail "uv did not install correctly. See $script:TranscriptPath" }
        Write-Ok "uv installed: $($uv.Source)"
    }

    # --- 3. Install the server (uv sync) ---------------------------------
    Write-Step "Installing mokuro-bunko server (Python 3.12 + dependencies)"
    & uv sync --directory $repoDir
    if ($LASTEXITCODE -ne 0) { Fail "uv sync failed (exit $LASTEXITCODE)." }
    Write-Ok "Server installed."

    # --- 4. Install OCR --------------------------------------------------
    if ($SkipOcr) {
        Write-Step "Skipping OCR install (-SkipOcr); the server installs it on first launch"
    } else {
        Write-Step "Installing OCR engine (backend: $Backend) - downloads ~2 GB on first install"
        & uv run --directory $repoDir mokuro-bunko install-ocr --backend $Backend
        if ($LASTEXITCODE -ne 0) { Fail "OCR installation failed (exit $LASTEXITCODE)." }
        Write-Ok "OCR engine installed and verified."
    }

    # --- 5. Verify with doctor -------------------------------------------
    Write-Step "Running diagnostics (mokuro-bunko doctor)"
    & uv run --directory $repoDir mokuro-bunko doctor
    if ($LASTEXITCODE -ne 0) { Fail "Diagnostics reported problems (see above)." }
    Write-Ok "Diagnostics passed."

    # --- 6. Start script + shortcut --------------------------------------
    Write-Step "Creating launcher"
    $startCmd = Join-Path $repoDir "start-mokuro-bunko.cmd"
    $cmdContent = @(
        "@echo off",
        "title Mokuro Bunko",
        "cd /d `"%~dp0`"",
        "set `"Path=%USERPROFILE%\.local\bin;%Path%`"",
        "echo Starting Mokuro Bunko server (close this window to stop it)...",
        "uv run mokuro-bunko serve",
        "pause"
    ) -join "`r`n"
    Set-Content -Path $startCmd -Value $cmdContent -Encoding Ascii
    Write-Ok "Launcher: $startCmd"

    if (-not $NoShortcut) {
        try {
            $desktop = [Environment]::GetFolderPath("Desktop")
            $shell = New-Object -ComObject WScript.Shell
            $shortcut = $shell.CreateShortcut((Join-Path $desktop "Mokuro Bunko.lnk"))
            $shortcut.TargetPath = $startCmd
            $shortcut.WorkingDirectory = $repoDir
            $shortcut.Description = "Start the Mokuro Bunko manga library server"
            $shortcut.Save()
            Write-Ok "Desktop shortcut created."
        } catch {
            Write-Host "    (Could not create desktop shortcut: $($_.Exception.Message))" -ForegroundColor Yellow
        }
    }

    # --- 7. Start server + open browser ----------------------------------
    $serverUrl = "http://127.0.0.1:8080"
    if (-not $NoStart) {
        Write-Step "Starting the server"
        Start-Process -FilePath $startCmd -WorkingDirectory $repoDir
        Write-Host "    Waiting for $serverUrl ..."
        $ready = $false
        for ($i = 0; $i -lt 60; $i++) {
            Start-Sleep -Seconds 1
            try {
                $response = Invoke-WebRequest -Uri $serverUrl -UseBasicParsing -TimeoutSec 2
                if ($response.StatusCode -eq 200) { $ready = $true; break }
            } catch {}
        }
        if ($ready) {
            Write-Ok "Server is up."
            Start-Process $serverUrl
            Write-Ok "Browser opened - finish setup there (create your admin account)."
        } else {
            Write-Host "    Server did not respond within 60s - check the server window." -ForegroundColor Yellow
        }
    }

    # --- Summary ----------------------------------------------------------
    Write-Host ""
    Write-Host "Setup complete!" -ForegroundColor Green
    Write-Host "---------------"
    Write-Host "  Install dir : $repoDir"
    Write-Host "  Start server: double-click 'Mokuro Bunko' on your desktop"
    Write-Host "                or run: $startCmd"
    Write-Host "  Web UI      : $serverUrl  (first visit sets up your admin account)"
    Write-Host "  Storage     : $env:LOCALAPPDATA\mokuro-bunko  (library, config, logs)"
    Write-Host "  Diagnostics : uv run --directory `"$repoDir`" mokuro-bunko doctor"
    Write-Host "  Setup log   : $script:TranscriptPath"
} catch {
    Fail $_.Exception.Message
} finally {
    try { Stop-Transcript | Out-Null } catch {}
}
