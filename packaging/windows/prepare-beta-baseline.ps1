#Requires -Version 7.0
[CmdletBinding()]
param(
  [Parameter(Mandatory)][ValidatePattern('^\d+\.\d+\.\d+-beta\.\d+$')][string]$Version,
  [Parameter(Mandatory)][ValidatePattern('^[a-fA-F0-9]{64}$')][string]$InstallerSha256,
  [Parameter(Mandatory)][string]$CatalogPath
)

$ErrorActionPreference = 'Stop'
if (!$IsWindows -or $env:GITHUB_ACTIONS -ne 'true' -or $env:RUNNER_ENVIRONMENT -ne 'github-hosted') {
  throw 'Predecessor inventory requires an ephemeral GitHub-hosted Windows runner.'
}
. (Join-Path $PSScriptRoot 'league-standalone/retire-user-install.ps1')

$id = 'b1d9ab62-895c-574e-a779-36d3f2ee3611'
$record = "HKCU:\Software\$id"
$arp = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\$id"
if (Test-Path -LiteralPath $record) { throw 'The predecessor fixture requires a clean user installation.' }

$root = Join-Path $env:RUNNER_TEMP ('hkslgt-predecessor-' + [Guid]::NewGuid().ToString('n'))
$null = New-Item -ItemType Directory -Path $root
$program = Join-Path $root 'Program'
$name = "MaxGameStudioLeague_$($Version)_x64-setup.exe"
$installer = Join-Path $root $name
Invoke-WebRequest -Uri "https://github.com/INEEDBUG/HKsLGT/releases/download/league-v$Version/$name" -OutFile $installer -TimeoutSec 90
if ((Get-FileHash -LiteralPath $installer).Hash -ine $InstallerSha256) { throw 'Previous released installer SHA-256 did not match.' }

$setup = Start-Process -FilePath $installer -ArgumentList "/S /currentuser /D=$program" -WindowStyle Hidden -Wait -PassThru
if ($setup.ExitCode -ne 0 -or (Get-ItemPropertyValue -LiteralPath $record -Name InstallLocation).Trim('"') -ine $program) {
  throw 'Previous Beta did not install into the disposable fixture.'
}
try {
  $runtime = Get-LeagueNativeRuntime $program
  if (!$runtime -or $runtime.Version -cne $Version) { throw 'Previous Beta executable did not match.' }
  $inventory = [ordered]@{}
  foreach ($name in @($LeagueNativeFiles) + $LeagueInventoryName + $LeagueUninstallerName) {
    $file = Get-LeagueOrdinaryFile $program $name
    if ($file) { $inventory[$name] = (Get-FileHash -LiteralPath $file.FullName).Hash.ToLowerInvariant() }
  }
  if (!$inventory[$LeagueRuntimeName] -or !$inventory[$LeagueInventoryName] -or !$inventory[$LeagueUninstallerName]) {
    throw 'Previous Beta has an incomplete native inventory.'
  }
  $inventory | ConvertTo-Json -Compress | Set-Content -LiteralPath $CatalogPath -Encoding utf8
} finally {
  $uninstaller = Join-Path $program $LeagueUninstallerName
  $remove = Start-Process -FilePath $uninstaller -ArgumentList '/S /currentuser' -WindowStyle Hidden -Wait -PassThru
  if ($remove.ExitCode -ne 0 -or (Test-Path -LiteralPath $record) -or (Test-Path -LiteralPath $arp)) {
    throw 'Predecessor fixture did not uninstall.'
  }
}
