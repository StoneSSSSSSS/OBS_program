<#
.SYNOPSIS
Prepares one existing-project Editor review or inspects an exported MP4.
.DESCRIPTION
Build the operator utility with cargo build --locked --release -p clipvault-media
--example editor_review and pass that executable as -Tool. Pass the launcher's
Evidence directory as -Run. Seed once before handing the review to a tester.
The original input copies stay in place. Paths in the command output are
relative to the same review run.

For inspection, pass a reviewed export under output. Frame timestamps and
audio ranges are in seconds. Frames are decoded at the selected presentation
sample and saved as PNGs. The audio report lists peak level and RMS per track.
.EXAMPLE
.\scripts\editor-review-materials.ps1 -Tool $tool -Run $review.Evidence -Scenario Revision
.EXAMPLE
.\scripts\editor-review-materials.ps1 -Tool $tool -Run $review.Evidence -Scenario LongAssembly
.EXAMPLE
.\scripts\editor-review-materials.ps1 -Tool $tool -Run $review.Evidence -Scenario Relink
.EXAMPLE
.\scripts\editor-review-materials.ps1 -Tool $tool -Run $review.Evidence -Export 'output\demo.mp4' -FrameAt 0.5,1.5 -AudioRange '0.1,0.4','1,1.4'
#>
[CmdletBinding(DefaultParameterSetName='Seed')]
param(
    [Parameter(Mandatory)][string]$Tool,
    [Parameter(Mandatory)][string]$Run,
    [Parameter(Mandatory,ParameterSetName='Seed')]
    [ValidateSet('Revision','LongAssembly','Relink')][string]$Scenario,
    [Parameter(Mandatory,ParameterSetName='Inspect')][string]$Export,
    [Parameter(ParameterSetName='Inspect')][double[]]$FrameAt = @(),
    [Parameter(ParameterSetName='Inspect')][string[]]$AudioRange = @()
)
$ErrorActionPreference = 'Stop'
$project = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$local = Join-Path $project '.clipvault-local'
$runPath = (Resolve-Path -LiteralPath $Run).Path
$binary = (Resolve-Path -LiteralPath $Tool).Path
if (!(Split-Path -Parent $runPath).Equals((Join-Path $local 'qa\editor-reviews'), [StringComparison]::OrdinalIgnoreCase)) {
    throw 'Pass the Evidence path for a review run in this project.'
}
if (!$binary.StartsWith($local + '\', [StringComparison]::OrdinalIgnoreCase) -or
    (Split-Path -Leaf $binary) -ine 'editor_review.exe') {
    throw 'Pass editor_review.exe from this project''s isolated Cargo target directory.'
}
foreach ($part in $local, (Join-Path $local 'qa'), (Join-Path $local 'qa\editor-reviews'), $runPath,
    (Join-Path $runPath 'tmp'), (Join-Path $runPath 'profile')) {
    $item = Get-Item -LiteralPath $part -Force
    if (!$item.PSIsContainer -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        throw "Expected an ordinary review directory: $part"
    }
}

if ($PSCmdlet.ParameterSetName -eq 'Seed') {
    $name = if ($Scenario -eq 'LongAssembly') { 'long_assembly' } else { $Scenario.ToLowerInvariant() }
    $operation = @{ kind = 'seed'; scenario = $name }
} else {
    $ranges = @()
    foreach ($range in $AudioRange) {
        $parts = @($range.Split(','))
        if ($parts.Count -ne 2) { throw "Use start,end seconds for each -AudioRange." }
        $ranges += ,@(
            [double]::Parse($parts[0], [Globalization.CultureInfo]::InvariantCulture),
            [double]::Parse($parts[1], [Globalization.CultureInfo]::InvariantCulture)
        )
    }
    $operation = @{ kind = 'inspect'; output = $Export; frame_at = $FrameAt; audio_ranges = $ranges }
}

$psi = [Diagnostics.ProcessStartInfo]::new()
$psi.FileName = $binary
$psi.WorkingDirectory = $runPath
$psi.UseShellExecute = $false
$psi.CreateNoWindow = $true
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.EnvironmentVariables['CLIPVAULT_DATA_DIR'] = Join-Path $runPath 'profile'
$psi.EnvironmentVariables['TEMP'] = Join-Path $runPath 'tmp'
$psi.EnvironmentVariables['TMP'] = $psi.EnvironmentVariables['TEMP']
$utf8 = [Text.UTF8Encoding]::new($false)
$psi.StandardOutputEncoding = $utf8
$psi.StandardErrorEncoding = $utf8
$request = ConvertTo-Json -InputObject @{ run = $runPath; operation = $operation } -Depth 6 -Compress
$bytes = $utf8.GetBytes($request)
$process = [Diagnostics.Process]::Start($psi)
try {
    $stdout = $process.StandardOutput.ReadToEndAsync()
    $stderr = $process.StandardError.ReadToEndAsync()
    $process.StandardInput.BaseStream.Write($bytes, 0, $bytes.Length)
    $process.StandardInput.Close()
    $process.WaitForExit()
    if ($process.ExitCode) { throw $stderr.Result.Trim() }
    ConvertFrom-Json -InputObject $stdout.Result
} finally {
    $process.Dispose()
}
