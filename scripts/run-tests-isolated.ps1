# Runs herdr's unit tests the way upstream does (`just test`: cargo nextest, one process per test)
# with every directory a test might write to redirected to a scratch folder.
#
# Several tests set XDG_CONFIG_HOME / XDG_STATE_HOME / HOME themselves, which is only safe when each
# test has a process of its own; plain `cargo test` runs them as threads of one process, so they race,
# poison each other's lock and, in a release build, fall back to the real %APPDATA%\herdr, where a
# test once left agent-detection\codex.toml and shadowed the real Codex manifest. Redirecting
# APPDATA, LOCALAPPDATA, USERPROFILE and HOME as well makes any fallback land in the scratch folder.
#
#   scripts\run-tests-isolated.ps1 [nextest filter...]
#
# Needs cargo-nextest. Set CARGO_TARGET_DIR (and ZIG) in the environment when they are not the defaults.
param([Parameter(ValueFromRemainingArguments = $true)] [string[]] $Filter)

$ErrorActionPreference = 'Stop'
Remove-Item Env:NoDefaultCurrentDirectoryInExePath -ErrorAction SilentlyContinue

# cargo and rustup find the toolchain through the profile directory: pin them before redirecting it.
if (-not $env:CARGO_HOME) { $env:CARGO_HOME = Join-Path $env:USERPROFILE '.cargo' }
if (-not $env:RUSTUP_HOME) { $env:RUSTUP_HOME = Join-Path $env:USERPROFILE '.rustup' }

$scratch = Join-Path ([IO.Path]::GetTempPath()) "herdr-tests-$PID"
New-Item -ItemType Directory -Force $scratch | Out-Null
try {
    $env:APPDATA = Join-Path $scratch 'roaming'
    $env:LOCALAPPDATA = Join-Path $scratch 'local'
    $env:USERPROFILE = Join-Path $scratch 'profile'
    $env:HOME = $env:USERPROFILE
    $env:XDG_CONFIG_HOME = Join-Path $scratch 'config'
    $env:XDG_STATE_HOME = Join-Path $scratch 'state'
    foreach ($dir in $env:APPDATA, $env:LOCALAPPDATA, $env:USERPROFILE, $env:XDG_CONFIG_HOME, $env:XDG_STATE_HOME) {
        New-Item -ItemType Directory -Force $dir | Out-Null
    }
    cargo nextest run --release --bin herdr --test-threads 32 --build-jobs 4 @Filter
    exit $LASTEXITCODE
}
finally {
    Remove-Item $scratch -Recurse -Force -ErrorAction SilentlyContinue
}
