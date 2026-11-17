param(
    [Parameter(Mandatory)][string]$Exe,  # the herdr.exe to prove
    [string]$Work = (Join-Path $env:TEMP 'herdr-restore-proof'),  # scratch folder: APPDATA, CODEX_HOME, logs
    [string]$Bet = 'G:\BET\blake_trws_blake_main\BET\Tools\UETestRunnerCLI\bet.exe'  # process list with parents (bet ps)
)
# Throwaway-server proof of the restore changes (BET-7529, TRIIODIDE.md): exit 0 when every check passes.
# The production herdr is never touched: APPDATA and CODEX_HOME point at a scratch folder, so the server,
# its sockets, its session.json and its log are the scratch folder's. The panes run the machine's real `codex` (against an empty CODEX_HOME, so it
# only sits at its login screen); the command lines herdr typed are read back from the process list.
#
# Only processes in the process tree of the throwaway server are looked at or stopped, each by its exact pid. Other agents' codex processes are
# never matched by a name, an id or a pattern.
$ErrorActionPreference = 'Stop'
$bet = $Bet
$root = Join-Path $Work 'run'  # emptied at the start: only this subfolder of -Work is ever removed
if (Test-Path $root) { Remove-Item $root -Recurse -Force }
$appData = Join-Path $root 'appdata'
$herdrDir = Join-Path $appData 'herdr'
$codexHome = Join-Path $root 'codexhome'
New-Item -ItemType Directory -Force -Path $herdrDir, (Join-Path $codexHome 'sessions\2026\10\30') | Out-Null

Get-ChildItem Env: | Where-Object { $_.Name -like 'HERDR_*' } | ForEach-Object { Remove-Item "Env:$($_.Name)" }
$env:APPDATA = $appData
$env:CODEX_HOME = $codexHome

$u1 = '01a18ce6-42f2-7c01-9098-fee91706852d'  # hook-recorded Codex session saved by an old herdr: no flags, a rollout
$u2 = '01a18ce6-0000-7c01-9098-fee917068520'  # hand-started Codex whose resume command was recorded
$u3 = '01a18ce6-1111-7c01-9098-fee917068521'  # started by hand while the server runs
Set-Content -Path (Join-Path $codexHome "sessions\2026\10\30\rollout-2026-10-30T00-00-00-$u1.jsonl") -Value @(
    '{"type":"session_meta","payload":{"id":"x"}}',
    '{"type":"turn_context","payload":{"model":"gpt-fake-1","effort":"high"}}'
)
Set-Content -Path (Join-Path $herdrDir 'config.toml') -Value @('[session]', 'codex_resume_args = ["--no-daemon", "-p", "lorefs"]')

function New-Tab($id, $name, $pane) {
    [ordered]@{ custom_name = $name; layout = [ordered]@{ Pane = $id }; zoomed = $false; focused = $id; root_pane = $id; panes = [ordered]@{ "$id" = $pane } }
}
$session = [ordered]@{
    version = 3; active = 0; selected = 0
    workspaces = @([ordered]@{
            id = 'w1'; custom_name = 'proof'; identity_cwd = $root
            public_pane_numbers = [ordered]@{ '1' = 1; '2' = 2; '3' = 3 }; next_public_pane_number = 4
            public_tab_numbers = @(1, 2, 3); next_public_tab_number = 4; active_tab = 0
            tabs = @(
                (New-Tab 1 'old-session-json' ([ordered]@{ cwd = $root; agent_session = [ordered]@{ source = 'herdr:codex'; agent = 'codex'; kind = 'id'; value = $u1 } })),
                (New-Tab 2 'hand-started' ([ordered]@{ cwd = $root; agent_resume = [ordered]@{ source = 'herdr:process'; agent = 'codex'; argv = @('codex', 'resume', $u2, '-p', 'lorefs', '-m', 'manual-model', '--no-daemon') } })),
                (New-Tab 3 'bare' ([ordered]@{ cwd = $root }))
            )
        })
}
$session | ConvertTo-Json -Depth 12 | Set-Content -Path (Join-Path $herdrDir 'session.json') -Encoding utf8

function Start-Server {
    $log = Join-Path $root ("server-{0}.err" -f (Get-Date -Format 'HHmmss'))
    $p = Start-Process -FilePath $Exe -ArgumentList 'server' -PassThru -WindowStyle Hidden -RedirectStandardError $log
    $deadline = (Get-Date).AddSeconds(30)
    while (-not (Test-Path (Join-Path $herdrDir 'herdr.sock')) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 200 }
    return $p
}
# Every process below $rootPid: pid, parent, image and command line, from one `bet ps`.
function Get-Tree($rootPid) {
    $rows = @(& $bet ps | Select-Object -Skip 1 | ForEach-Object {
            if ($_ -match '^\s*(\d+)\s+(\d+)\s+(\S+)\s*(.*)$') { [pscustomobject]@{ Pid = [int]$Matches[1]; Parent = [int]$Matches[2]; Name = $Matches[3]; Cmd = $Matches[4] } }
        })
    $found = @{ $rootPid = $true }
    do {
        $before = $found.Count
        foreach ($row in $rows) { if ($found.ContainsKey($row.Parent) -and -not $found.ContainsKey($row.Pid)) { $found[$row.Pid] = $true } }
    } while ($found.Count -gt $before)
    $rows | Where-Object { $found.ContainsKey($_.Pid) -and $_.Pid -ne $rootPid }
}
# The codex.exe processes below the server, by the command line herdr typed (the three session ids of this proof only).
function Get-StartedCodex($serverPid) {
    Get-Tree $serverPid | Where-Object { $_.Name -eq 'codex.exe' } | ForEach-Object {
        if ($_.Cmd -match 'codex\.exe\s+(resume (?:01a18ce6-42f2-7c01-9098-fee91706852d|01a18ce6-0000-7c01-9098-fee917068520|01a18ce6-1111-7c01-9098-fee917068521)\b.*)$') {
            [pscustomobject]@{ Pid = $_.Pid; Line = $Matches[1].Trim() }
        }
    }
}
function Wait-Started($serverPid, $wanted) {
    $deadline = (Get-Date).AddSeconds(60)
    while ((Get-Date) -lt $deadline) {
        $lines = @(Get-StartedCodex $serverPid | ForEach-Object Line)
        if (@($wanted | Where-Object { $lines -contains $_ }).Count -eq $wanted.Count) { return $lines }
        Start-Sleep -Seconds 1
    }
    return @(Get-StartedCodex $serverPid | ForEach-Object Line)
}
# Stops the whole tree of a proof server, deepest first, by exact pid; the server itself is stopped by the caller.
function Stop-Tree($serverPid) {
    foreach ($row in @(Get-Tree $serverPid | Sort-Object Pid -Descending)) { Stop-Process -Id $row.Pid -Force -ErrorAction SilentlyContinue }
}

$results = [ordered]@{}
$server = $null
$second = $null
$oldServerId = $null
try {
    '== 1. start the server on the saved session'
    $server = Start-Server
    $expected1 = "resume $u1 -p lorefs --no-daemon -m gpt-fake-1 -c model_reasoning_effort=high"
    $expected2 = "resume $u2 -p lorefs -m manual-model --no-daemon"
    $lines = Wait-Started $server.Id @($expected1, $expected2)
    'codex was started as:'; $lines | ForEach-Object { "  codex $_" }
    # Two real codex processes start on the same empty CODEX_HOME at once, and one can die before it is seen: also read what was typed.
    $typed = (& $Exe pane read w1:p1 --source visible --lines 80 | Out-String) -replace '\s+', ''
    'pane 1 shows: ' + ($typed -replace '.*(resume01a18ce6)', '$1').Substring(0, [Math]::Min(150, $typed.Length))
    $results.oldSessionJsonCodexKeepsConfiguredFlagsAndRolloutModel = ($lines -contains $expected1) -or $typed.Contains(($expected1 -replace '\s+', ''))
    $results.handStartedResumeCommandIsTypedAsSaved = ($lines -contains $expected2)

    '== 2. a second server beside the live one'
    $marker1 = Get-Content (Join-Path $herdrDir 'herdr.sock') -Raw
    $spawnedBefore = @(Get-Content (Join-Path $herdrDir 'herdr-server.log') | Where-Object { $_ -match 'pane child spawned' }).Count
    $watch = [Diagnostics.Stopwatch]::StartNew()
    $second = Start-Process -FilePath $Exe -ArgumentList 'server' -PassThru -WindowStyle Hidden -RedirectStandardError (Join-Path $root 'second.err') -RedirectStandardOutput (Join-Path $root 'second.out')
    [void]$second.WaitForExit(120000)
    "second server exited=$($second.HasExited) after $([int]$watch.Elapsed.TotalSeconds) s, exit code $($second.ExitCode)"
    'second server said: ' + ((Get-Content (Join-Path $root 'second.err') -ErrorAction SilentlyContinue) -join ' | ')
    $spawnedAfter = @(Get-Content (Join-Path $herdrDir 'herdr-server.log') | Where-Object { $_ -match 'pane child spawned' }).Count
    $results.secondServerRefusedWithExitCode1 = ($second.HasExited -and $second.ExitCode -eq 1)
    $results.secondServerSpawnedNoPane = ($spawnedAfter -eq $spawnedBefore)
    $results.markerOfLiveServerUntouched = ((Get-Content (Join-Path $herdrDir 'herdr.sock') -Raw) -eq $marker1)
    $results.liveServerStillAnswers = [bool]((& $Exe status server --json | ConvertFrom-Json).running)

    '== 3. an agent started by hand while the server runs'
    $panes = (& $Exe pane list | ConvertFrom-Json).result.panes
    $bare = $panes | Where-Object { ($_ | ConvertTo-Json -Compress) -match 'bare' } | Select-Object -First 1
    if (-not $bare) { $bare = $panes | Select-Object -Last 1 }
    "typing into pane $($bare.pane_id)"
    & $Exe pane run $bare.pane_id "codex resume $u3 --no-daemon -p lorefs -m hand-model -c model_reasoning_effort=xhigh" | Out-Null
    $expected3 = "resume $u3 --no-daemon -p lorefs -m hand-model -c model_reasoning_effort=xhigh"
    [void](Wait-Started $server.Id @($expected3))
    $recorded = $null
    $deadline = (Get-Date).AddSeconds(60)
    $sessionJson = Join-Path $herdrDir 'session.json'
    while ((Get-Date) -lt $deadline -and -not $recorded) {
        Start-Sleep -Seconds 2
        $snap = Get-Content $sessionJson -Raw | ConvertFrom-Json
        foreach ($tab in $snap.workspaces[0].tabs) {
            foreach ($pane in $tab.panes.PSObject.Properties.Value) {
                if ($pane.agent_resume -and ($pane.agent_resume.argv -contains $u3)) { $recorded = $pane.agent_resume }
            }
        }
    }
    if ($recorded) { 'session.json now holds: ' + ($recorded.argv -join ' ') } else { 'session.json holds no resume command for it' }
    $results.handStartedWhileRunningIsInSessionJson = [bool]$recorded -and (($recorded.argv -join ' ') -eq "codex resume $u3 -p lorefs -m hand-model -c model_reasoning_effort=xhigh --no-daemon")
    'server log, detection lines:'
    Get-Content (Join-Path $herdrDir 'herdr-server.log') | Where-Object { $_ -match 'command line' } | ForEach-Object { '  ' + $_.Substring(0, [Math]::Min(230, $_.Length)) }

    '== 4. stop the server, start it again'
    $oldServerId = $server.Id
    & $Exe server stop | Out-Null
    [void]$server.WaitForExit(60000)
    Start-Sleep -Seconds 3
    $server = Start-Server
    $again = Wait-Started $server.Id @("resume $u3 -p lorefs -m hand-model -c model_reasoning_effort=xhigh --no-daemon", $expected1)
    'codex was started as:'; $again | ForEach-Object { "  codex $_" }
    $results.restartBringsBackTheAgentStartedByHandWhileRunning = ($again -contains "resume $u3 -p lorefs -m hand-model -c model_reasoning_effort=xhigh --no-daemon")
    $results.restartKeepsTheConfiguredFlagsOfTheOldSessionJsonAgent = ($again -contains $expected1)
}
finally {
    foreach ($serverId in @($oldServerId, $server.Id)) { if ($serverId) { Stop-Tree $serverId } }
    foreach ($proc in @($second, $server)) { if ($proc -and -not $proc.HasExited) { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue } }
}
''
'RESULTS'
$results.GetEnumerator() | ForEach-Object { '{0,-62} {1}' -f $_.Key, $(if ($_.Value) { 'PASS' } else { 'FAIL' }) }
if ($results.Values -contains $false) { exit 1 }
