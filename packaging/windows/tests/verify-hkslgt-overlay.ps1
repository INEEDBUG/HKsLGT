[CmdletBinding()]
param(
  [Parameter(Mandatory)][string]$Installer,
  [Parameter(Mandatory)][ValidatePattern('^\d+\.\d+\.\d+-beta\.\d+$')][string]$Version,
  [Parameter(Mandatory)][ValidatePattern('^[a-fA-F0-9]{64}$')][string]$ExecutableSha256
)

$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true
if (!$IsWindows -or $env:GITHUB_ACTIONS -ne 'true' -or $env:RUNNER_ENVIRONMENT -ne 'github-hosted') {
  throw 'League overlay verification requires an ephemeral GitHub-hosted Windows runner.'
}

function Run-Installer([string]$File, [string]$Arguments) {
  $result = Start-Process -FilePath $File -ArgumentList $Arguments -WindowStyle Hidden -Wait -PassThru
  if ($result.ExitCode -ne 0) { throw "League installer returned $($result.ExitCode)." }
}

function Assert-Install([string]$ExpectedVersion) {
  $version = Get-ItemPropertyValue -LiteralPath $uninstallKey -Name DisplayVersion
  $location = (Get-ItemPropertyValue -LiteralPath $installKey -Name InstallLocation).Trim('"')
  if ($version -cne $ExpectedVersion -or $location -ine $program -or !(Test-Path -LiteralPath $executable -PathType Leaf)) {
    throw 'The installed version or program location did not match.'
  }
}

function Assert-Profile {
  $savedLocation = [IO.File]::ReadAllText($locator, [Text.Encoding]::Unicode).Trim()
  if ($savedLocation -ine $profile -or (Get-FileHash -LiteralPath $settings).Hash -ne $settingsHash) {
    throw 'The existing data directory or settings changed.'
  }
  python -c "import sqlite3,sys; db=sqlite3.connect(sys.argv[1]); assert db.execute('select value from hkslgt_overlay_fixture').fetchall()==[('preserve-me',)]; db.close()" $database
}

$installer = (Get-Item -LiteralPath $Installer).FullName
if ((Split-Path -Leaf $installer) -cne "MaxGameStudioLeague_$($Version)_x64-setup.exe") {
  throw 'Unexpected League installer name.'
}
$identity = 'b1d9ab62-895c-574e-a779-36d3f2ee3611'
$installKey = "HKCU:\Software\$identity"
$uninstallKey = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\$identity"
$control = Join-Path $env:APPDATA 'MaxGameStudioLeague'
$scratch = Join-Path $env:RUNNER_TEMP ('hkslgt-overlay-' + [Guid]::NewGuid().ToString('n'))
$program = Join-Path $scratch 'Program'
$profile = Join-Path $scratch 'Profile'
$executable = Join-Path $program 'MaxGameStudioLeagueToolbox.exe'
$uninstaller = Join-Path $program 'Uninstall MaxGameStudioLeagueToolbox.exe'
$locator = Join-Path $control 'data-location.txt'
$settings = Join-Path $profile 'base-config.json'
$database = Join-Path $profile 'LeagueAkari.db'
if (@($installKey, $uninstallKey, $control, $scratch | Where-Object { Test-Path -LiteralPath $_ }).Count) {
  throw 'Expected a clean disposable fixture.'
}
$null = New-Item -ItemType Directory -Path $scratch, $profile, $control
$baselineVersion = '1.0.0-beta.5'
$oldName = "MaxGameStudioLeague_$($baselineVersion)_x64-setup.exe"
$baseline = Join-Path $scratch $oldName
$baselineUri = "https://github.com/INEEDBUG/MaxGameStudio/releases/download/league-v$baselineVersion/$oldName"
Invoke-WebRequest -Uri $baselineUri -OutFile $baseline -TimeoutSec 100
if ((Get-FileHash -LiteralPath $baseline).Hash -ine 'd34f181569e03f8079e48d9a080706eadd6943205a08f3669d5f1f802ec1c3d4') {
  throw 'The released League beta.5 installer did not match its pinned SHA-256.'
}

$installArgs = "/S /currentuser /D=$program"
Run-Installer $baseline $installArgs
Assert-Install $baselineVersion
if (!(Test-Path -LiteralPath (Join-Path $program 'resources/app.asar') -PathType Leaf)) {
  throw 'The old Electron installation was incomplete.'
}

[IO.File]::WriteAllText($locator, $profile, [Text.Encoding]::Unicode)
'{"locale":"zh-CN"}' | Set-Content -LiteralPath $settings -Encoding utf8
$settingsHash = (Get-FileHash -LiteralPath $settings).Hash
python -c "import sqlite3,sys; db=sqlite3.connect(sys.argv[1]); db.execute('create table hkslgt_overlay_fixture(value text)'); db.execute('insert into hkslgt_overlay_fixture values (?)',('preserve-me',)); db.commit(); db.close()" $database
Assert-Profile

Run-Installer $installer $installArgs
Assert-Install $Version
Assert-Profile
if ((Get-FileHash -LiteralPath $executable).Hash -ine $ExecutableSha256 -or
    (Test-Path -LiteralPath (Join-Path $program 'resources/app.asar'))) {
  throw 'The new binary did not match, or the old Electron payload was left behind.'
}

$process = Start-Process -FilePath $executable -WorkingDirectory $program -PassThru
try {
  $until = [DateTime]::UtcNow.AddSeconds(30)
  do {
    if ($process.HasExited) { throw "Installed League exited with $($process.ExitCode)." }
    $process.Refresh()
    $rendered = (Test-Path -LiteralPath (Join-Path $profile 'WebView2/EBWebView') -PathType Container) -and
      $process.MainWindowHandle -ne [IntPtr]::Zero
    if ($rendered) { break }
    Start-Sleep -Milliseconds 350
  } while ([DateTime]::UtcNow -lt $until)
  if (!$rendered) { throw 'The upgraded desktop window and WebView2 profile were not observed.' }
  Assert-Profile
} finally {
  if (!$process.HasExited) {
    $process.Kill()
    if (!$process.WaitForExit(8000)) { throw 'The owned fixture process did not exit.' }
  }
}

Run-Installer $uninstaller '/S /currentuser'
Assert-Profile
if ((Test-Path -LiteralPath $installKey) -or (Test-Path -LiteralPath $uninstallKey) -or (Test-Path -LiteralPath $executable)) {
  throw 'Uninstall did not remove the upgraded program and registration.'
}
Write-Host "HKSLGT_OVERLAY_VERIFIED: $baselineVersion -> $Version, custom program and data paths, settings and SQLite data preserved, startup and uninstall verified."
