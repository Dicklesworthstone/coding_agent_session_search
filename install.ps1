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
if (-not $Version) {
  Write-Host "Resolving latest version..."

  # =========================================================================
  # Redirect-Based Version Resolution (GitHub Issue #28)
  # =========================================================================
  # We use GitHub's redirect behavior instead of the API:
  # - NO RATE LIMITING: API limits to 60/hr; redirects have no limit
  # - NO JSON PARSING: API requires grep+sed which varies GNU/BSD
  # - SIMPLER FAILURES: Only fails if GitHub is completely down
  #
  # How it works: GitHub redirects /releases/latest -> /releases/tag/{version}
  # We capture the final URL and extract the tag with Split-Path
  # =========================================================================

  $releasesUrl = "https://github.com/$Owner/$Repo/releases/latest"
  $finalUrl = ""
  try {
    $resp = Invoke-WebRequest -Uri $releasesUrl -UseBasicParsing -TimeoutSec 30 -UserAgent "cass-installer/1.0"
    if ($resp.BaseResponse -and $resp.BaseResponse.ResponseUri) {
      $finalUrl = $resp.BaseResponse.ResponseUri.AbsoluteUri
    }
  } catch {
    $finalUrl = ""
  }

  $tag = if ($finalUrl) { Split-Path $finalUrl -Leaf } else { "" }
  if ($tag -and ($finalUrl -like "*/releases/tag/*")) {
    $Version = $tag
    Write-Host "Resolved latest version: $Version"
  } else {
    $Version = "v0.1.49"
    Write-Warning "Could not resolve latest version; defaulting to $Version"
  }
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

