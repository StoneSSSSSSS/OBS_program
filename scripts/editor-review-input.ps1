<#
.SYNOPSIS
Sends a command to an isolated ClipVault review started with -Driver.
.DESCRIPTION
Pass the launcher's Evidence path as -Run. Keep each review window open and
unminimized. Its client area receives input from this script. Coordinates and
scroll deltas use the PNG's physical pixels, measured from the top left of the
image. After resizing or moving the window to another monitor, take a new
snapshot. The resize result reports the actual size, including OS clamping.

Dialog queues the response to the next supported picker. Click the normal
button after queuing it. For open/import, existing files are required. Use
-Save for a save/export destination, or omit -Paths to cancel. All file paths
are relative to -Run. Use one caller per review and wait for each command.
.EXAMPLE
.\scripts\editor-review-input.ps1 -Run $review.Evidence -Action Snapshot
.EXAMPLE
.\scripts\editor-review-input.ps1 -Run $review.Evidence -Action Click -At 94,210
.EXAMPLE
.\scripts\editor-review-input.ps1 -Run $review.Evidence -Action Drag -At 94,210 -To 330,210
.EXAMPLE
.\scripts\editor-review-input.ps1 -Run $review.Evidence -Action Scroll -At 400,450 -Delta 0,-300
.EXAMPLE
.\scripts\editor-review-input.ps1 -Run $review.Evidence -Action Key -Key A -Ctrl
.EXAMPLE
.\scripts\editor-review-input.ps1 -Run $review.Evidence -Action Text -Text 'New title'
.EXAMPLE
.\scripts\editor-review-input.ps1 -Run $review.Evidence -Action Drop -At 480,440 -Paths 'inputs\Court replay.mp4'
.EXAMPLE
.\scripts\editor-review-input.ps1 -Run $review.Evidence -Action Dialog -Save -Paths 'output\highlight.mp4'
.EXAMPLE
.\scripts\editor-review-input.ps1 -Run $review.Evidence -Action Resize -Width 1100 -Height 700
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Run,
    [Parameter(Mandatory)][ValidateSet('Snapshot','Click','Drag','Scroll','Key','Text','Drop','Resize','Dialog')][string]$Action,
    [int[]]$At,
    [int[]]$To,
    [double[]]$Delta,
    [ValidateSet('primary','secondary','middle')][string]$Button = 'primary',
    [ValidateRange(1,2)][int]$Count = 1,
    [ValidateRange(2,60)][int]$Steps = 10,
    [string]$Key,
    [switch]$Ctrl,
    [switch]$Alt,
    [switch]$Shift,
    [string]$Text,
    [string[]]$Paths = @(),
    [switch]$Save,
    [int]$Width,
    [int]$Height,
    [ValidateRange(1,60)][int]$TimeoutSeconds = 10
)
$ErrorActionPreference = 'Stop'

function Assert-ReviewPoint($Point, $Argument) {
    if ($null -eq $Point -or $Point.Count -ne 2 -or $Point[0] -lt 0 -or $Point[1] -lt 0) {
        throw "Supply $Argument as two nonnegative PNG coordinates, e.g. 80,240."
    }
}
function Assert-PrivateDirectory($Path) {
    $item = Get-Item -LiteralPath $Path -Force
    if (!$item.PSIsContainer -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        throw "Expected an ordinary review directory: $Path"
    }
}
function Wait-ReviewFile($Path, $Deadline) {
    while (!(Test-Path -LiteralPath $Path -PathType Leaf)) {
        if ([DateTime]::UtcNow -gt $Deadline) { throw "No response at $Path. Check that the review window is running and unminimized." }
        Start-Sleep -Milliseconds 40
    }
    $item = Get-Item -LiteralPath $Path -Force
    if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw "Unexpected linked response: $Path" }
}

$project = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$runPath = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Run)
$runPath = [IO.Path]::GetFullPath($runPath).TrimEnd('\','/')
$reviews = Join-Path $project '.clipvault-local\qa\editor-reviews'
$runName = Split-Path -Leaf $runPath
if (!(Split-Path -Parent $runPath).Equals($reviews, [StringComparison]::OrdinalIgnoreCase) -or
    $runName -cnotmatch '^[a-z0-9-]{1,40}-[a-f0-9]{8}$') {
    throw 'Pass the Evidence path from this project''s start-editor-review.ps1.'
}
$mail = Join-Path $runPath 'driver'
foreach ($dir in (Join-Path $project '.clipvault-local'), (Join-Path $project '.clipvault-local\qa'), $reviews, $runPath, $mail) {
    Assert-PrivateDirectory $dir
}
$request = @{ action = $Action.ToLowerInvariant() }
switch ($Action) {
    'Click' {
        Assert-ReviewPoint $At '-At'
        $request.at = $At
        $request.button = $Button.ToLowerInvariant()
        $request.count = $Count
    }
    'Drag' {
        Assert-ReviewPoint $At '-At'
        Assert-ReviewPoint $To '-To'
        $request.from = $At
        $request.to = $To
        $request.button = $Button.ToLowerInvariant()
        $request.steps = $Steps
    }
    'Scroll' {
        Assert-ReviewPoint $At '-At'
        if ($null -eq $Delta -or $Delta.Count -ne 2) { throw 'Supply -Delta as x,y. Use a negative y to scroll down.' }
        $request.at = $At
        $request.delta = $Delta
    }
    'Key' {
        if (!$Key) { throw 'Supply an egui key name with -Key.' }
        $request.key = $Key
        $request.ctrl = [bool]$Ctrl
        $request.alt = [bool]$Alt
        $request.shift = [bool]$Shift
    }
    'Text' {
        if (!$PSBoundParameters.ContainsKey('Text')) { throw 'Supply -Text.' }
        $request.text = $Text
    }
    'Drop' {
        Assert-ReviewPoint $At '-At'
        if (!$Paths.Count) { throw 'Supply one or more synthetic files with -Paths.' }
        $request.at = $At
        $request.paths = $Paths
    }
    'Dialog' {
        $request.save = [bool]$Save
        $request.paths = $Paths
    }
    'Resize' {
        if ($Width -lt 1 -or $Height -lt 1) { throw 'Supply -Width and -Height in physical pixels.' }
        $request.width = $Width
        $request.height = $Height
    }
}

Wait-ReviewFile (Join-Path $mail 'ready.json') ([DateTime]::UtcNow.AddSeconds($TimeoutSeconds))
$id = [guid]::NewGuid().ToString('N')
$temp = Join-Path $mail "$id.request.tmp"
$pending = Join-Path $mail "$id.request.json"
$resultPath = Join-Path $mail "$id.result.json"
$json = ConvertTo-Json -InputObject $request -Compress
[IO.File]::WriteAllText($temp, $json, [Text.UTF8Encoding]::new($false))
[IO.File]::Move($temp, $pending)
try {
    Wait-ReviewFile $resultPath ([DateTime]::UtcNow.AddSeconds($TimeoutSeconds))
} catch {
    if (Test-Path -LiteralPath $pending -PathType Leaf) { Remove-Item -LiteralPath $pending }
    throw
}
$response = ConvertFrom-Json -InputObject ([IO.File]::ReadAllText($resultPath))
if (!$response.ok) { throw $response.error }
if ($Action -eq 'Snapshot') {
    $response.result.image = Join-Path $mail "$id.png"
}
$response.result
