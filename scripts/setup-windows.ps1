<#
.SYNOPSIS
    Old name of the Windows installer, kept so the 0.5 one-liner keeps working:
        powershell -c "irm https://raw.githubusercontent.com/Gnathonic/mokuro-bunko/main/scripts/setup-windows.ps1 | iex"

.DESCRIPTION
    mokuro-bunko 0.7 is a single program: there is no source download, Python or OCR
    environment to set up any more. This forwards to scripts/install.ps1, mapping the
    0.5 options: every -Backend installs the full build (`mokuro-bunko install-ocr`
    then picks the OCR pack: CUDA on an NVIDIA machine); -Ref and -SkipOcr are
    ignored. -InstallDir is passed through only when given (0.5's default,
    %LOCALAPPDATA%\Programs\mokuro-bunko, held a source tree).
#>
[CmdletBinding()]
param(
    [string]$InstallDir = "",
    [string]$Ref = "",
    [ValidateSet("auto", "cuda", "rocm", "cpu")]
    [string]$Backend = "auto",
    [switch]$SkipOcr,
    [switch]$NoShortcut,
    [switch]$NoStart,
    [switch]$NonInteractive
)

$ErrorActionPreference = "Stop"
# 0.7 has one full build for every GPU (install.ps1 takes full or lite); the CUDA
# or ROCm part is the OCR pack that `mokuro-bunko install-ocr` downloads.
$params = @{ Flavor = "full" }
if ($Backend -ne "auto") { Write-Host "Note: -Backend $Backend now means the full build; run 'mokuro-bunko install-ocr' afterwards to install the OCR pack for this machine." -ForegroundColor Yellow }
if ($InstallDir) { $params.InstallDir = $InstallDir }
if ($NoShortcut) { $params.NoShortcut = $true }
if ($NoStart) { $params.NoStart = $true }
if ($NonInteractive) { $params.NonInteractive = $true }
if ($Ref -or $SkipOcr) { Write-Host "Note: -Ref and -SkipOcr no longer apply and are ignored." -ForegroundColor Yellow }

$local = if ($PSScriptRoot) { Join-Path $PSScriptRoot "install.ps1" } else { $null }
if ($local -and (Test-Path $local)) {
    & $local @params
} else {
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    $code = Invoke-RestMethod "https://raw.githubusercontent.com/Gnathonic/mokuro-bunko/main/scripts/install.ps1"
    & ([scriptblock]::Create($code)) @params
}
