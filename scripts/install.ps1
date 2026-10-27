# Installs or updates open_mess for the current Windows user and starts the daemon.
#
#   pwsh -NoProfile -File scripts\install.ps1            # build output of this checkout
#   pwsh -NoProfile -File scripts\install.ps1 -NoStart   # install without starting
#
# What it does: stops a running daemon, copies om.exe to %LOCALAPPDATA%\open_mess\bin, copies the
# platform modules (not the test guests) into the data folder's modules, registers the scheduled
# task "open_mess" that starts the daemon hidden at every logon of this user and every 5 minutes
# (a running daemon makes that a no-op, a stopped one comes back), and starts it.
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
# The data folder the task's daemon will use: a persistent OPEN_MESS_DATA_DIR of this user (the task
# inherits it; one set only in this shell does not count), else the default (paths.rs).
$dataDir = [Environment]::GetEnvironmentVariable('OPEN_MESS_DATA_DIR', 'User')
if (-not $dataDir) { $dataDir = Join-Path $env:APPDATA 'open_mess' }
$modules = Join-Path $dataDir 'modules'
$exe = Join-Path $Source 'target\release\om.exe'
$wasmDir = Join-Path $Source 'target\wasm32-wasip2\release'
$taskName = 'open_mess'

# Everything to install is checked before the running daemon is touched.
if (-not (Test-Path $exe)) { throw "No release build at $exe - run: cargo build --release -p open-mess" }
$built = @(Get-ChildItem $wasmDir -Filter 'open_mess_*.wasm' -ErrorAction SilentlyContinue |
    Where-Object { $_.BaseName -notmatch '^open_mess_test_' })
if ($built.Count -eq 0) {
    throw "No platform modules in $wasmDir - run: cargo build --release --target wasm32-wasip2 -p open-mess-teams"
}

# 1. Stop the daemon: the scheduled task, then any om.exe daemon of this user.
# ponytail: a hard stop; pending actions are recovered at the next start (spec 8.6). Add `om stop`
# for a graceful one if anything ever needs it.
# Stopping the task ends the agents its triggers started too; a daemon run by hand leaves its own.
function Stop-Daemon {
    if (Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue) {
        Stop-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
    }
    # `daemon` must be the first argument after the program: a bridge whose path has the word survives.
    Get-CimInstance Win32_Process -Filter "Name='om.exe'" |
        Where-Object { $_.CommandLine -match '^\s*("[^"]*"|\S+)\s+daemon(\s|$)' -and (Invoke-CimMethod -InputObject $_ -MethodName GetOwner).User -eq $env:USERNAME } |
        ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
    Start-Sleep -Milliseconds 500
}
$existing = [bool](Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue)
Stop-Daemon

# Steps 2 and 3 replace files; if one fails, the task is started again, so the previous install
# keeps running (step 2 puts the previous om.exe back itself).
try {
    # 2. The binary. A running copy cannot be overwritten on Windows, so move it aside first.
    New-Item -ItemType Directory -Force $bin | Out-Null
    # The update helper's own copy (the daemon replaces it itself during an update). Copied first:
    # while a helper runs, the file is locked, and the install fails here before om.exe is touched.
    $helper = Join-Path $bin 'om.update.exe'
    try {
        Copy-Item $exe $helper -Force
    } catch {
        throw "Cannot write $helper (an update may be running; try again in a minute): $($_.Exception.Message)"
    }
    Write-Host "om.exe -> $helper"
    $target = Join-Path $bin 'om.exe'
    $old = Join-Path $bin 'om.old.exe'
    if (Test-Path $target) {
        Remove-Item $old -Force -ErrorAction SilentlyContinue
        Move-Item $target $old -Force
    }
    try {
        Copy-Item $exe $target
    } catch {
        # A full disk, say: put the previous binary back so the task still has one to start.
        if (Test-Path $old) { Move-Item $old $target -Force }
        throw
    }
    Write-Host "om.exe -> $target"

    # 3. Platform modules: open_mess_<name>.wasm -> <name>.wasm; test guests are never installed.
    New-Item -ItemType Directory -Force $modules | Out-Null
    $built |
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
} catch {
    if ($existing -and -not $NoStart) {
        # Enabled first: a task someone disabled would ignore the start.
        Enable-ScheduledTask -TaskName $taskName | Out-Null
        Start-ScheduledTask -TaskName $taskName
    }
    throw
}

# 4. Start at logon, hidden (conhost --headless keeps the console window away), and every 5 minutes:
# conhost exits 0 whatever the daemon did, so the restart-on-failure settings alone never fire.
$action = New-ScheduledTaskAction -Execute 'conhost.exe' -Argument "--headless `"$target`" daemon" -WorkingDirectory $bin
$trigger = @(
    New-ScheduledTaskTrigger -AtLogOn -User "$env:USERDOMAIN\$env:USERNAME"
    New-ScheduledTaskTrigger -Once -At (Get-Date) -RepetitionInterval (New-TimeSpan -Minutes 5)
)
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
    -ExecutionTimeLimit ([TimeSpan]::Zero) -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1) -MultipleInstances IgnoreNew
$principal = New-ScheduledTaskPrincipal -UserId "$env:USERDOMAIN\$env:USERNAME" -LogonType Interactive -RunLevel Limited
Register-ScheduledTask -TaskName $taskName -Action $action -Trigger $trigger -Settings $settings -Principal $principal `
    -Description 'open_mess daemon (scripts/install.ps1)' -Force | Out-Null
Write-Host "scheduled task '$taskName' starts the daemon at logon and every 5 minutes if it stopped"

# The update helper task (same shape as the one the daemon registers itself, update/system.rs): runs
# om.update.exe apply-update at logon and every 5 minutes; with nothing to install it exits at once.
# No RestartCount: the 5-minute trigger is the retry.
$updateTask = 'open_mess-update'
$updateAction = New-ScheduledTaskAction -Execute 'conhost.exe' `
    -Argument "--headless `"$helper`" apply-update --data-dir `"$dataDir`"" -WorkingDirectory $bin
$updateSettings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
    -ExecutionTimeLimit ([TimeSpan]::Zero) -MultipleInstances IgnoreNew
Register-ScheduledTask -TaskName $updateTask -Action $updateAction -Trigger $trigger -Settings $updateSettings -Principal $principal `
    -Description 'open_mess update helper (scripts/install.ps1)' -Force | Out-Null
Write-Host "scheduled task '$updateTask' installs accepted updates at logon and every 5 minutes"

# The 5-minute trigger may have started the old daemon while files were replaced (it keeps running
# from om.old.exe), and IgnoreNew would then keep it: stop it once more before starting.
Stop-Daemon

if (-not $NoStart) {
    Start-ScheduledTask -TaskName $taskName
    Start-Sleep -Seconds 3
    & $target status
}
