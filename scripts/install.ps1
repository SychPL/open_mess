# Installs or updates open_mess for the current Windows user and starts the daemon.
#
#   pwsh -NoProfile -File scripts\install.ps1            # build output of this checkout
#   pwsh -NoProfile -File scripts\install.ps1 -NoStart   # install without starting
#
# What it does: stops a running daemon, copies om.exe to %LOCALAPPDATA%\open_mess\bin, copies the
# platform modules (not the test guests) into %APPDATA%\open_mess\modules, registers the scheduled
# task "open_mess" that starts the daemon hidden at every logon of this user, and starts it.
# A changed module file waits for approval again in the configuration window (spec 8.3).
#
# Run it from a normal terminal, not from a tool inside Claude Desktop: that is an MSIX package and
# sees a virtualized %APPDATA% / %LOCALAPPDATA% (CONTRIBUTING.md).
#Requires -Version 7
param(
    [string]$Source = (Split-Path -Parent $PSScriptRoot),
    [switch]$NoStart
)
$ErrorActionPreference = 'Stop'

$bin = Join-Path $env:LOCALAPPDATA 'open_mess\bin'
$modules = Join-Path $env:APPDATA 'open_mess\modules'
$exe = Join-Path $Source 'target\release\om.exe'
$wasmDir = Join-Path $Source 'target\wasm32-wasip2\release'
$taskName = 'open_mess'

if (-not (Test-Path $exe)) { throw "No release build at $exe - run: cargo build --release -p open-mess" }

# 1. Stop the daemon: the scheduled task, then any om.exe daemon of this user.
if (Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue) {
    Stop-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
}
# ponytail: a hard stop; pending actions are recovered at the next start (spec 8.6). Add `om stop`
# for a graceful one if anything ever needs it.
Get-CimInstance Win32_Process -Filter "Name='om.exe'" |
    Where-Object { $_.CommandLine -match '\bdaemon\b' } |
    ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
Start-Sleep -Milliseconds 500

# 2. The binary. A running copy cannot be overwritten on Windows, so move it aside first.
New-Item -ItemType Directory -Force $bin | Out-Null
$target = Join-Path $bin 'om.exe'
if (Test-Path $target) {
    $old = Join-Path $bin 'om.old.exe'
    Remove-Item $old -Force -ErrorAction SilentlyContinue
    Move-Item $target $old -Force
}
Copy-Item $exe $target
Write-Host "om.exe -> $target"

# 3. Platform modules: open_mess_<name>.wasm -> <name>.wasm; test guests are never installed.
New-Item -ItemType Directory -Force $modules | Out-Null
Get-ChildItem $wasmDir -Filter 'open_mess_*.wasm' -ErrorAction SilentlyContinue |
    Where-Object { $_.BaseName -notmatch '^open_mess_test_' } |
    ForEach-Object {
        $name = $_.BaseName -replace '^open_mess_', ''
        $dest = Join-Path $modules "$name.wasm"
        $same = (Test-Path $dest) -and ((Get-FileHash $dest).Hash -eq (Get-FileHash $_.FullName).Hash)
        if ($same) {
            Write-Host "module $name unchanged"
        } else {
            Copy-Item $_.FullName $dest -Force
            Write-Host "module $name -> $dest (approve it again in the window)"
        }
    }

# 4. Start at logon, hidden (conhost --headless keeps the console window away), restart on failure.
$action = New-ScheduledTaskAction -Execute 'conhost.exe' -Argument "--headless `"$target`" daemon" -WorkingDirectory $bin
$trigger = New-ScheduledTaskTrigger -AtLogOn -User "$env:USERDOMAIN\$env:USERNAME"
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
    -ExecutionTimeLimit ([TimeSpan]::Zero) -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1) -MultipleInstances IgnoreNew
$principal = New-ScheduledTaskPrincipal -UserId "$env:USERDOMAIN\$env:USERNAME" -LogonType Interactive -RunLevel Limited
Register-ScheduledTask -TaskName $taskName -Action $action -Trigger $trigger -Settings $settings -Principal $principal `
    -Description 'open_mess daemon (scripts/install.ps1)' -Force | Out-Null
Write-Host "scheduled task '$taskName' starts the daemon at logon"

if (-not $NoStart) {
    Start-ScheduledTask -TaskName $taskName
    Start-Sleep -Seconds 3
    & $target status
}
