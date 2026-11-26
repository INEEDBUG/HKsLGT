#Requires -Version 7.0
param(
  [Parameter(Mandatory)][ValidatePattern('^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)-beta\.(0|[1-9]\d*)$')][string]$Version,
  [Parameter(Mandatory)][string]$SourceDir,
  [Parameter(Mandatory)][string]$OutputDir,
  [string]$PredecessorCatalog,
  [string]$WebView2Bootstrapper,
  [switch]$VerifyCompanionWindows
)

$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true
if (!$IsWindows -or [Runtime.InteropServices.RuntimeInformation]::ProcessArchitecture -ne 'X64') {
  throw 'Use a Windows x64 PowerShell process.'
}
$root = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '../..')).Path
$source = (Resolve-Path -LiteralPath $SourceDir).Path
$output = [IO.Path]::GetFullPath($OutputDir)
if (Test-Path -LiteralPath $output) { throw 'Choose a fresh artifact directory.' }
$leaguePin = '14557723706ccc0e0a9d62c470141d4cb7190fcd'
if ((git -C $source rev-parse HEAD).Trim() -cne $leaguePin) { throw 'Unexpected LeagueAkari checkout.' }
$revision = (git -C $root rev-parse HEAD).Trim()
if ($env:GITHUB_SHA -and $revision -cne $env:GITHUB_SHA) { throw 'Unexpected workflow checkout.' }

$known = [ordered]@{
  'trusted-1.0.0-beta.5.json' = '85c33b257b4bcb4faa77ea8bbe947cd69deadbffdda0618711e0cc5177d50bc9'
  'trusted-1.0.0-beta.8.json' = '358d78e5417cadeff0153539e2f99cbe8b0be5a6123c0b58e1182005334aac7b'
}
$catalogRoot = Join-Path $PSScriptRoot 'native-legacy'
if (Test-Path -LiteralPath (Join-Path $catalogRoot 'trusted-1.0.0-beta.12.json') -PathType Leaf) {
  $known['trusted-1.0.0-beta.12.json'] = '2c982b25c841bc21c28fe305cdc6fe191e1c4b51c51aa50c745db592e7c2825b'
}
$catalogPaths = @($known.Keys | ForEach-Object { Join-Path $catalogRoot $_ })
$catalogHashes = @($known.Values)
if ($PredecessorCatalog) {
  $extra = Get-Item -LiteralPath $PredecessorCatalog
  $extraHash = (Get-FileHash -LiteralPath $extra.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
  if ($extraHash -notin $catalogHashes) {
    $catalogPaths += $extra.FullName
    $catalogHashes += $extraHash
  }
}

$bootstrapUrl = 'https://msedge.sf.dl.delivery.mp.microsoft.com/filestreamingservice/files/cc1101bc-3d8a-4c67-8678-b753ccdc08ad/MicrosoftEdgeWebview2Setup.exe'
$bootstrapHash = 'fc2972da71d43e0c57b8d85a5cb9b200ed221f723cf43052d08ca722eb0bb22e'
$desktop = Join-Path $root 'rust/league-desktop'
$cargoTarget = if ($env:CARGO_TARGET_DIR) { [IO.Path]::GetFullPath($env:CARGO_TARGET_DIR) } else { Join-Path $desktop 'target' }
$target = 'x86_64-pc-windows-msvc'
$scratchName = 'hkslgt-build-' + [Guid]::NewGuid().ToString('n')
$scratch = [IO.Directory]::CreateDirectory((Join-Path $desktop $scratchName)).FullName
$oldConfig = $env:TAURI_CONFIG
try {
  if (!$WebView2Bootstrapper) {
    $WebView2Bootstrapper = Join-Path $scratch 'MicrosoftEdgeWebview2Setup.exe'
    Invoke-WebRequest -Uri $bootstrapUrl -OutFile $WebView2Bootstrapper -TimeoutSec 90
  }
  if ((Get-FileHash -LiteralPath $WebView2Bootstrapper -Algorithm SHA256).Hash -ine $bootstrapHash) {
    throw 'The WebView2 bootstrapper differs from the pinned Microsoft download.'
  }
  $renderer = Join-Path $scratch 'renderer'
  & (Join-Path $PSScriptRoot 'stage-league-runtime.ps1') -SourceDir $source -OutputDir $renderer -Version $Version
  $env:TAURI_CONFIG = @{
    version = $Version
    build = @{ frontendDist = "$scratchName/renderer" }
    bundle = @{ icon = @("$scratchName/renderer/league.ico") }
  } | ConvertTo-Json -Depth 4 -Compress
  $cargoArgs = @('--release', '--locked', '--target', $target, '--target-dir', $cargoTarget)
  $desktopManifest = Join-Path $desktop 'Cargo.toml'
  cargo test @cargoArgs --manifest-path (Join-Path $root 'rust/Cargo.toml') -p maxgamestudio-league-core
  cargo test @cargoArgs --manifest-path $desktopManifest -- --test-threads=1
  if ($VerifyCompanionWindows) {
    $companionTest = 'desktop::companion_window_smoke::champ_select_opens_real_windows'
    cargo test @cargoArgs --manifest-path $desktopManifest $companionTest -- --ignored --exact --nocapture
  }
  cargo build @cargoArgs --manifest-path $desktopManifest

  $release = Join-Path $cargoTarget "$target/release"
  $payload = [IO.Directory]::CreateDirectory((Join-Path $scratch 'payload')).FullName
  $runtime = Join-Path $payload 'MaxGameStudioLeagueToolbox.exe'
  Copy-Item -LiteralPath (Join-Path $release 'maxgamestudio-league-desktop.exe') -Destination $runtime
  $loader = Join-Path $release 'WebView2Loader.dll'
  if (Test-Path -LiteralPath $loader -PathType Leaf) { Copy-Item -LiteralPath $loader -Destination $payload }
  $options = @{
    SourceDir = $source; UnpackedDir = $payload; InstallerOutputDir = $output; Version = $Version
    WebView2Bootstrapper = $WebView2Bootstrapper; LegacyInventory = $catalogPaths; LegacyInventorySHA256 = $catalogHashes
  }
  & (Join-Path $PSScriptRoot 'build-league-installer.ps1') @options
  $setup = Get-Item -LiteralPath (Join-Path $output "MaxGameStudioLeague_$($Version)_x64-setup.exe")
  $hash = (Get-FileHash -LiteralPath $setup.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
  "$hash *$($setup.Name)" | Set-Content -LiteralPath "$($setup.FullName).sha256" -Encoding ascii
  [ordered]@{
    version = $Version; sourceCommit = $revision; leagueAkariCommit = $leaguePin
    installer = $setup.Name; installerSha256 = $hash; installerBytes = $setup.Length
    executableSha256 = (Get-FileHash -LiteralPath $runtime -Algorithm SHA256).Hash.ToLowerInvariant()
    webView2BootstrapperSha256 = $bootstrapHash; priorCatalogSha256 = $catalogHashes
    node = (node --version).Trim(); rustc = (rustc --version).Trim(); target = $target
    runnerImage = $env:ImageOS; imageVersion = $env:ImageVersion; workflowRunId = $env:GITHUB_RUN_ID
  } | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath (Join-Path $output 'provenance.json') -Encoding utf8NoBOM
} finally {
  $env:TAURI_CONFIG = $oldConfig
  $temporary = Get-Item -LiteralPath $scratch -Force
  if ($temporary.Parent.FullName -ine $desktop -or $temporary.Name -cne $scratchName -or
      ($temporary.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
    throw 'Unexpected temporary build directory.'
  }
  Remove-Item -LiteralPath $temporary.FullName -Recurse -Force
}
