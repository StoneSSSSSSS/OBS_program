# Run the real Windows renderer on a seeded, disposable library. The fixture
# process is the service test executable built by cargo test --no-run.
param(
    [Parameter(Mandatory)][string]$Gui,
    [Parameter(Mandatory)][string]$FixtureService,
    [Parameter(Mandatory)][string]$Media,
    [ValidateRange(0.75, 2.0)][double]$Scale = 1,
    [switch]$Interactions,
    [switch]$OwnedWindowCapture,
    [string]$OutputRoot = (Join-Path $PSScriptRoot '..\.clipvault-local\qa')
)
$ErrorActionPreference = 'Stop'
$projectRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$guiFile = (Resolve-Path -LiteralPath $Gui).Path
$fixtureFile = (Resolve-Path -LiteralPath $FixtureService).Path
$mediaFile = (Resolve-Path -LiteralPath $Media).Path
$round = Join-Path ([IO.Path]::GetFullPath($OutputRoot)) ('resize-' + [guid]::NewGuid().ToString('N').Substring(0,8))
[void][IO.Directory]::CreateDirectory($round)
$profile = Join-Path $round 'clipvault-ipc-resize'
$screens = Join-Path $round 'screens'
[void][IO.Directory]::CreateDirectory($screens)
$app = Join-Path $round 'clipvault.exe'
Copy-Item -LiteralPath $guiFile -Destination $app
$service = $null
$window = $null
$environment = @{}
foreach ($name in 'CLIPVAULT_TEST_FIXTURE','CLIPVAULT_DATA_DIR','CLIPVAULT_RESIZE_CHECK','CLIPVAULT_RESIZE_SCALE','CLIPVAULT_INTERACTION_CHECK','CLIPVAULT_QA_CAPTURE_WINDOW','TEMP','TMP') {
    $environment[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
}
try {
    $testName = 'test_ipc::serve_isolated_profile'
    $readyText = 'Ready for isolated GUI testing'
    $serviceDir = $round
    if ($OwnedWindowCapture) {
        $testName = 'test_ipc::window_capture::serve_owned_window_capture'
        $readyText = 'Ready for genuine window-capture UI QA'
        $serviceDir = $projectRoot
        $env:CLIPVAULT_QA_CAPTURE_WINDOW = '1'
    } else {
        [Environment]::SetEnvironmentVariable('CLIPVAULT_QA_CAPTURE_WINDOW',$null,'Process')
    }
    if ($Interactions -or $OwnedWindowCapture) { $env:CLIPVAULT_INTERACTION_CHECK = '1' }
    $env:CLIPVAULT_TEST_FIXTURE = $mediaFile
    $env:CLIPVAULT_DATA_DIR = $profile
    $env:TEMP = [IO.Directory]::CreateDirectory((Join-Path $round 'tmp')).FullName
    $env:TMP = $env:TEMP
    $stdout = Join-Path $round 'fixture.log'
    $service = Start-Process -FilePath $fixtureFile -WorkingDirectory $serviceDir -WindowStyle Hidden -PassThru -RedirectStandardOutput $stdout -RedirectStandardError (Join-Path $round 'fixture.err') -ArgumentList '--exact',$testName,'--ignored','--nocapture','--test-threads=1'
    $deadline = [datetime]::UtcNow.AddSeconds(30)
    while ($true) {
        if ($service.HasExited -or [datetime]::UtcNow -gt $deadline) { throw "Fixture failed to start. See $round." }
        if ([string](Get-Content -LiteralPath $stdout -Raw) -match $readyText) { break }
        Start-Sleep -Milliseconds 100
    }
    $env:CLIPVAULT_RESIZE_CHECK = $screens
    $env:CLIPVAULT_RESIZE_SCALE = $Scale.ToString([Globalization.CultureInfo]::InvariantCulture)
    $window = Start-Process -FilePath $app -WorkingDirectory $round -PassThru
    Write-Output "Testing $Scale UI scale in $round"
    if (!$window.WaitForExit(300000)) { throw "Resize check timed out. See $round." }
    $receipt = Get-Content -LiteralPath (Join-Path $screens 'result.txt') -Raw
    Write-Output $receipt
    if ($window.ExitCode -ne 0 -or !$receipt.StartsWith('PASS:')) { throw "Resize check failed. See $screens." }
    if ($OwnedWindowCapture) {
        if (!$service.WaitForExit(15000)) { throw "Owned-window recorder did not finish. See $round." }
        $serviceLog = Get-Content -LiteralPath $stdout -Raw
        if ($service.ExitCode -ne 0 -or $serviceLog -notmatch '1 passed; 0 failed') {
            throw "Owned-window recording failed. See $stdout."
        }
        Get-Content -LiteralPath (Join-Path $screens 'interaction-result.txt')
    }
} finally {
    foreach ($process in @($window, $service)) {
        if ($null -ne $process -and !$process.HasExited) {
            $process.Kill()
            $process.WaitForExit()
        }
    }
    foreach ($name in $environment.Keys) { [Environment]::SetEnvironmentVariable($name,$environment[$name],'Process') }
    Remove-Item -LiteralPath $app
}
