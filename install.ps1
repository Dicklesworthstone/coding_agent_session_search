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

# Resolve version: fetch latest release from GitHub unless explicitly set.
if (-not $Version) {
  try {
    $releaseInfo = Invoke-RestMethod -Uri "https://api.github.com/repos/$Owner/$Repo/releases/latest" -UseBasicParsing
    $Version = $releaseInfo.tag_name
    Write-Host "Using latest release: $Version"
  } catch {
    Write-Error "Could not determine latest version. Pass -Version <tag> explicitly."
    exit 1
  }
}

# Map architecture to the naming convention used by release.yml
$arch = if ([Environment]::Is64BitProcess) { "amd64" } else { "x86" }
$zip = "cass-windows-${arch}.zip"

if ($ArtifactUrl) {
  $url = $ArtifactUrl
} else {
  # Release asset names follow the pattern: cass-windows-amd64.zip
  # (produced by the release.yml workflow matrix `asset_name` field)
  $url = "https://github.com/$Owner/$Repo/releases/download/$Version/$zip"
}

$tmp = (New-TemporaryFile).DirectoryName
$zipFile = Join-Path $tmp $zip

Write-Host "Downloading $url"
Invoke-WebRequest -Uri $url -OutFile $zipFile

$checksumToUse = $Checksum
if (-not $checksumToUse) {
  if (-not $ChecksumUrl) { $ChecksumUrl = "$url.sha256" }
  Write-Host "Fetching checksum from $ChecksumUrl"
  $checksumFetched = $false
  # Try per-file .sha256 first, then fall back to SHA256SUMS.txt
  foreach ($tryUrl in @($ChecksumUrl, "https://github.com/$Owner/$Repo/releases/download/$Version/SHA256SUMS.txt")) {
    if ($checksumFetched) { break }
    try {
      # Use Invoke-RestMethod which returns the body as a string and follows
      # redirects reliably across all PowerShell versions (Windows PS 5.x and
      # PS Core 7+).  Invoke-WebRequest with -UseBasicParsing can return
      # .Content as a byte array in PS 5.x, breaking .Trim().
      $raw = Invoke-RestMethod -Uri $tryUrl -ErrorAction Stop
      if ($tryUrl -like "*/SHA256SUMS.txt") {
        # SHA256SUMS.txt contains lines like: <hash>  <filename>
        foreach ($line in $raw -split "`n") {
          if ($line -match $zip) {
            $checksumToUse = ($line.Trim() -split '\s+')[0]
            $checksumFetched = $true
            break
          }
        }
      } else {
        $checksumToUse = ($raw.Trim() -split '\s+')[0]
        $checksumFetched = $true
      }
    } catch {
      Write-Host "Could not fetch checksum from $tryUrl, trying next source..."
    }
  }
  if (-not $checksumFetched -or -not $checksumToUse) {
    Write-Error "Checksum file not found or invalid; refusing to install."
    exit 1
  }
}

$hash = Get-FileHash $zipFile -Algorithm SHA256
if ($hash.Hash.ToLower() -ne $checksumToUse.ToLower()) { Write-Error "Checksum mismatch"; exit 1 }

Add-Type -AssemblyName System.IO.Compression.FileSystem
$extractDir = Join-Path $tmp "extract"
[System.IO.Compression.ZipFile]::ExtractToDirectory($zipFile, $extractDir)

$bin = Get-ChildItem -Path $extractDir -Recurse -Filter "cass.exe" | Select-Object -First 1
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
