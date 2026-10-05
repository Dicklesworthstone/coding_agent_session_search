# Native Windows indexing acceptance for one cass.exe (beads aegfi / GH #429
# and 4w0ma / GH #406). Run by .github/workflows/windows-index-acceptance.yml
# against a published release and against a source build.
#
# Proves, in fresh processes on a real NTFS data dir:
#   1. a full index of a Claude Code fixture exits 0 with JSON on stdout;
#   2. a lexical search from a new process finds a fixture word (cold reopen);
#   3. after a message is appended, an incremental index exits 0 and a new
#      process finds the new word;
#   4. while `cass index --watch` holds the index, a second `cass index`
#      exits 7 with kind `index-busy`; after the watcher is stopped, the next
#      `cass index` reacquires the lock and exits 0;
#   5. status and health print parseable JSON.
# Every command must also be free of the GH #406 teardown panic: no
# "panicked at" or "threads should not terminate unexpectedly" on stderr and
# no 0xC0000409 exit, including after valid JSON was printed.
param(
    [Parameter(Mandatory = $true)][string]$Cass,
    [Parameter(Mandatory = $true)][string]$Fixture,
    [Parameter(Mandatory = $true)][string]$Work
)
$ErrorActionPreference = 'Stop'

$failures = [System.Collections.Generic.List[string]]::new()
$data = Join-Path $Work 'data'
$claude = Join-Path $Work 'claude'
$project = Join-Path $claude 'projects\-test-project'
New-Item -ItemType Directory -Force -Path $data, $project | Out-Null
$session = Join-Path $project 'agent-test123.jsonl'
Copy-Item $Fixture $session

$env:CLAUDE_CONFIG_DIR = $claude
$env:CASS_AUTO_REFRESH = '0'
$env:CASS_IGNORE_SOURCES_CONFIG = '1'
$env:CODING_AGENT_SEARCH_NO_UPDATE_PROMPT = '1'
$env:NO_COLOR = '1'

function Invoke-Cass {
    param([string]$Label, [string[]]$CassArgs, [int[]]$ExpectExit)
    $out = Join-Path $Work "$Label.stdout"
    $err = Join-Path $Work "$Label.stderr"
    $started = Get-Date
    & $Cass @CassArgs 1> $out 2> $err
    $code = $LASTEXITCODE
    $seconds = [math]::Round(((Get-Date) - $started).TotalSeconds, 1)
    $stdout = if (Test-Path $out) { Get-Content $out -Raw } else { '' }
    $stderr = if (Test-Path $err) { Get-Content $err -Raw } else { '' }
    if ($null -eq $stdout) { $stdout = '' }
    if ($null -eq $stderr) { $stderr = '' }
    Write-Host "[$Label] exit=$code ${seconds}s stdout=$($stdout.Length)B stderr=$($stderr.Length)B"
    if ($ExpectExit -notcontains $code) {
        $failures.Add("$Label exited $code, expected $($ExpectExit -join '/')")
        Write-Host "---- $Label stdout ----`n$stdout`n---- $Label stderr ----`n$stderr"
    }
    if ($code -eq -1073740791 -or $stderr -match 'panicked at|threads should not terminate unexpectedly') {
        $failures.Add("$Label hit a teardown panic (exit $code)")
        Write-Host "---- $Label stderr (panic) ----`n$stderr"
    }
    return [pscustomobject]@{ Code = $code; Stdout = $stdout; Stderr = $stderr }
}

function Get-Json {
    param([string]$Label, [string]$Text)
    try {
        return $Text | ConvertFrom-Json -Depth 64
    } catch {
        $failures.Add("$Label stdout is not JSON: $($_.Exception.Message)")
        return $null
    }
}

function Assert-Hits {
    param([string]$Label, [string]$Query)
    $r = Invoke-Cass $Label @('search', $Query, '--data-dir', $data, '--robot', '--mode', 'lexical', '--limit', '5') @(0)
    $json = Get-Json $Label $r.Stdout
    $count = if ($json -and $json.hits) { @($json.hits).Count } else { 0 }
    Write-Host "[$Label] hits=$count for '$Query'"
    if ($count -lt 1) { $failures.Add("$Label found no hit for '$Query'") }
}

& $Cass --version
Write-Host "cass: $Cass"
Write-Host "sha256: $((Get-FileHash $Cass -Algorithm SHA256).Hash)"

# 1-2. Full index, then a cold-reopen search in a new process.
$r = Invoke-Cass 'index-full' @('index', '--full', '--data-dir', $data, '--json') @(0)
$null = Get-Json 'index-full' $r.Stdout
Assert-Hits 'search-fixture' 'smartedgar'

# 3. Incremental index picks up an appended message.
$line = '{"parentUuid":"msg-001","cwd":"/test/project","sessionId":"test-session","version":"2.0.37","gitBranch":"main","agentId":"test123","type":"user","message":{"role":"user","content":"windowsincrementalprobe appended after the first index"},"uuid":"msg-win-append","timestamp":"2025-11-12T19:00:00.000Z"}'
Add-Content -Path $session -Value $line
(Get-Item $session).LastWriteTime = (Get-Date)
$r = Invoke-Cass 'index-incremental' @('index', '--data-dir', $data, '--json') @(0)
$null = Get-Json 'index-incremental' $r.Stdout
Assert-Hits 'search-appended' 'windowsincrementalprobe'

# 4. Contention: a watcher holds the index; a second run is index-busy.
$watchOut = Join-Path $Work 'watch.stdout'
$watchErr = Join-Path $Work 'watch.stderr'
$watcher = Start-Process -FilePath $Cass -ArgumentList @('index', '--watch', '--data-dir', $data) -PassThru -NoNewWindow -RedirectStandardOutput $watchOut -RedirectStandardError $watchErr
$holding = $false
for ($i = 0; $i -lt 60 -and -not $watcher.HasExited; $i++) {
    Start-Sleep -Seconds 1
    $s = Invoke-Cass "status-poll-$i" @('status', '--json', '--data-dir', $data) @(0, 1)
    $sj = Get-Json "status-poll-$i" $s.Stdout
    # Live signals only: rebuild.pid can survive in lock metadata after a run.
    if ($sj -and $sj.rebuild -and $sj.rebuild.active) { $holding = $true; break }
    if ($sj -and $sj.pending -and $sj.pending.watch_active) { $holding = $true; break }
}
if ($watcher.HasExited) {
    $failures.Add("the watcher exited early with $($watcher.ExitCode)")
    Write-Host "---- watcher stderr ----`n$(Get-Content $watchErr -Raw)"
} elseif (-not $holding) {
    $failures.Add('status never reported the watcher as holding the index within 60 s')
} else {
    $busy = Invoke-Cass 'index-contended' @('index', '--data-dir', $data, '--json') @(7)
    if (($busy.Stdout + $busy.Stderr) -notmatch 'index-busy') {
        $failures.Add('the contended index run did not report kind index-busy')
    }
}
if (-not $watcher.HasExited) {
    Stop-Process -Id $watcher.Id -Force
    $watcher.WaitForExit(30000) | Out-Null
}
$r = Invoke-Cass 'index-reacquired' @('index', '--data-dir', $data, '--json') @(0)
$null = Get-Json 'index-reacquired' $r.Stdout
Assert-Hits 'search-after-reacquire' 'smartedgar'

# 5. Truth surfaces print JSON (health may be 1 while semantic is absent).
$r = Invoke-Cass 'status' @('status', '--json', '--data-dir', $data) @(0, 1)
$null = Get-Json 'status' $r.Stdout
$r = Invoke-Cass 'health' @('health', '--json', '--data-dir', $data) @(0, 1)
$null = Get-Json 'health' $r.Stdout

if ($failures.Count -gt 0) {
    Write-Host "`nFAILED ($($failures.Count)):"
    $failures | ForEach-Object { Write-Host "  - $_" }
    exit 1
}
Write-Host "`nPASSED: index, cold search, incremental, contention and reacquisition, truth surfaces, no teardown panic"
