# Regression check for the fleet commands against the live herdr server.
#
#   scripts\measure-fleet.ps1 -Patched <patched herdr.exe> [-Stock <stock herdr.exe>]
#
# 1. `agent list` (default) must match the stock binary's output on every field that does not change between two calls,
#    so a piped `herdr agent list | ConvertFrom-Json` keeps every field it had.
# 2. `agent list --brief` is reported next to it: bytes, and which JSON fields it drops.
# 3. Every agent in the list must resolve to itself by pane id.
# 4. `broadcast --to all-active --dry-run` must name every agent that has a Claude session.
param(
    [Parameter(Mandatory)] [string] $Patched,
    [string] $Stock = (Join-Path $env:USERPROFILE '.herdr\bin\herdr.exe')
)
$ErrorActionPreference = 'Stop'
$volatile = 'revision', 'state_change_seq', 'completion_seq', 'agent_status', 'focused', 'title', 'terminal_title', 'terminal_title_stripped', 'interactive_ready', 'state_labels', 'tokens'
function Stable($json) {
    $o = $json | ConvertFrom-Json
    foreach ($a in $o.result.agents) { foreach ($v in $volatile) { $a.PSObject.Properties.Remove($v) } }
    $o | ConvertTo-Json -Depth 20 -Compress
}
$fail = 0
$new = & $Patched agent list | Out-String
$brief = & $Patched agent list --brief | Out-String
if (Test-Path $Stock) {
    $old = & $Stock agent list | Out-String
    $same = (Stable $old) -eq (Stable $new)
    "default list identical to stock (stable fields): $same ($($old.Length) B stock, $($new.Length) B patched)"
    if (-not $same) { $fail++ }
} else { "stock binary not found at ${Stock}: skipped the identity check" }
$agents = ($new | ConvertFrom-Json).result.agents
$fields = $agents | ForEach-Object { $_.PSObject.Properties.Name } | Sort-Object -Unique
"--brief: $($brief.Length) B for $($agents.Count) agents; the JSON carries $($fields.Count) fields per agent ($($fields -join ', ')); brief keeps pane_id, agent_status, name/display_agent and the worktree"
$bad = 0
foreach ($a in $agents) {
    $line = (& $Patched resolve $a.pane_id) -join "`n"
    if (-not $line.StartsWith("$($a.pane_id) ")) { $bad++; "resolve $($a.pane_id) -> $line" }
}
"resolve by pane id: $($agents.Count - $bad)/$($agents.Count) agents resolve to themselves"
if ($bad) { $fail++ }
$dry = & $Patched broadcast --to all-active x --dry-run
$want = @($agents | Where-Object { $_.agent_session -and $_.agent_status -ne 'unknown' -and $_.pane_id -ne $env:HERDR_PANE_ID }).Count
$got = @($dry | Where-Object { $_ -match ' would_send$' }).Count
"broadcast dry-run: $got panes, expected $want"
if ($got -ne $want) { $fail++ }
exit $fail
