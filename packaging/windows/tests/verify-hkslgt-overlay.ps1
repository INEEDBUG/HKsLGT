[CmdletBinding()]
param(
  [Parameter(Mandatory)][string]$Installer,
  [Parameter(Mandatory)][ValidatePattern('^\d+\.\d+\.\d+-beta\.\d+$')][string]$Version,
  [Parameter(Mandatory)][ValidatePattern('^[a-fA-F0-9]{64}$')][string]$ExecutableSha256,
  [ValidateSet('MaxGameStudio','HKsLGT')][string]$BaselineRepository = 'MaxGameStudio',
  [ValidatePattern('^\d+\.\d+\.\d+-beta\.\d+$')][string]$BaselineVersion = '1.0.0-beta.5',
  [ValidatePattern('^[a-fA-F0-9]{64}$')][string]$BaselineSha256 = 'd34f181569e03f8079e48d9a080706eadd6943205a08f3669d5f1f802ec1c3d4',
  [string]$BaselineInstaller,
  [Alias('SameVersionBaseline')][string]$PriorCandidateInstaller,
  [ValidateSet('currentuser','allusers')][string]$InstallScope = 'currentuser',
  [switch]$UserToMachine
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

function Assert-Install([string]$ExpectedVersion, [string]$RegistryHive = $hive, [string]$Directory = $program) {
  $version = Get-ItemPropertyValue -LiteralPath "$RegistryHive\Software\Microsoft\Windows\CurrentVersion\Uninstall\$identity" -Name DisplayVersion
  $location = (Get-ItemPropertyValue -LiteralPath "$RegistryHive\Software\$identity" -Name InstallLocation).Trim('"')
  if ($version -cne $ExpectedVersion -or $location -ine $Directory -or !(Test-Path -LiteralPath (Join-Path $Directory 'MaxGameStudioLeagueToolbox.exe') -PathType Leaf)) {
    throw 'The installed version or program location did not match.'
  }
}

function Assert-Profile {
  $savedLocation = [IO.File]::ReadAllText($locator, [Text.Encoding]::Unicode).Trim()
  if ($savedLocation -ine $profile -or (Get-FileHash -LiteralPath $settings).Hash -ne $settingsHash) {
    throw 'The existing data directory or settings changed.'
  }
  $check = @'
import json, sqlite3, sys
with sqlite3.connect(sys.argv[1]) as db:
    assert db.execute("SELECT id, gameId FROM EncounteredGames").fetchall() == [(27, 132)]
    assert json.loads(db.execute("SELECT value FROM Settings WHERE key='app-common-main/locale'").fetchone()[0]) == "zh-CN"
    version = int(db.execute("SELECT value FROM Metadata WHERE key='version'").fetchone()[0])
    assert version in (10, 15)
'@
  python -c $check $database
}

$installer = (Get-Item -LiteralPath $Installer).FullName
if ((Split-Path -Leaf $installer) -cne "MaxGameStudioLeague_$($Version)_x64-setup.exe") {
  throw 'Unexpected League installer name.'
}
$identity = 'b1d9ab62-895c-574e-a779-36d3f2ee3611'
$hive = if ($InstallScope -eq 'allusers') { 'HKLM:' } else { 'HKCU:' }
$installKey = "$hive\Software\$identity"
$uninstallKey = "$hive\Software\Microsoft\Windows\CurrentVersion\Uninstall\$identity"
$control = Join-Path $env:APPDATA 'MaxGameStudioLeague'
$scratch = Join-Path $env:RUNNER_TEMP ('hkslgt-overlay-' + [Guid]::NewGuid().ToString('n'))
$program = if ($InstallScope -eq 'allusers') {
  Join-Path ([Environment]::GetFolderPath('ProgramFiles')) 'MaxGameStudioLeagueToolbox'
} else {
  Join-Path $scratch 'Program'
}
$profile = Join-Path $scratch 'Profile'
$executable = Join-Path $program 'MaxGameStudioLeagueToolbox.exe'
$uninstaller = Join-Path $program 'Uninstall MaxGameStudioLeagueToolbox.exe'
$locator = Join-Path $control 'data-location.txt'
$settings = Join-Path $profile 'base-config.json'
$database = Join-Path $profile 'LeagueAkari.db'
if (@($installKey, $uninstallKey, $control, $scratch, $program | Where-Object { Test-Path -LiteralPath $_ }).Count) {
  throw 'Expected a clean disposable fixture.'
}
$null = New-Item -ItemType Directory -Path $scratch, $profile, $control
$oldName = "MaxGameStudioLeague_$($baselineVersion)_x64-setup.exe"
if ($BaselineInstaller) {
  $baseline = (Get-Item -LiteralPath $BaselineInstaller).FullName
  if ((Split-Path -Leaf $baseline) -cne $oldName) { throw 'Earlier candidate filename did not match its embedded version.' }
} else {
  $baseline = Join-Path $scratch $oldName
  $baselineUri = "https://github.com/INEEDBUG/$BaselineRepository/releases/download/league-v$BaselineVersion/$oldName"
  Invoke-WebRequest -Uri $baselineUri -OutFile $baseline -TimeoutSec 100
}
if ((Get-FileHash -LiteralPath $baseline).Hash -ine $BaselineSha256) {
  throw 'The released League baseline installer did not match its pinned SHA-256.'
}

$installArgs = "/S /$InstallScope /D=$program"
if ($UserToMachine) {
  if ($InstallScope -ne 'allusers' -or $PriorCandidateInstaller) { throw 'Use a fresh per-user predecessor for cross-scope verification.' }
  $oldProgram = Join-Path $scratch 'PerUserProgram'
  if (Test-Path -LiteralPath "HKCU:\Software\$identity") { throw 'Expected a clean per-user registration.' }
  Run-Installer $baseline "/S /currentuser /D=$oldProgram"
  Assert-Install $baselineVersion 'HKCU:' $oldProgram
} else {
  Run-Installer $baseline $installArgs
  Assert-Install $baselineVersion
}
if ($BaselineRepository -eq 'MaxGameStudio' -and !(Test-Path -LiteralPath (Join-Path $program 'resources/app.asar') -PathType Leaf)) {
  throw 'The old Electron installation was incomplete.'
}

[IO.File]::WriteAllText($locator, $profile, [Text.Encoding]::Unicode)
'{"locale":"zh-CN"}' | Set-Content -LiteralPath $settings -Encoding utf8
$settingsHash = (Get-FileHash -LiteralPath $settings).Hash
# Match the legacy v10 database fixture in rust/league-core/tests/profile_database.rs.
$seed = @'
import json, sqlite3, sys
with sqlite3.connect(sys.argv[1]) as db:
    db.executescript("""
      CREATE TABLE Metadata (key VARCHAR NOT NULL PRIMARY KEY, value JSON NOT NULL);
      CREATE TABLE Settings (key VARCHAR NOT NULL PRIMARY KEY, value JSON NOT NULL);
      CREATE TABLE EncounteredGames (id INTEGER PRIMARY KEY, gameId INTEGER NOT NULL);
      INSERT INTO Metadata VALUES ('version', json('10'));
      INSERT INTO EncounteredGames VALUES (27, 132);
    """)
    db.execute("INSERT INTO Settings VALUES (?, ?)", ("app-common-main/locale", json.dumps("zh-CN")))
'@
python -c $seed $database
Assert-Profile

function Test-CandidateOverlay([string]$FromVersion) {
  Run-Installer $installer $installArgs
  Assert-Install $Version
  Assert-Profile
  if ((Get-FileHash -LiteralPath $executable).Hash -ine $ExecutableSha256 -or
      (Test-Path -LiteralPath (Join-Path $program 'resources/app.asar'))) {
    throw 'The new binary did not match, or the old Electron payload was left behind.'
  }

  $stderr = Join-Path $scratch 'startup-stderr.txt'
  $process = Start-Process -FilePath $executable -WorkingDirectory $program -PassThru -RedirectStandardError $stderr
  try {
    $until = [DateTime]::UtcNow.AddSeconds(30)
    do {
      if ($process.HasExited) {
        Get-Content -LiteralPath $stderr -Tail 10 -ErrorAction SilentlyContinue | ForEach-Object {
          $_ -replace '[A-Za-z]:\\[^"\r\n]+', '<runner-path>'
        }
        throw "Installed League exited with $($process.ExitCode)."
      }
      $process.Refresh()
      $rendered = (Test-Path -LiteralPath (Join-Path $profile 'WebView2/EBWebView') -PathType Container) -and
        $process.MainWindowHandle -ne [IntPtr]::Zero
      if ($rendered) { break }
      Start-Sleep -Milliseconds 350
    } while ([DateTime]::UtcNow -lt $until)
    if (!$rendered) { throw 'The upgraded desktop window and WebView2 profile were not observed.' }
    Assert-Profile
    $programBytes = (Get-ChildItem -LiteralPath $program -File -Recurse | Measure-Object -Property Length -Sum).Sum
    Write-Host "HKSLGT_STARTUP_SAMPLE: installedBytes=$programBytes mainProcessWorkingSetBytes=$($process.WorkingSet64)"
    $processes = @(Get-CimInstance Win32_Process -Filter "Name='msedgewebview2.exe' OR ProcessId=$($process.Id)" -Property ProcessId,ParentProcessId,PrivatePageCount)
    $owned = @($processes | Where-Object ProcessId -EQ $process.Id)
    $children = $owned
    while ($children.Count) {
      $children = @($processes | Where-Object { $_.ParentProcessId -in $children.ProcessId -and $_.ProcessId -notin $owned.ProcessId })
      $owned += $children
    }
    $privateBytes = ($owned | Measure-Object PrivatePageCount -Sum).Sum
    Write-Host "HKSLGT_STARTUP_TREE: processes=$($owned.Count) privateBytes=$privateBytes"
    if ($UserToMachine) {
      $oldEntry = Test-Path -LiteralPath "HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\$identity"
      $oldExe = Test-Path -LiteralPath (Join-Path $oldProgram 'MaxGameStudioLeagueToolbox.exe')
      Write-Host "HKSLGT_USER_TO_MACHINE: priorUserEntry=$oldEntry priorUserExecutable=$oldExe dataLocatorAndRecordsVerified=true"
      $shell = New-Object -ComObject WScript.Shell
      foreach ($desktop in 'DesktopDirectory','CommonDesktopDirectory') {
        $folder = [Environment]::GetFolderPath($desktop)
        foreach ($link in Get-ChildItem -LiteralPath $folder -Filter '*.lnk' -File -ErrorAction SilentlyContinue) {
          $target = $shell.CreateShortcut($link.FullName).TargetPath
          if ($target -ieq $executable) { Write-Host "HKSLGT_SHORTCUT: $desktop -> upgraded toolbox" }
          if ($target -ieq (Join-Path $oldProgram 'MaxGameStudioLeagueToolbox.exe')) { Write-Host "HKSLGT_SHORTCUT: $desktop -> previous toolbox" }
        }
      }
    }
  } finally {
    if (!$process.HasExited) {
      $process.Kill()
      if (!$process.WaitForExit(8000)) { throw 'The owned fixture process did not exit.' }
    }
  }

  Run-Installer $uninstaller "/S /$InstallScope"
  Assert-Profile
  if ((Test-Path -LiteralPath $installKey) -or (Test-Path -LiteralPath $uninstallKey) -or (Test-Path -LiteralPath $executable)) {
    throw 'Uninstall did not remove the upgraded program and registration.'
  }
  Write-Host "HKSLGT_OVERLAY_VERIFIED: $FromVersion -> $Version, scope=$InstallScope, registered program location, custom data path, settings and SQLite data preserved, startup and uninstall verified."
}

Test-CandidateOverlay $baselineVersion
if ($PriorCandidateInstaller) {
  $prior = Get-Item -LiteralPath $PriorCandidateInstaller
  $priorVersion = $prior.VersionInfo.ProductVersion
  if ($priorVersion -cnotmatch '^\d+\.\d+\.\d+-beta\.\d+$' -or $prior.Name -cne "MaxGameStudioLeague_$($priorVersion)_x64-setup.exe") {
    throw 'Earlier candidate name and embedded League Beta version must match.'
  }
  Run-Installer $prior.FullName $installArgs
  Assert-Install $priorVersion
  Assert-Profile
  if ((Get-FileHash -LiteralPath $executable).Hash -ieq $ExecutableSha256) {
    throw 'The earlier installer must contain a different executable.'
  }
  Test-CandidateOverlay "$priorVersion (previous candidate)"
}
