<#
.SYNOPSIS
    Install mokuro-bunko on Windows from a GitHub release.

.DESCRIPTION
    Downloads the release zip for Windows x64, checks its SHA-256 against the
    release manifest (release.json), unpacks it into the install folder, adds
    Start-menu shortcuts (and optionally a logon shortcut), runs
    'mokuro-bunko doctor' and starts the server.

    "Mokuro Bunko" in the Start menu starts the app (mokuro-bunko.exe): an icon by
    the clock with status, pause/resume and the settings, which runs the server for
    you (tray.json). "Mokuro Bunko server (console)" keeps the old console window
    (run.bat). In a terminal, use mokuro-bunko-cli.

    No admin rights, no Python, nothing in the registry. The library, config
    and logs stay in %LOCALAPPDATA%\mokuro-bunko (where mokuro-bunko 0.5 kept
    them), unless -Portable keeps them in a data\ folder next to the program.

    Run from anywhere:
        powershell -c "irm https://raw.githubusercontent.com/Gnathonic/mokuro-bunko/main/scripts/install.ps1 | iex"
    With options:
        & ([scriptblock]::Create((irm https://raw.githubusercontent.com/Gnathonic/mokuro-bunko/main/scripts/install.ps1))) -Flavor full -Startup

    Updating: the admin panel installs new releases itself (signed manifest +
    checksum). Running this script again also updates; your data is kept.

.PARAMETER Flavor
    full (default: local OCR; the setup wizard or `mokuro-bunko-cli install-ocr`
    installs CUDA support for an NVIDIA GPU with driver 580+ or the CPU backend).

.PARAMETER Version
    Release to install, e.g. 0.7.0 (default: the latest release).

.PARAMETER InstallDir
    Program folder (default: %LOCALAPPDATA%\mokuro-bunko\app).

.PARAMETER Portable
    Keep PORTABLE.txt: config, library and logs go to <InstallDir>\data.

.PARAMETER Startup
    Also start Mokuro Bunko when you log in: the app (mokuro-bunko.exe), which runs
    the server, through a shortcut in the Startup folder (the same shortcut as the
    tray's "Start at login").

.PARAMETER NoShortcut
    Don't create Start-menu shortcuts.

.PARAMETER NoStart
    Don't start the server at the end.

.PARAMETER NonInteractive
    Automation mode: implies -NoStart; stops a running copy without asking.

.PARAMETER BaseUrl
    Where release.json lives (default: the GitHub release). For testing and mirrors.
#>
[CmdletBinding()]
param(
    [ValidateSet("full")]
    [string]$Flavor = "full",
    [string]$Version = "",
    [string]$InstallDir = "",
    [switch]$Portable,
    [switch]$Startup,
    [switch]$NoShortcut,
    [switch]$NoStart,
    [switch]$NonInteractive,
    [string]$BaseUrl = ""
)

$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"   # Invoke-WebRequest is very slow with the progress bar
# PowerShell 5.1 defaults can exclude TLS 1.2, which GitHub requires.
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

$Repo = if ($env:MOKURO_BUNKO_REPO) { $env:MOKURO_BUNKO_REPO } else { "Gnathonic/mokuro-bunko" }
$Target = "x86_64-pc-windows-msvc"
if (-not $InstallDir) { $InstallDir = Join-Path $env:LOCALAPPDATA "mokuro-bunko\app" }
if ($NonInteractive) { $NoStart = $true }
$Version = $Version.TrimStart("v")

function Write-Step([string]$Message) { Write-Host ""; Write-Host "==> $Message" -ForegroundColor Cyan }
function Write-Ok([string]$Message) { Write-Host "    $Message" -ForegroundColor Green }
# Never `exit` while running under `irm | iex`: that would close the user's window.
function Fail([string]$Message) { throw [System.InvalidOperationException]::new($Message) }

try {
    Write-Host "Mokuro Bunko - Windows install" -ForegroundColor White

    switch ($env:PROCESSOR_ARCHITECTURE) {
        "AMD64" {}
        "ARM64" { Write-Host "    Windows on ARM: installing the x64 build (runs under emulation)." -ForegroundColor Yellow }
        default { Fail "mokuro-bunko needs 64-bit Windows (this is $($env:PROCESSOR_ARCHITECTURE))." }
    }

    # --- 1. Release manifest ----------------------------------------------
    if ($BaseUrl) { $base = $BaseUrl.TrimEnd("/") }
    elseif ($Version) { $base = "https://github.com/$Repo/releases/download/v$Version" }
    else { $base = "https://github.com/$Repo/releases/latest/download" }
    Write-Step "Reading the release manifest ($base/release.json)"
    $tmp = Join-Path ([IO.Path]::GetTempPath()) ("mokuro-bunko-install-" + [Guid]::NewGuid().ToString("N"))
    New-Item -ItemType Directory -Force -Path $tmp | Out-Null
    $manifestPath = Join-Path $tmp "release.json"
    Invoke-WebRequest -Uri "$base/release.json" -OutFile $manifestPath -UseBasicParsing
    # The ed25519 signature (release.json.sig) is checked by mokuro-bunko itself when it
    # updates; Windows PowerShell has no ed25519, so here the trust is HTTPS to GitHub
    # plus the manifest's SHA-256 of the zip.
    $manifest = Get-Content -Raw -Path $manifestPath | ConvertFrom-Json
    $flavors = $manifest.artifacts.$Target
    if (-not $flavors) { Fail "release $($manifest.version) has no Windows build." }
    $artifact = $flavors.$Flavor
    if (-not $artifact) {
        $have = ($flavors.PSObject.Properties | ForEach-Object { $_.Name }) -join ", "
        Fail "release $($manifest.version) has no '$Flavor' build for Windows (available: $have)."
    }
    Write-Ok "mokuro-bunko $($manifest.version) ($Flavor)"

    # --- 2. Download and verify -------------------------------------------
    Write-Step "Downloading $($artifact.url)"
    $zipPath = Join-Path $tmp ([IO.Path]::GetFileName(([Uri]$artifact.url).AbsolutePath))
    Invoke-WebRequest -Uri $artifact.url -OutFile $zipPath -UseBasicParsing
    $hash = (Get-FileHash -Algorithm SHA256 -Path $zipPath).Hash
    if ($hash -ne $artifact.sha256) {
        Fail "SHA-256 mismatch: got $hash, the manifest says $($artifact.sha256)."
    }
    Write-Ok "Checksum OK ($([math]::Round((Get-Item $zipPath).Length / 1MB, 1)) MB)"

    $extract = Join-Path $tmp "x"
    Expand-Archive -Path $zipPath -DestinationPath $extract -Force
    $inner = Get-ChildItem -Path $extract -Directory | Select-Object -First 1
    if (-not $inner -or -not (Test-Path (Join-Path $inner.FullName "mokuro-bunko-cli.exe"))) {
        Fail "the zip has no mokuro-bunko-cli.exe."
    }

    # --- 3. Install ---------------------------------------------------------
    Write-Step "Installing into $InstallDir"
    # The console build runs the commands below; the app is what shortcuts start.
    $exe = Join-Path $InstallDir "mokuro-bunko-cli.exe"
    $trayExe = Join-Path $InstallDir "mokuro-bunko.exe"
    $running = Get-Process -Name "mokuro-bunko", "mokuro-bunko-cli" -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -and (($_.Path -ieq $exe) -or ($_.Path -ieq $trayExe)) }
    if ($running) {
        if (-not $NonInteractive) {
            $answer = Read-Host "    mokuro-bunko is running from $InstallDir. Stop it to update? [Y/n]"
            if ($answer -and $answer -notmatch "^[Yy]") { Fail "stop the running server first." }
        }
        $running | Stop-Process -Force
        Start-Sleep -Seconds 2
        Write-Ok "Stopped the running server."
    }
    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    # Copy over the old version; data\ (portable mode) and anything else of yours stays.
    Copy-Item -Path (Join-Path $inner.FullName "*") -Destination $InstallDir -Recurse -Force
    if (-not $Portable) {
        Remove-Item -Force -ErrorAction SilentlyContinue (Join-Path $InstallDir "PORTABLE.txt")
    }
    $versionText = & $exe --version
    if ($LASTEXITCODE -ne 0) { Fail "the installed mokuro-bunko.exe does not run (exit $LASTEXITCODE)." }
    Write-Ok "$versionText"

    # --- 4. Shortcuts -------------------------------------------------------
    $runBat = Join-Path $InstallDir "run.bat"
    $shell = New-Object -ComObject WScript.Shell
    $hasTray = $true
    $iconFile = Join-Path $InstallDir "mokuro-bunko.ico"
    if (-not (Test-Path $iconFile)) { $iconFile = $exe }
    function New-Shortcut([string]$Path, [string]$Target, [string]$Description, [int]$WindowStyle = 1) {
        $s = $shell.CreateShortcut($Path)
        $s.TargetPath = $Target
        $s.WorkingDirectory = $InstallDir
        $s.Description = $Description
        $s.WindowStyle = $WindowStyle
        $s.IconLocation = "$iconFile,0"
        $s.Save()
    }
    if ($hasTray) {
        # What the tray runs (GUI.md §5): the library server, as the Start-menu shortcut
        # did before the tray. Kept if it exists (the setup wizard may have changed it).
        $trayDir = if ($Portable) { Join-Path $InstallDir "data" } else { Join-Path $env:LOCALAPPDATA "mokuro-bunko" }
        $trayJson = Join-Path $trayDir "tray.json"
        if (-not (Test-Path $trayJson)) {
            New-Item -ItemType Directory -Force -Path $trayDir | Out-Null
            Set-Content -Path $trayJson -Encoding ASCII -Value '{ "managed": [ { "role": "server" } ] }'
            Write-Ok "The tray runs the server: $trayJson"
        }
    }
    if (-not $NoShortcut) {
        $menu = Join-Path ([Environment]::GetFolderPath("Programs")) "Mokuro Bunko"
        New-Item -ItemType Directory -Force -Path $menu | Out-Null
        if ($hasTray) {
            New-Shortcut (Join-Path $menu "Mokuro Bunko.lnk") $trayExe "Mokuro Bunko: status, pause and settings (tray icon)"
            New-Shortcut (Join-Path $menu "Mokuro Bunko server (console).lnk") $runBat "Run the Mokuro Bunko server in a console window"
        } else {
            New-Shortcut (Join-Path $menu "Mokuro Bunko.lnk") $runBat "Start the Mokuro Bunko manga library server"
        }
        New-Shortcut (Join-Path $menu "Mokuro Bunko diagnostics.lnk") (Join-Path $InstallDir "doctor.bat") "Diagnose Mokuro Bunko problems"
        Write-Ok "Start-menu shortcuts: $menu"
    }
    $startupLink = Join-Path ([Environment]::GetFolderPath("Startup")) "Mokuro Bunko.lnk"
    if ($Startup) {
        if ($hasTray) {
            New-Shortcut $startupLink $trayExe "Start Mokuro Bunko (tray) at logon"
        } else {
            New-Shortcut $startupLink $runBat "Start the Mokuro Bunko server at logon" 7   # 7 = minimized
        }
        Write-Ok "Starts at logon: $startupLink"
    }

    # --- 5. Diagnostics -----------------------------------------------------
    Write-Step "Running diagnostics (mokuro-bunko doctor)"
    & $exe doctor
    if ($LASTEXITCODE -ne 0) {
        Write-Host "    doctor reported problems (exit $LASTEXITCODE); see above." -ForegroundColor Yellow
    } else {
        Write-Ok "Diagnostics passed."
    }

    # --- 6. Start -------------------------------------------------------------
    if (-not $NoStart) {
        if ($hasTray) {
            Write-Step "Starting Mokuro Bunko (tray icon by the clock; it starts the server)"
            Write-Host "    Windows 11 may put the icon under the ^ by the clock (hidden icons): drag it onto the taskbar to keep it in view." -ForegroundColor Gray
            Start-Process -FilePath $trayExe -WorkingDirectory $InstallDir
            # Open the browser once the server answers, as run.bat does.
            Start-Process powershell -WindowStyle Hidden -ArgumentList "-NoProfile", "-Command", "for(`$i=0;`$i -lt 120;`$i++){try{`$r=Invoke-WebRequest -Uri 'http://127.0.0.1:8080' -UseBasicParsing -TimeoutSec 2; if(`$r.StatusCode -lt 500){Start-Process 'http://127.0.0.1:8080'; break}}catch{}; Start-Sleep -Seconds 1}"
        } else {
            Write-Step "Starting the server (it opens your browser when ready)"
            Start-Process -FilePath $runBat -WorkingDirectory $InstallDir
        }
    }

    $dataDir = if ($Portable) { Join-Path $InstallDir "data" } else { Join-Path $env:LOCALAPPDATA "mokuro-bunko" }
    Write-Host ""
    Write-Host "Installed mokuro-bunko $($manifest.version) ($Flavor)." -ForegroundColor Green
    Write-Host "  Program     : $InstallDir  (in a terminal: mokuro-bunko-cli)"
    if ($hasTray) {
        Write-Host "  Start       : Start menu > Mokuro Bunko (tray icon), or $runBat (console)"
    } else {
        Write-Host "  Start       : Start menu > Mokuro Bunko, or $runBat"
    }
    Write-Host "  Web UI      : http://127.0.0.1:8080  (first visit creates your admin account)"
    Write-Host "  Data        : $dataDir  (library, config.yaml, logs)"
    Write-Host "  Uninstall   : quit the tray, then delete $InstallDir, the Start-menu folder 'Mokuro Bunko'"
    if ($Portable) {
        Write-Host "                and the shortcut 'Mokuro Bunko' in shell:startup if it is there (copy data\ first: it is your library)"
    } else {
        Write-Host "                and the shortcut 'Mokuro Bunko' in shell:startup if it is there ($dataDir is your library)"
    }
} catch {
    Write-Host ""
    Write-Host "INSTALL FAILED: $($_.Exception.Message)" -ForegroundColor Red
    if ($PSCommandPath) { exit 1 }   # run as a file: report failure through the exit code
} finally {
    if ($tmp -and (Test-Path $tmp)) { Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $tmp }
}
