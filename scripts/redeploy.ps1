# Rebuilds the app, installs it over the existing installation and relaunches it.
#
# Settings are stored in %APPDATA%\com.zapret.tools\config.json (outside the
# install folder), so they survive the reinstall untouched.
#
# Usage:
#   npm run redeploy
#   powershell -File scripts/redeploy.ps1 -NoLaunch
#   powershell -File scripts/redeploy.ps1 -SkipBuild   # just reinstall last build

param(
    [string]$InstallDir = '',
    [switch]$NoLaunch,
    [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot

# The installer remembers the folder chosen at install time (this user has
# E:\zapret\Zapret Tools, not the default). Read it from the uninstall entry
# so we stop and relaunch the right copy.
function Get-InstalledDir {
    $entries = @()
    foreach ($base in @(
        'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\*',
        'HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall\*',
        'HKLM:\Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\*'
    )) {
        $entries += Get-ItemProperty $base -ErrorAction SilentlyContinue |
            Where-Object { $_.DisplayName -eq 'Zapret Tools' -and $_.InstallLocation }
    }
    foreach ($entry in $entries) {
        $dir = $entry.InstallLocation.Trim('"')
        if (Test-Path $dir) { return $dir }
    }
    return $null
}

if (-not $InstallDir) { $InstallDir = Get-InstalledDir }
if (-not $InstallDir) { $InstallDir = Join-Path $env:LOCALAPPDATA 'Zapret Tools' }

# Updater signing key: lives in the repo but is gitignored. Without it
# `tauri build` exits 1 after producing the bundles.
if (-not $env:TAURI_SIGNING_PRIVATE_KEY) {
    $key = Join-Path $root 'src-tauri\keys\zapret-tools.key'
    if (Test-Path $key) {
        $env:TAURI_SIGNING_PRIVATE_KEY = (Resolve-Path $key).Path
    }
}
if (-not $env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD) {
    $env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = ''
}

function Get-AppExe {
    param([string]$Dir)
    if (-not (Test-Path $Dir)) { return $null }
    Get-ChildItem -Path $Dir -Filter '*.exe' -File |
        Where-Object { $_.Name -notlike 'uninstall*' } |
        Select-Object -First 1
}

# 1. Stop the installed copy. Only processes started from the install folder
#    are touched, so a portable copy running from elsewhere keeps working.
$appExe = Get-AppExe $InstallDir
if ($appExe) {
    Get-Process -ErrorAction SilentlyContinue | ForEach-Object {
        $p = $_
        try {
            if ($p.Path -and $p.Path.Equals($appExe.FullName, [StringComparison]::OrdinalIgnoreCase)) {
                Write-Host "Stopping PID $($p.Id): $($p.Path)"
                Stop-Process -Id $p.Id -Force
            }
        } catch {
            # Access denied for another user's process - not ours to touch.
        }
    }
    Start-Sleep -Milliseconds 700
}

# 2. Build the NSIS bundle.
if (-not $SkipBuild) {
    Push-Location $root
    try {
        & npm run tauri build
        if ($LASTEXITCODE -ne 0) { throw "tauri build failed with exit code $LASTEXITCODE" }
    } finally {
        Pop-Location
    }
}

# 3. Silent install over the previous version (same folder, same shortcuts).
$nsisDir = Join-Path $root 'src-tauri\target\release\bundle\nsis'
$setup = Get-ChildItem -Path $nsisDir -Filter '*-setup.exe' -File -ErrorAction SilentlyContinue |
    Sort-Object LastWriteTime -Descending |
    Select-Object -First 1
if (-not $setup) { throw "No NSIS installer found in $nsisDir" }

Write-Host "Installing $($setup.FullName) -> $InstallDir"
# /D= must be the last argument and is not quoted (NSIS rule).
$proc = Start-Process -FilePath $setup.FullName -ArgumentList "/S /D=$InstallDir" -Wait -PassThru
if ($proc.ExitCode -ne 0) { throw "Installer exited with code $($proc.ExitCode)" }

# 4. Relaunch the installed copy.
if (-not $NoLaunch) {
    $appExe = Get-AppExe $InstallDir
    if ($appExe) {
        Write-Host "Launching $($appExe.FullName)"
        Start-Process -FilePath $appExe.FullName
    } else {
        Write-Warning "Executable not found in $InstallDir"
    }
}

Write-Host 'Done.'
