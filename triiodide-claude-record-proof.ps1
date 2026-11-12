param(
    [Parameter(Mandatory)][string]$Exe,  # the herdr.exe to prove
    [string]$Work = (Join-Path $env:TEMP 'herdr-claude-record-proof'),  # scratch folder: APPDATA, XDG_STATE_HOME, the stand-in claude, logs
    [string]$Bet = 'G:\BET\blake_trws_blake_main\BET\Tools\UETestRunnerCLI\bet.exe'  # process list with parents (bet ps --json)
)
# Throwaway-server proof of BET-9889: a Claude tab started as Opus at xhigh and moved to Fable at low (what Luna's mod records on /model and /effort) comes back from a herdr
# restart as Fable at low, and again as what it is moved to next. Exit 0 when every check passes.
# The production herdr is never touched: APPDATA and XDG_STATE_HOME point at a scratch folder, so the server, its sockets, its session.json and the per-session records are the
# scratch folder's. session.json is written as the live one holds it (agent_session with the launch_args read off the command line at launch, which a /model never changes).
# The panes run a stand-in claude.exe (a program that sleeps), so no real Claude Code session is started or resumed; the command lines herdr typed are read back from the
# process list, and the proof fails if any claude.exe under the server is not the stand-in.
#
# Only processes in the process tree of the throwaway server are looked at or stopped, each by its exact pid.
$ErrorActionPreference = 'Stop'
$root = Join-Path $Work 'run'  # emptied at the start: only this subfolder of -Work is ever removed
if (Test-Path $root) { Remove-Item $root -Recurse -Force }
$appData = Join-Path $root 'appdata'
$state = Join-Path $root 'state'
$herdrDir = Join-Path $appData 'herdr'
$records = Join-Path $state 'herdr\plugins\triiodide.luna\effort'
$fakeSrc = Join-Path $root 'fakeclaude-src'
New-Item -ItemType Directory -Force -Path $herdrDir, $records, $fakeSrc | Out-Null

Set-Content -Path (Join-Path $fakeSrc 'main.go') -Value "package main`nimport `"time`"`nfunc main() { time.Sleep(time.Hour) }`n"
Set-Content -Path (Join-Path $fakeSrc 'go.mod') -Value "module fakeclaude`ngo 1.21`n"
Push-Location $fakeSrc
try { & go build -o (Join-Path $root 'claude.exe') . } finally { Pop-Location }
if (-not (Test-Path (Join-Path $root 'claude.exe'))) { throw 'the stand-in claude.exe was not built' }

Get-ChildItem Env: | Where-Object { $_.Name -like 'HERDR_*' } | ForEach-Object { Remove-Item "Env:$($_.Name)" }
Remove-Item Env:XDG_CONFIG_HOME -ErrorAction SilentlyContinue
# The panes start in $root, where the stand-in is. cmd.exe reads no profile and looks in the current folder before PATH, unless this variable, which this machine sets to 1, says not to.
Remove-Item Env:NoDefaultCurrentDirectoryInExePath -ErrorAction SilentlyContinue
$env:APPDATA = $appData
$env:XDG_STATE_HOME = $state
Set-Content -Path (Join-Path $herdrDir 'config.toml') -Value @('[terminal]', 'default_shell = "cmd.exe"')

$u1 = '9a1d4c7e-2b53-4f08-8e6a-0c3f5d7b1e24'  # launched Opus xhigh, moved to Fable low
$u2 = '9a1d4c7e-2b53-4f08-8e6a-0c3f5d7b1e25'  # launched Opus xhigh, never moved: no Opus at xhigh
$u3 = '9a1d4c7e-2b53-4f08-8e6a-0c3f5d7b1e26'  # launched Sonnet xhigh with a permission mode, moved to high: the permission mode stays

function New-Tab($id, $name, $sid, $launchArgs) {
    $pane = [ordered]@{ cwd = $root; agent_session = [ordered]@{ source = 'herdr:claude'; agent = 'claude'; kind = 'id'; value = $sid; launch_args = $launchArgs } }
    [ordered]@{ custom_name = $name; layout = [ordered]@{ Pane = $id }; zoomed = $false; focused = $id; root_pane = $id; panes = [ordered]@{ "$id" = $pane } }
}
$session = [ordered]@{
    version = 3; active = 0; selected = 0
    workspaces = @([ordered]@{
            id = 'w1'; custom_name = 'proof'; identity_cwd = $root
            public_pane_numbers = [ordered]@{ '1' = 1; '2' = 2; '3' = 3 }; next_public_pane_number = 4
            public_tab_numbers = @(1, 2, 3); next_public_tab_number = 4; active_tab = 0
            tabs = @(
                (New-Tab 1 'moved-to-fable' $u1 @('--model', 'claude-opus-5-5', '--effort', 'xhigh')),
                (New-Tab 2 'opus-xhigh' $u2 @('--model', 'claude-opus-5-5', '--effort', 'xhigh')),
                (New-Tab 3 'sonnet-mode' $u3 @('--model', 'claude-sonnet-5-5', '--effort', 'xhigh', '--permission-mode', 'auto'))
            )
        })
}
$session | ConvertTo-Json -Depth 12 | Set-Content -Path (Join-Path $herdrDir 'session.json') -Encoding utf8

# What Luna's mod writes into the session's record after a /model or /effort.
function Set-Record($sid, $model, $effort) {
    Set-Content -Path (Join-Path $records "$sid.json") -Value (@{ model = $model; effort = $effort } | ConvertTo-Json -Compress) -Encoding ascii
}
# The changes typed in tabs 1 and 3 after launch: nothing on a command line or in session.json moves, only the mod's record does.
Set-Record $u1 'claude-fable-5-2' 'low'
Set-Record $u3 'claude-sonnet-5-5' 'high'

function Start-Server {
    $log = Join-Path $root ("server-{0}.err" -f (Get-Date -Format 'HHmmss'))
    $p = Start-Process -FilePath $Exe -ArgumentList 'server' -PassThru -WindowStyle Hidden -RedirectStandardError $log
    $deadline = (Get-Date).AddSeconds(30)
    while (-not (Test-Path (Join-Path $herdrDir 'herdr.sock')) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 200 }
    return $p
}
# Every process below $rootPid, from one `bet ps --json`.
function Get-Tree($rootPid) {
    $rows = @(& $Bet ps --json | ConvertFrom-Json)
    $found = @{ $rootPid = $true }
    do {
        $before = $found.Count
        foreach ($row in $rows) { if ($found.ContainsKey([int]$row.ppid) -and -not $found.ContainsKey([int]$row.pid)) { $found[[int]$row.pid] = $true } }
    } while ($found.Count -gt $before)
    $rows | Where-Object { $found.ContainsKey([int]$_.pid) -and [int]$_.pid -ne $rootPid }
}
function Get-ClaudeProcesses($serverPid) {
    Get-Tree $serverPid | Where-Object { $_.name -eq 'claude.exe' }
}
function Test-StandIn($row) { (Get-Process -Id ([int]$row.pid) -ErrorAction SilentlyContinue).Path -eq (Join-Path $root 'claude.exe') }
# The stand-in claude processes below the server, by the command line herdr typed, as everything after `claude`.
function Get-Started($serverPid) {
    Get-ClaudeProcesses $serverPid | Where-Object { Test-StandIn $_ } | ForEach-Object {
        if ($_.commandLine -match 'claude(?:\.exe)?"?\s+(.*)$') { $Matches[1].Trim() }
    }
}
function Wait-Started($serverPid, $wanted) {
    $deadline = (Get-Date).AddSeconds(60)
    while ((Get-Date) -lt $deadline) {
        $lines = @(Get-Started $serverPid)
        if (@($wanted | Where-Object { $lines -contains $_ }).Count -eq $wanted.Count) { return $lines }
        Start-Sleep -Seconds 1
    }
    return @(Get-Started $serverPid)
}
function Stop-Tree($serverPid) {
    foreach ($row in @(Get-Tree $serverPid | Sort-Object { [int]$_.pid } -Descending)) { Stop-Process -Id ([int]$row.pid) -Force -ErrorAction SilentlyContinue }
}
function Get-SavedLaunchArgs($sid) {
    $snap = Get-Content (Join-Path $herdrDir 'session.json') -Raw | ConvertFrom-Json
    foreach ($tab in $snap.workspaces[0].tabs) {
        foreach ($pane in $tab.panes.PSObject.Properties.Value) {
            if ($pane.agent_session -and $pane.agent_session.value -eq $sid) { return ($pane.agent_session.launch_args -join ' ') }
        }
    }
    return $null
}

$results = [ordered]@{}
$server = $null
$oldServerId = $null
try {
    '== 1. start the server on the saved session: tab 1 was launched Opus xhigh and moved to Fable low, tab 2 launched Opus xhigh, tab 3 moved from xhigh to high'
    $server = Start-Server
    $want1 = "--resume $u1 --model claude-fable-5-2 --effort low"
    $want2 = "--resume $u2 --model claude-opus-5-5 --effort medium"
    $want3 = "--resume $u3 --model claude-sonnet-5-5 --effort high --permission-mode auto"
    $first = Wait-Started $server.Id @($want1, $want2, $want3)
    'claude was started as:'; $first | ForEach-Object { "  claude $_" }
    $results.launchedOpusXhighMovedToFableLowComesBackFableLow = ($first -contains $want1)
    $results.launchedOpusXhighNeverMovedComesBackAtMedium = ($first -contains $want2)
    $results.aChangeOfEffortKeepsThePermissionMode = ($first -contains $want3)

    '== 2. a later change in the restored tab, then the server stopped and started again'
    Set-Record $u1 'claude-sonnet-5-5' 'xhigh'
    $oldServerId = $server.Id
    & $Exe server stop | Out-Null
    [void]$server.WaitForExit(60000)
    Start-Sleep -Seconds 3
    $server = Start-Server
    $want1b = "--resume $u1 --model claude-sonnet-5-5 --effort xhigh"
    $second = Wait-Started $server.Id @($want1b, $want2, $want3)
    'claude was started as:'; $second | ForEach-Object { "  claude $_" }
    $results.theNextChangeIsTheOneTheNextRestoreFollows = ($second -contains $want1b)
    $results.theOtherTabsComeBackAsBefore = (($second -contains $want2) -and ($second -contains $want3))
    $running = @(Get-ClaudeProcesses $server.Id)
    $foreign = @($running | Where-Object { -not (Test-StandIn $_) })
    "claude.exe processes under the server: $($running.Count), not the stand-in: $($foreign.Count)"
    $foreign | ForEach-Object { "  not the stand-in: pid $($_.pid) $($_.commandLine)" }
    $results.onlyTheStandInClaudeRan = ($running.Count -gt 0) -and ($foreign.Count -eq 0)
}
finally {
    foreach ($serverId in @($oldServerId, $server.Id)) { if ($serverId) { Stop-Tree $serverId } }
    if ($server -and -not $server.HasExited) { Stop-Process -Id $server.Id -Force -ErrorAction SilentlyContinue }
}
''
'RESULTS'
$results.GetEnumerator() | ForEach-Object { '{0,-62} {1}' -f $_.Key, $(if ($_.Value) { 'PASS' } else { 'FAIL' }) }
if ($results.Values -contains $false) { exit 1 }
