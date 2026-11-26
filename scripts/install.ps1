# Installs (or updates) ClipVault for the current user and replaces the old
# Python program in the Startup folder.
#
#   cargo build --release
#   powershell -ExecutionPolicy Bypass -File scripts\install.ps1
#
# OBS keeps running (and recording) while the service is swapped.

param(
    [string]$Dest = "$env:LOCALAPPDATA\Programs\ClipVault",
    [string]$Ffmpeg = "",
    [switch]$NoStart,
    [switch]$NoDesktopShortcut
)
$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$rel = Join-Path $repo 'target\release'

foreach ($f in 'clipvault.exe', 'clipvault-service.exe') {
    if (-not (Test-Path (Join-Path $rel $f))) { throw "Build first with 'cargo build --release' ($f is missing)." }
}
$mpv = Join-Path $repo 'third_party\libmpv-2.dll'
if (-not (Test-Path $mpv)) {
    throw "third_party\libmpv-2.dll is missing. Download mpv-dev-x86_64-*.7z from https://github.com/shinchiro/mpv-winbuild-cmake/releases and extract libmpv-2.dll into third_party\."
}

# Stop running ClipVault processes. The service exits without touching OBS.
$services = @(
    (Join-Path $Dest 'clipvault-service.exe'),
    (Join-Path $rel 'clipvault-service.exe'),
    (Join-Path $repo 'target\debug\clipvault-service.exe')
)
foreach ($svc in $services) {
    if (Test-Path $svc) { try { & $svc --stop 2>$null | Out-Null } catch {} }
}
Start-Sleep -Seconds 2
$guiWasOpen = [bool](Get-Process clipvault -ErrorAction SilentlyContinue)
Get-Process clipvault, clipvault-service -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Seconds 1

New-Item -ItemType Directory -Force $Dest, (Join-Path $Dest 'assets') | Out-Null
Copy-Item (Join-Path $rel 'clipvault.exe'), (Join-Path $rel 'clipvault-service.exe') $Dest -Force
Copy-Item $mpv $Dest -Force
Copy-Item (Join-Path $repo 'assets\*.png') (Join-Path $Dest 'assets') -Force
Write-Output "Installed to $Dest"

# Bundle a modern ffmpeg next to the app (PATH may have an ancient one).
if (-not $Ffmpeg) {
    $Ffmpeg = Get-ChildItem 'C:\Program Files' -Directory -Filter 'ffmpeg*' -ErrorAction SilentlyContinue |
        Sort-Object Name -Descending |
        ForEach-Object { Join-Path $_.FullName 'bin' } |
        Where-Object { (Test-Path (Join-Path $_ 'ffmpeg.exe')) -and (Test-Path (Join-Path $_ 'ffprobe.exe')) } |
        Select-Object -First 1
}
if ($Ffmpeg -and (Test-Path (Join-Path $Ffmpeg 'ffmpeg.exe'))) {
    Copy-Item (Join-Path $Ffmpeg 'ffmpeg.exe'), (Join-Path $Ffmpeg 'ffprobe.exe') $Dest -Force
    Write-Output "Bundled ffmpeg from $Ffmpeg"
} else {
    Write-Warning "No ffmpeg 5+ found; set its path in Settings > Advanced."
}

# Remove the old Python program from Startup.
$startup = [Environment]::GetFolderPath('Startup')
$shell = New-Object -ComObject WScript.Shell
Get-ChildItem $startup -Filter *.lnk | ForEach-Object {
    $target = $shell.CreateShortcut($_.FullName).TargetPath
    if ($target -match 'OBS_program\\(main|obs_start)\.py$') {
        Remove-Item $_.FullName
        Write-Output "Removed old startup entry: $($_.Name)"
    }
}

# Startup + Start Menu (+ Desktop) shortcuts.
$shortcutArgs = @('--install-shortcuts')
if (-not $NoDesktopShortcut) { $shortcutArgs += '--desktop' }
& (Join-Path $Dest 'clipvault-service.exe') @shortcutArgs

if (-not $NoStart) {
    Start-Process (Join-Path $Dest 'clipvault-service.exe') -WorkingDirectory $Dest
    Write-Output 'ClipVault service started (it adopts the running OBS).'
    if ($guiWasOpen) {
        Start-Sleep -Seconds 2
        Start-Process (Join-Path $Dest 'clipvault.exe') -WorkingDirectory $Dest
        Write-Output 'Reopened the ClipVault window.'
    }
}
