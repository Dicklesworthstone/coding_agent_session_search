Param(
  [string]$Version = "",
  [string]$Dest = "$HOME/.local/bin",
  [string]$Owner = "Dicklesworthstone",
  [string]$Repo = "coding_agent_session_search",
  [string]$Checksum = "",
  [string]$ChecksumUrl = "",
  [string]$ArtifactUrl = "",
  [switch]$EasyMode,
  [switch]$Verify
)

$ErrorActionPreference = "Stop"
$PinnedRatatuiVersion = "v0.1.64"

# Use pinned stable version unless explicitly overridden.
if (-not $Version) {
  $Version = $PinnedRatatuiVersion
  Write-Host "Using pinned stable version: $Version"
}
$os = "windows"
$arch = if ([Environment]::Is64BitProcess) { "x86_64" } else { "x86" }
$zip = "coding-agent-search-$Version-$arch-$os-msvc.zip"
if ($ArtifactUrl) {
  $url = $ArtifactUrl
} else {
  # cargo-dist usually names windows zips like package-vX.Y.Z-x86_64-pc-windows-msvc.zip
  # But we'll use a simpler guess matching install.sh logic or common dist patterns
  $target = "x86_64-pc-windows-msvc"
  $zip = "coding-agent-search-$target.zip"
  $url = "https://github.com/$Owner/$Repo/releases/download/$Version/$zip"
}

$tmp = New-TemporaryFile | Split-Path
$zipFile = Join-Path $tmp $zip

Write-Host "Downloading $url"
Invoke-WebRequest -Uri $url -OutFile $zipFile

$checksumToUse = $Checksum
if (-not $checksumToUse) {
  if (-not $ChecksumUrl) { $ChecksumUrl = "$url.sha256" }
  Write-Host "Fetching checksum from $ChecksumUrl"
  try { $checksumToUse = (Invoke-WebRequest -Uri $ChecksumUrl -UseBasicParsing).Content.Trim().Split(' ')[0] } catch { Write-Error "Checksum file not found or invalid; refusing to install."; exit 1 }
}

$hash = Get-FileHash $zipFile -Algorithm SHA256
if ($hash.Hash.ToLower() -ne $checksumToUse.ToLower()) { Write-Error "Checksum mismatch"; exit 1 }

Add-Type -AssemblyName System.IO.Compression.FileSystem
$extractDir = Join-Path $tmp "extract"
[System.IO.Compression.ZipFile]::ExtractToDirectory($zipFile, $extractDir)

$bin = Get-ChildItem -Path $extractDir -Recurse -Filter "cass.exe" | Select-Object -First 1
if (-not $bin) {
  $bin = Get-ChildItem -Path $extractDir -Recurse -Filter "coding-agent-search.exe" | Select-Object -First 1
  if ($bin) { Write-Warning "Found coding-agent-search.exe instead of cass.exe; installing as cass.exe" }
}

if (-not $bin) { Write-Error "Binary not found in zip"; exit 1 }

if (-not (Test-Path $Dest)) { New-Item -ItemType Directory -Force -Path $Dest | Out-Null }
Copy-Item $bin.FullName (Join-Path $Dest "cass.exe") -Force

Write-Host "Installed to $Dest\cass.exe"
$path = [Environment]::GetEnvironmentVariable("PATH", "User")
if (-not $path.Contains($Dest)) {
  if ($EasyMode) {
    [Environment]::SetEnvironmentVariable("PATH", "$path;$Dest", "User")
    Write-Host "Added $Dest to PATH (User)"
  } else {
    Write-Host "Add $Dest to PATH to use cass"
  }
}

if ($Verify) {
  & "$Dest\cass.exe" --version | Write-Host
}
