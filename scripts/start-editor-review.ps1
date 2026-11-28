# Start an isolated interactive Editor review with copies of synthetic input files.
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Gui,
    [Parameter(Mandatory)][ValidatePattern('^[a-z0-9-]{1,40}$')][string]$Brief,
    [string]$Inputs = (Join-Path $PSScriptRoot '..\.clipvault-local\qa\editor-inputs'),
    [switch]$Driver
)
$ErrorActionPreference = 'Stop'
$projectRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$localRoot = (Resolve-Path -LiteralPath (Join-Path $projectRoot '.clipvault-local')).Path
$exe = (Resolve-Path -LiteralPath $Gui).Path
$sourceDir = (Resolve-Path -LiteralPath $Inputs).Path
if (!$sourceDir.StartsWith($localRoot + '\',[StringComparison]::OrdinalIgnoreCase)) {
    throw 'Place the synthetic input files inside .clipvault-local in this project.'
}
$runName = $Brief + '-' + [guid]::NewGuid().ToString('N').Substring(0,8)
$run = Join-Path (Join-Path $localRoot 'qa\editor-reviews') $runName
$folders = @('inputs', 'output', 'profile', 'tmp')
if ($Driver) { $folders += 'driver' }
foreach ($folder in $folders) {
    [void][IO.Directory]::CreateDirectory((Join-Path $run $folder))
}
$inputFiles = @(Get-ChildItem -LiteralPath $sourceDir -File)
if (!$inputFiles.Count) { throw "No input files in $sourceDir" }
$hashes = foreach ($file in $inputFiles) {
    $copy = Join-Path (Join-Path $run 'inputs') $file.Name
    Copy-Item -LiteralPath $file.FullName -Destination $copy
    [pscustomobject]@{ name = $file.Name; sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $copy).Hash }
}
$hashes | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $run 'source-hashes.json') -Encoding utf8
$reviewExe = Join-Path $run 'clipvault.exe'
Copy-Item -LiteralPath $exe -Destination $reviewExe
$runtime = Join-Path (Split-Path -LiteralPath $exe) 'vcruntime140.dll'
if (Test-Path -LiteralPath $runtime) { Copy-Item -LiteralPath $runtime -Destination $run }
# Only the GUI is present here. Editor reviewers can work while the recording service is offline.
$psi = [Diagnostics.ProcessStartInfo]::new()
$psi.FileName = $reviewExe
$psi.WorkingDirectory = $run
$psi.UseShellExecute = $false
$psi.EnvironmentVariables['CLIPVAULT_DATA_DIR'] = Join-Path $run 'profile'
$psi.EnvironmentVariables['TEMP'] = Join-Path $run 'tmp'
$psi.EnvironmentVariables['TMP'] = $psi.EnvironmentVariables['TEMP']
$driverDir = $null
$psi.EnvironmentVariables.Remove('CLIPVAULT_EDITOR_REVIEW_DRIVER')
if ($Driver) {
    $driverDir = Join-Path $run 'driver'
    $psi.EnvironmentVariables['CLIPVAULT_EDITOR_REVIEW_DRIVER'] = $driverDir
}
foreach ($name in 'CLIPVAULT_RESIZE_CHECK','CLIPVAULT_INTERACTION_CHECK','CLIPVAULT_QA_CAPTURE_WINDOW','CLIPVAULT_SELFTEST') {
    $psi.EnvironmentVariables.Remove($name)
}
$process = [Diagnostics.Process]::Start($psi)
[pscustomobject]@{
    ProcessId = $process.Id
    BuildSHA256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $reviewExe).Hash
    Inputs = Join-Path $run 'inputs'
    Output = Join-Path $run 'output'
    Evidence = $run
    Driver = $driverDir
}
