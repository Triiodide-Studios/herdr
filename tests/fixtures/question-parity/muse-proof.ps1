param(
    [string]$Muse = 'C:/Users/BlakeThomas/AppData/Local/Programs/muse/muse-bin-1.4.12-R6909.1.exe',
    [string]$Gate = 'C:/Users/BlakeThomas/herdr-config/plugins/luna/bin/herdr-luna-question-parity.exe'
)

$ErrorActionPreference = 'Stop'
$hooks = @{ Stop = @(@{ hooks = @(@{ type = 'command'; command = "$Gate native-stop muse"; timeout = 10 }) }) } |
    ConvertTo-Json -Compress -Depth 8
& $Muse --trust-workspace --model muse-wonder-full-1.0 --reasoning-effort xhigh -c "hooks=$hooks"
