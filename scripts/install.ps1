# Installs (or updates) ClipVault for the current user and replaces the old
# Python program in the Startup folder.
#
#   cargo build --release
#   powershell -ExecutionPolicy Bypass -File scripts\install.ps1
#
# Updating restarts the native recorder and briefly interrupts recording.

param(
    [string]$Dest = "$env:LOCALAPPDATA\Programs\ClipVault",
    [switch]$NoStart,
    [switch]$NoDesktopShortcut
)
$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$rel = Join-Path $repo 'target\release'

foreach ($f in 'clipvault.exe', 'clipvault-service.exe') {
    if (-not (Test-Path (Join-Path $rel $f))) { throw "Build first with 'cargo build --release' ($f is missing)." }
}
# Stop the installed ClipVault recorder before replacing its executable.
$serviceExe = [IO.Path]::GetFullPath((Join-Path $Dest 'clipvault-service.exe'))
$guiExe = [IO.Path]::GetFullPath((Join-Path $Dest 'clipvault.exe'))
$runningService = @(Get-Process clipvault-service -ErrorAction SilentlyContinue | Where-Object Path -EQ $serviceExe)
$runningGui = @(Get-Process clipvault -ErrorAction SilentlyContinue | Where-Object Path -EQ $guiExe)
$guiWasOpen = $runningGui.Count -gt 0
foreach ($process in $runningGui) {
    [void]$process.CloseMainWindow()
    if (-not $process.WaitForExit(10000)) { throw 'Close the ClipVault window and run the installer again.' }
}
if ($runningService.Count) {
    & $serviceExe --stop
    foreach ($process in $runningService) {
        if (-not $process.WaitForExit(20000)) { throw 'The recorder is still finishing. Wait for it to exit before updating.' }
    }
}

New-Item -ItemType Directory -Force $Dest, (Join-Path $Dest 'assets') | Out-Null
Copy-Item (Join-Path $rel 'clipvault.exe'), (Join-Path $rel 'clipvault-service.exe') $Dest -Force
Copy-Item (Join-Path $repo 'assets\*.png') (Join-Path $Dest 'assets') -Force
Write-Output "Installed to $Dest"

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
    Write-Output 'ClipVault recorder started.'
    if ($guiWasOpen) {
        Start-Sleep -Seconds 2
        Start-Process (Join-Path $Dest 'clipvault.exe') -WorkingDirectory $Dest
        Write-Output 'Reopened the ClipVault window.'
    }
}
