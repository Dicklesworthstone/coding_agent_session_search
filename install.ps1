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

# Windows releases are published for 64-bit Windows only.
if (-not [Environment]::Is64BitOperatingSystem) {
  Write-Error "cass publishes Windows binaries for 64-bit Windows only."
  exit 1
}

function Get-ArtifactNameFromUrl {
  param([string]$Url)

  try {
    $uri = [System.Uri]$Url
    $path = $uri.AbsolutePath
  } catch {
    $path = ($Url -replace '[?#].*$', '')
  }

  if (-not $path) { return $null }
  return [System.IO.Path]::GetFileName($path)
}

function Get-SiblingUrl {
  param(
    [string]$Url,
    [string]$SiblingName
  )

  try {
    $uri = [System.Uri]$Url
    $builder = [System.UriBuilder]::new($uri)
    $path = $builder.Path
    if (-not $path) { return $null }
    $directory = [System.IO.Path]::GetDirectoryName($path.TrimEnd('/'))
    if ([string]::IsNullOrEmpty($directory)) {
      $builder.Path = "/$SiblingName"
    } else {
      $builder.Path = ($directory.TrimEnd('/') + "/$SiblingName")
    }
    $builder.Query = ""
    $builder.Fragment = ""
    return $builder.Uri.AbsoluteUri
  } catch {
    $base = ($Url -replace '[?#].*$', '')
    if (-not $base) { return $null }
    $lastSlash = $base.LastIndexOf('/')
    if ($lastSlash -lt 0) { return $SiblingName }
    return $base.Substring(0, $lastSlash + 1) + $SiblingName
  }
}

function Resolve-LocalSourcePath {
  param([string]$Location)

  if (-not $Location) { return $null }

  try {
    $uri = [System.Uri]$Location
    if ($uri.IsAbsoluteUri -and $uri.IsFile) {
      return $uri.LocalPath
    }
  } catch {
  }

  if (Test-Path -LiteralPath $Location) {
    return (Resolve-Path -LiteralPath $Location).ProviderPath
  }

  return $null
}

function Copy-ArtifactToFile {
  param(
    [string]$Location,
    [string]$Destination
  )

  $localPath = Resolve-LocalSourcePath $Location
  if ($localPath) {
    Copy-Item -LiteralPath $localPath -Destination $Destination -Force
    return
  }

  Invoke-WebRequest -Uri $Location -OutFile $Destination
}

function Read-TextResource {
  param([string]$Location)

  $localPath = Resolve-LocalSourcePath $Location
  if ($localPath) {
    return Get-Content -LiteralPath $localPath -Raw
  }

  return Invoke-RestMethod -Uri $Location -ErrorAction Stop
}

function Resolve-ChecksumToken {
  param([string]$Value)

  if (-not $Value) { return $null }

  $candidate = ($Value.Trim() -split '\s+', 2)[0]
  if ($candidate -match '^[0-9a-fA-F]{64}$') {
    return $candidate.ToLower()
  }

  return $null
}

# Map architecture to the naming convention used by release.yml
$arch = "amd64"
$zip = "cass-windows-${arch}.zip"

if ($ArtifactUrl) {
  $url = $ArtifactUrl
  $artifactName = Get-ArtifactNameFromUrl $ArtifactUrl
  if ($artifactName) { $zip = $artifactName }
} else {
  # Release asset names follow the pattern: cass-windows-amd64.zip
  # (produced by the release.yml workflow matrix `asset_name` field)
  $url = "https://github.com/$Owner/$Repo/releases/download/$Version/$zip"
}

$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("cass-install-" + [System.Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Force -Path $tmp | Out-Null

try {
  $zipFile = Join-Path $tmp $zip

  Write-Host "Downloading $url"
  Copy-ArtifactToFile -Location $url -Destination $zipFile

  $checksumToUse = $null
  if ($Checksum) {
    $checksumToUse = Resolve-ChecksumToken $Checksum
    if (-not $checksumToUse) {
      Write-Error "Checksum must be a 64-character SHA256 value or a .sha256 line containing one."
      exit 1
    }
  }
  if (-not $checksumToUse) {
    if (-not $ChecksumUrl) { $ChecksumUrl = Get-SiblingUrl $url "$zip.sha256" }
    Write-Host "Fetching checksum from $ChecksumUrl"
    $checksumFetched = $false
    # Try per-file .sha256 first, then fall back to SHA256SUMS.txt
    $sha256SumsUrl = Get-SiblingUrl $url "SHA256SUMS.txt"
    foreach ($tryUrl in @($ChecksumUrl, $sha256SumsUrl)) {
      if ($checksumFetched) { break }
      if (-not $tryUrl) { continue }
      try {
        # Read checksum content as text from either a local file or a remote URL.
        $raw = Read-TextResource $tryUrl
        if ($tryUrl -like "*/SHA256SUMS.txt") {
          # SHA256SUMS.txt contains lines like: <hash>  <filename>
          foreach ($line in $raw -split "`n") {
            $parts = $line.Trim() -split '\s+', 2
            if ($parts.Count -ge 2 -and $parts[1] -eq $zip) {
              $checksumToUse = Resolve-ChecksumToken $parts[0]
            }
            if ($checksumToUse) {
              $checksumFetched = $true
              break
            }
          }
        } else {
          $checksumToUse = Resolve-ChecksumToken $raw
          if ($checksumToUse) {
            $checksumFetched = $true
          }
        }
        if (-not $checksumFetched) {
          Write-Host "Checksum data from $tryUrl did not contain a valid entry for $zip, trying next source..."
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

  $bin = Get-ChildItem -Path $extractDir -Recurse -File |
    Where-Object { $_.Name -in @("cass.exe", "coding-agent-search.exe") } |
    Select-Object -First 1
  if (-not $bin) { Write-Error "Binary not found in zip"; exit 1 }
  if ($bin.Name -ne "cass.exe") {
    Write-Warning "Found legacy binary name '$($bin.Name)'; installing it as cass.exe"
  }

  if (-not (Test-Path $Dest)) { New-Item -ItemType Directory -Force -Path $Dest | Out-Null }
  Copy-Item $bin.FullName (Join-Path $Dest "cass.exe") -Force

  Write-Host "Installed to $Dest\cass.exe"
  $path = [Environment]::GetEnvironmentVariable("PATH", "User")
  if (-not $path) { $path = "" }
  $pathEntries = @($path -split ';' | Where-Object { $_ })
  if (-not ($pathEntries -contains $Dest)) {
    if ($EasyMode) {
      $newPath = if ($pathEntries.Count -gt 0) { "$path;$Dest" } else { $Dest }
      [Environment]::SetEnvironmentVariable("PATH", $newPath, "User")
      Write-Host "Added $Dest to PATH (User)"
    } else {
      Write-Host "Add $Dest to PATH to use cass"
    }
  }

  if ($Verify) {
    & "$Dest\cass.exe" --version | Write-Host
  }
} finally {
  if (Test-Path $tmp) {
    Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
  }
}
