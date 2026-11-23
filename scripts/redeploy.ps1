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
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'Zapret Tools'),
    [switch]$NoLaunch,
    [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot

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

Write-Host "Installing $($setup.FullName)"
$proc = Start-Process -FilePath $setup.FullName -ArgumentList '/S' -Wait -PassThru
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
