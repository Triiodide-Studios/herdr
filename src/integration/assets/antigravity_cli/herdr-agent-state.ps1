# installed by herdr
# managed by herdr; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# HERDR_INTEGRATION_ID=antigravity_cli
# HERDR_INTEGRATION_VERSION=4

# Session-only: this hook reports the Antigravity conversation so Herdr can
# resume the pane. Lifecycle state comes from Herdr's screen detection.

param([string]$Action = "")

# Antigravity CLI expects a JSON object on stdout and this hook never injects
# anything, so every exit path emits an empty object.
function Exit-Hook {
    Write-Output "{}"
    exit 0
}

if ($Action -ne "session") { Exit-Hook }
if ($env:HERDR_ENV -ne "1") { Exit-Hook }
if ([string]::IsNullOrWhiteSpace($env:HERDR_PANE_ID)) { Exit-Hook }

$inputText = [Console]::In.ReadToEnd()
try {
    $payload = if ([string]::IsNullOrWhiteSpace($inputText)) { $null } else { $inputText | ConvertFrom-Json }
} catch {
    Exit-Hook
}

if ($null -eq $payload) { Exit-Hook }

$conversationId = if ($payload.conversationId -is [string]) { $payload.conversationId } else { $null }
if ([string]::IsNullOrWhiteSpace($conversationId)) { Exit-Hook }

# Subagents (a /swarm's workers, research and image agents) fire PreInvocation with their own
# conversationId; each is listed in the conversation that spawned it, under
# brain\<parent>\.system_generated\subagents\<id>.json, and is not the pane's conversation.
if ($payload.artifactDirectoryPath -is [string] -and -not [string]::IsNullOrWhiteSpace($payload.artifactDirectoryPath)) {
    $brain = Split-Path -Parent ($payload.artifactDirectoryPath.TrimEnd('\', '/'))
    $listed = Get-ChildItem -LiteralPath $brain -Directory -ErrorAction SilentlyContinue | Where-Object {
        Test-Path -LiteralPath (Join-Path $_.FullName (Join-Path ".system_generated" (Join-Path "subagents" "$conversationId.json")))
    } | Select-Object -First 1
    if ($null -ne $listed) { Exit-Hook }
}

$seq = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
$herdr = if ([string]::IsNullOrWhiteSpace($env:HERDR_BIN_PATH)) { "herdr" } else { $env:HERDR_BIN_PATH }
try {
    $sessionArgs = @(
        "pane",
        "report-agent-session",
        $env:HERDR_PANE_ID,
        "--source",
        "herdr:antigravity_cli",
        "--agent",
        "agy",
        "--seq",
        "$seq",
        "--agent-session-id",
        "$conversationId"
    )
    if ($payload.transcriptPath -is [string] -and -not [string]::IsNullOrWhiteSpace($payload.transcriptPath)) {
        $sessionArgs += @("--agent-session-path", "$($payload.transcriptPath)")
    }
    & $herdr @sessionArgs 2>$null | Out-Null
} catch {
}

Exit-Hook
