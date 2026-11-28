# Generate a fresh media kit for independent Editor reviews using the GUI test harness.
[CmdletBinding()]
param([Parameter(Mandatory)][string]$GuiTests)
$ErrorActionPreference = 'Stop'
$root = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$binary = (Resolve-Path -LiteralPath $GuiTests).Path
$kit = Join-Path $root ('.clipvault-local\qa\editor-kit-' + [guid]::NewGuid().ToString('N').Substring(0,8))
[void][IO.Directory]::CreateDirectory($kit)
$profile = [IO.Directory]::CreateDirectory((Join-Path $kit 'profile')).FullName
$temp = [IO.Directory]::CreateDirectory((Join-Path $kit 'tmp')).FullName
$inputs = [IO.Directory]::CreateDirectory((Join-Path $kit 'inputs')).FullName
$psi = [Diagnostics.ProcessStartInfo]::new()
$psi.FileName = $binary
$psi.Arguments = 'native_player::tests::review_footage:: --ignored --nocapture --test-threads=1'
$psi.WorkingDirectory = $root
$psi.UseShellExecute = $false
$psi.CreateNoWindow = $true
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.EnvironmentVariables['CLIPVAULT_EDITOR_INPUTS'] = $inputs
$psi.EnvironmentVariables['CLIPVAULT_DATA_DIR'] = $profile
$psi.EnvironmentVariables['TMP'] = $temp
$psi.EnvironmentVariables['TEMP'] = $temp
$proc = [Diagnostics.Process]::Start($psi)
$stdout = $proc.StandardOutput.ReadToEndAsync()
$stderr = $proc.StandardError.ReadToEndAsync()
$proc.WaitForExit()
$receipt = $stdout.Result + $stderr.Result
[IO.File]::WriteAllText((Join-Path $kit 'generation.log'), $receipt)
if ($proc.ExitCode -or $receipt -notmatch '2 passed; 0 failed') { throw "Media generation failed. See $kit." }

# Windows Speech writes to a WAV endpoint, so the spoken fixture never plays through the speakers.
$voice = $null
$stream = $null
try {
    $stream = New-Object -ComObject SAPI.SpFileStream
    $stream.Open((Join-Path $inputs 'Voiceover.wav'),3,$false)
    $voice = New-Object -ComObject SAPI.SpVoice
    $voice.AudioOutputStream = $stream
    [void]$voice.Speak('Watch the pointer move across the screen. First, choose a clip. Then bring the music down so the instructions stay clear.')
} finally {
    if ($stream) { $stream.Close() }
    if ($voice) { [void][Runtime.InteropServices.Marshal]::FinalReleaseComObject($voice) }
    if ($stream) { [void][Runtime.InteropServices.Marshal]::FinalReleaseComObject($stream) }
}
Write-Output $inputs
