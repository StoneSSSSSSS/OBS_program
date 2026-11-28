# Run the ignored test binaries from cargo test --release --no-run.
# Use a synthetic ClipVault MP4 with at least 30 minutes and three AAC tracks.
param(
    [Parameter(Mandatory)][string]$FixtureService,
    [Parameter(Mandatory)][string]$FixturePlayer,
    [Parameter(Mandatory)][string]$Media,
    [ValidateRange(0,1800)][int]$CompletionSeconds = 300
)

$ErrorActionPreference = 'Stop'
$project = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$local = (Resolve-Path -LiteralPath (Join-Path $project '.clipvault-local')).Path
$qa = (Resolve-Path -LiteralPath (Join-Path $local 'qa')).Path
$service = (Resolve-Path -LiteralPath $FixtureService).Path
$player = (Resolve-Path -LiteralPath $FixturePlayer).Path
$archive = (Resolve-Path -LiteralPath $Media).Path
foreach ($path in @($service, $player)) {
    if (!$path.StartsWith("$local\", [StringComparison]::OrdinalIgnoreCase) -or [IO.Path]::GetExtension($path) -ne '.exe') {
        throw 'Both isolated test executables must be under .clipvault-local.'
    }
}
if (!$archive.StartsWith("$qa\", [StringComparison]::OrdinalIgnoreCase) -or [IO.Path]::GetExtension($archive) -ne '.mp4') {
    throw 'The synthetic archive must be under .clipvault-local\qa.'
}

$serviceTest = 'test_ipc::window_capture::backfill_bench::measure_archive_backfill'
$playerTest = 'native_player::tests::backfill_bench::play_synthetic_archive'
$run = Join-Path $qa ('backfill-' + [guid]::NewGuid().ToString('N').Substring(0,8))
$null = [IO.Directory]::CreateDirectory($run)
$privateService = Join-Path $run 'fixture-service.exe'
$privatePlayer = Join-Path $run 'fixture-player.exe'
Copy-Item -LiteralPath $service -Destination $privateService
Copy-Item -LiteralPath $player -Destination $privatePlayer
$service = $privateService
$player = $privatePlayer
$envBefore = @{}
foreach ($name in @('CLIPVAULT_DATA_DIR','CLIPVAULT_TEST_FIXTURE','CLIPVAULT_QA_CAPTURE_WINDOW','CLIPVAULT_QA_PLAYER_BIN','CLIPVAULT_QA_COMPLETION_SECS','TEMP','TMP')) {
    $envBefore[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
}
$testProcess = $null
$diskProcess = $null
try {
    $env:TEMP = [IO.Directory]::CreateDirectory((Join-Path $run 'tmp')).FullName
    $env:TMP = $env:TEMP
    $env:CLIPVAULT_DATA_DIR = Join-Path $run 'clipvault-ipc-backfill'
    $env:CLIPVAULT_QA_CAPTURE_WINDOW = '1'
    $env:CLIPVAULT_QA_PLAYER_BIN = $player
    $env:CLIPVAULT_TEST_FIXTURE = $archive
    $env:CLIPVAULT_QA_COMPLETION_SECS = "$CompletionSeconds"
    $tests = @(
        @{ Path = $service; Name = $serviceTest; Label = 'service' },
        @{ Path = $player; Name = $playerTest; Label = 'player' }
    )
    foreach ($test in $tests) {
        $listing = Join-Path $run ($test.Label + '-tests.log')
        $listOptions = @{
            FilePath = $test.Path
            ArgumentList = @('--list', '--exact', $test.Name)
            RedirectStandardOutput = $listing
            RedirectStandardError = Join-Path $run ($test.Label + '-tests.err')
            Wait = $true
            PassThru = $true
            WindowStyle = 'Hidden'
        }
        $listed = Start-Process @listOptions
        if ($listed.ExitCode -ne 0 -or (Get-Content -LiteralPath $listing) -notcontains ($test.Name + ': test')) {
            throw "Expected benchmark test missing in $($test.Path)."
        }
    }
    Get-FileHash -LiteralPath $service,$player,$archive | Export-Csv -LiteralPath (Join-Path $run 'sha256.csv') -NoTypeInformation
    try {
        $drive = [IO.Path]::GetPathRoot($run).TrimEnd('\')
        $diskPaths = (Get-Counter -ListSet PhysicalDisk).PathsWithInstances |
            Where-Object { $_ -match [Regex]::Escape($drive) -and $_ -match '\\Disk (Read|Write) Bytes/sec$' }
        if (@($diskPaths).Count -eq 2) {
            $diskCsv = Join-Path $run 'physical-disk.csv'
            $diskArgs = '"{0}" "{1}" -si 1 -sc 90 -f CSV -o "{2}"' -f $diskPaths[0], $diskPaths[1], $diskCsv
            $launchDisk = @{
                FilePath = 'typeperf.exe'
                ArgumentList = $diskArgs
                RedirectStandardOutput = Join-Path $run 'disk.log'
                RedirectStandardError = Join-Path $run 'disk.err'
                PassThru = $true
                WindowStyle = 'Hidden'
            }
            $diskProcess = Start-Process @launchDisk
        } else {
            Write-Warning 'Physical disk counters were unavailable for the benchmark volume.'
        }
    } catch {
        Write-Warning "Physical disk counters were unavailable: $_"
    }
    $log = Join-Path $run 'service.log'
    $launch = @{
        FilePath = $service
        WorkingDirectory = $project
        RedirectStandardOutput = $log
        RedirectStandardError = Join-Path $run 'service.err'
        ArgumentList = @('--exact', $serviceTest, '--ignored', '--nocapture', '--test-threads=1')
        PassThru = $true
        WindowStyle = 'Hidden'
    }
    $testProcess = Start-Process @launch
    Write-Output "Benchmark evidence in $run"
    $deadline = [DateTime]::UtcNow.AddSeconds(120 + $CompletionSeconds)
    while (!$testProcess.WaitForExit(1000)) {
        if ([DateTime]::UtcNow -gt $deadline) { throw "Benchmark timed out. See $log." }
    }
    Get-Content -LiteralPath $log
    if ($testProcess.ExitCode -ne 0) { Get-Content -LiteralPath (Join-Path $run "service.err"); throw "Backfill benchmark failed. See $log and service.err." }
} finally {
    foreach ($process in @($testProcess, $diskProcess)) {
        if ($null -ne $process -and !$process.HasExited) {
            $process.Kill()
            $process.WaitForExit()
        }
    }
    foreach ($name in $envBefore.Keys) { [Environment]::SetEnvironmentVariable($name,$envBefore[$name],'Process') }
}
