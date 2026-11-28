# Run the real Windows renderer on a seeded, disposable library. The fixture
# process is the service test executable built by cargo test --no-run.
param(
    [Parameter(Mandatory)][string]$Gui,
    [Parameter(Mandatory)][string]$FixtureService,
    [Parameter(Mandatory)][string]$Media,
    [ValidateRange(0.75, 2.0)][double]$Scale = 1,
    [switch]$Interactions,
    [string]$OutputRoot = (Join-Path $PSScriptRoot '..\.clipvault-local\qa')
)
$ErrorActionPreference = 'Stop'
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
foreach ($name in 'CLIPVAULT_TEST_FIXTURE','CLIPVAULT_DATA_DIR','CLIPVAULT_RESIZE_CHECK','CLIPVAULT_RESIZE_SCALE','CLIPVAULT_INTERACTION_CHECK','TEMP','TMP') {
    $environment[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
}
try {
    if ($Interactions) { $env:CLIPVAULT_INTERACTION_CHECK = '1' }
    $env:CLIPVAULT_TEST_FIXTURE = $mediaFile
    $env:CLIPVAULT_DATA_DIR = $profile
    $env:TEMP = [IO.Directory]::CreateDirectory((Join-Path $round 'tmp')).FullName
    $env:TMP = $env:TEMP
    $stdout = Join-Path $round 'fixture.log'
    $service = Start-Process -FilePath $fixtureFile -WorkingDirectory $round -WindowStyle Hidden -PassThru -RedirectStandardOutput $stdout -RedirectStandardError (Join-Path $round 'fixture.err') -ArgumentList '--exact','test_ipc::serve_isolated_profile','--ignored','--nocapture','--test-threads=1'
    $deadline = [datetime]::UtcNow.AddSeconds(30)
    while ($true) {
        if ($service.HasExited -or [datetime]::UtcNow -gt $deadline) { throw "Fixture failed to start. See $round." }
        if ([string](Get-Content -LiteralPath $stdout -Raw) -match 'Ready for isolated GUI testing') { break }
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
