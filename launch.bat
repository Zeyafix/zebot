@echo off
setlocal EnableExtensions DisableDelayedExpansion

for %%I in ("%~dp0.") do set "REPO_ROOT=%%~fI"
set "SCRIPT_PATH=%~f0"
set "PS_EXE=%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe"
if not exist "%PS_EXE%" set "PS_EXE=powershell.exe"

set "ZEBOT_LAUNCHER_ROOT=%REPO_ROOT%"
set "ZEBOT_LAUNCHER_SCRIPT=%SCRIPT_PATH%"

if "%~1"=="" goto :interactive
if not "%~2"=="" goto :invalid_args

if /I "%~1"=="start" goto :entry_start
if /I "%~1"=="stop" goto :entry_stop
if /I "%~1"=="status" goto :entry_status
if /I "%~1"=="help" goto :show_help
if /I "%~1"=="-h" goto :show_help
if /I "%~1"=="--help" goto :show_help
if "%~1"=="/?" goto :show_help
goto :invalid_args

:entry_start
call :cmd_start
exit /b %ERRORLEVEL%

:entry_stop
call :cmd_stop
exit /b %ERRORLEVEL%

:entry_status
call :invoke_ps status
exit /b %ERRORLEVEL%

:show_help
call :print_usage
exit /b 0

:invalid_args
echo Error: invalid arguments.
echo.
call :print_usage
exit /b 2

:interactive
call :invoke_ps status
set "STATUS_CODE=%ERRORLEVEL%"
if "%STATUS_CODE%"=="3" goto :interactive_start
if "%STATUS_CODE%"=="0" goto :interactive_prompt_stop
set "EXIT_CODE=%STATUS_CODE%"
goto :interactive_pause

:interactive_start
echo.
call :prepare_start
set "PREP_CODE=%ERRORLEVEL%"
if not "%PREP_CODE%"=="0" (
    set "EXIT_CODE=%PREP_CODE%"
    goto :interactive_pause
)
call :invoke_ps run-interactive
set "EXIT_CODE=%ERRORLEVEL%"
if not "%EXIT_CODE%"=="0" goto :interactive_pause
exit /b 0

:interactive_prompt_stop
echo.
set "STOP_CHOICE=Y"
set /p "STOP_CHOICE=Stop Zebot now? [Y/n]: "
if /I "%STOP_CHOICE%"=="Y" goto :interactive_stop
if /I "%STOP_CHOICE%"=="YES" goto :interactive_stop
echo Keeping Zebot running.
set "EXIT_CODE=0"
goto :interactive_pause

:interactive_stop
call :cmd_stop
set "EXIT_CODE=%ERRORLEVEL%"
goto :interactive_pause

:interactive_pause
echo.
echo Press Enter to close this window...
set /p "_ZEBOT_CLOSE="
exit /b %EXIT_CODE%

:cmd_start
call :invoke_ps check-running
set "CHECK_CODE=%ERRORLEVEL%"
if "%CHECK_CODE%"=="0" exit /b 0
if not "%CHECK_CODE%"=="3" exit /b %CHECK_CODE%

call :prepare_start
if errorlevel 1 exit /b %ERRORLEVEL%

call :invoke_ps start
exit /b %ERRORLEVEL%

:prepare_start
if not exist "%REPO_ROOT%\.env" goto :check_env_fallback
goto :maybe_build

:check_env_fallback
if defined DISCORD_TOKEN goto :maybe_build
echo Error: .env file not found at "%REPO_ROOT%\.env".
echo Copy .env.example to .env and fill in the required values (see docs/quick-start.md).
exit /b 1

:maybe_build
if not exist "%REPO_ROOT%\Cargo.toml" goto :check_binary_exists

where cargo >nul 2>&1
if errorlevel 1 goto :cargo_missing

echo Building Zebot...
pushd "%REPO_ROOT%"
if errorlevel 1 (
    echo Error: failed to access repository directory "%REPO_ROOT%".
    exit /b 1
)
cargo build --bin zebot
set "BUILD_CODE=%ERRORLEVEL%"
popd
if not "%BUILD_CODE%"=="0" (
    echo Error: cargo build failed. Ensure the Rust toolchain, C/C++ Build Tools, and cmake are installed ^(see docs/quick-start.md^).
    exit /b 1
)
goto :check_binary_exists

:cargo_missing
if exist "%REPO_ROOT%\target\debug\zebot.exe" (
    echo Warning: cargo was not found on PATH; using existing binary at target\debug\zebot.exe.
    goto :check_binary_exists
)
echo Error: Rust toolchain (cargo) was not found on PATH, and target\debug\zebot.exe does not exist.
echo Install Rust from https://rustup.rs/ and C/C++ Build Tools + cmake (see docs/quick-start.md).
exit /b 1

:check_binary_exists
if not exist "%REPO_ROOT%\target\debug\zebot.exe" (
    echo Error: Zebot binary was not found at "%REPO_ROOT%\target\debug\zebot.exe".
    exit /b 1
)
exit /b 0

:cmd_stop
call :invoke_ps stop
exit /b %ERRORLEVEL%

:invoke_ps
set "ZEBOT_PS_ACTION=%~1"
"%PS_EXE%" -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command "& { $lines = [System.IO.File]::ReadAllLines($env:ZEBOT_LAUNCHER_SCRIPT); $idx = [Array]::IndexOf($lines, '::ZEBOT_PS_BEGIN'); if ($idx -lt 0) { Write-Error 'Internal launcher error: missing PowerShell payload marker.'; exit 1 }; $code = ($lines[($idx + 1)..($lines.Length - 1)] -join [Environment]::NewLine); & ([ScriptBlock]::Create($code)) }"
exit /b %ERRORLEVEL%

:print_usage
echo Usage: launch.bat [start ^| stop ^| status ^| help]
echo.
echo Commands:
echo   ^(no arguments^)  Interactive mode: run Zebot in this console ^(Enter or close window stops it^), or prompt to stop if already running.
echo   start           Build ^(when Cargo.toml and cargo are present^) and start Zebot in the background.
echo   stop            Stop Zebot and any child audio processes ^(yt-dlp, ffmpeg^) cleanly.
echo   status          Show whether Zebot is running or stopped.
echo   help            Show this help message.
echo.
echo Exit codes:
echo   0  Success ^(or running for status^)
echo   1  Error ^(missing prerequisites, build failure, startup failure, or stop failure^)
echo   2  Invalid command-line arguments
echo   3  Stopped ^(for status^)
exit /b 0

::ZEBOT_PS_BEGIN
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repoRoot = [System.IO.Path]::GetFullPath($env:ZEBOT_LAUNCHER_ROOT)
$exePath = [System.IO.Path]::GetFullPath((Join-Path $repoRoot 'target\debug\zebot.exe'))
$launcherDir = Join-Path $repoRoot 'temp\launcher'
$pidFile = Join-Path $launcherDir 'zebot.pid'
$stdoutLog = Join-Path $launcherDir 'zebot.stdout.log'
$stderrLog = Join-Path $launcherDir 'zebot.stderr.log'
$action = $env:ZEBOT_PS_ACTION

function Get-NormalizedPath {
    param([string]$Path)
    if ([string]::IsNullOrWhiteSpace($Path)) {
        return $null
    }
    try {
        return [System.IO.Path]::GetFullPath($Path.Trim())
    } catch {
        return $null
    }
}

function Read-SharedFileText {
    param([string]$Path)
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        return ''
    }
    $stream = $null
    $reader = $null
    try {
        $stream = [System.IO.File]::Open(
            $Path,
            [System.IO.FileMode]::Open,
            [System.IO.FileAccess]::Read,
            [System.IO.FileShare]::ReadWrite
        )
        $reader = New-Object System.IO.StreamReader($stream, [System.Text.Encoding]::UTF8, $true)
        return $reader.ReadToEnd()
    } catch {
        return ''
    } finally {
        if ($null -ne $reader) { $reader.Dispose() }
        elseif ($null -ne $stream) { $stream.Dispose() }
    }
}

function Get-ProcessCreationTicks {
    param($CimProcess)
    if ($null -eq $CimProcess -or $null -eq $CimProcess.CreationDate) {
        return [long]0
    }
    return ([datetime]$CimProcess.CreationDate).ToUniversalTime().Ticks
}

function Get-LiveZebotProcesses {
    $candidates = @(Get-CimInstance -ClassName Win32_Process -Filter "Name = 'zebot.exe'" -ErrorAction SilentlyContinue)
    $matched = New-Object System.Collections.Generic.List[object]
    foreach ($proc in $candidates) {
        $procExe = Get-NormalizedPath $proc.ExecutablePath
        if ($null -ne $procExe -and $procExe.Equals($exePath, [System.StringComparison]::OrdinalIgnoreCase)) {
            $matched.Add($proc)
        }
    }
    return @($matched.ToArray() | Sort-Object ProcessId)
}

function Read-PidState {
    if (-not (Test-Path -LiteralPath $pidFile -PathType Leaf)) {
        return $null
    }
    try {
        $raw = [System.IO.File]::ReadAllText($pidFile, [System.Text.Encoding]::UTF8).Trim()
    } catch {
        return [pscustomobject]@{ Invalid = $true; Pid = 0; StartTimeUtcTicks = [long]0; ExecutablePath = $null }
    }
    if ([string]::IsNullOrWhiteSpace($raw)) {
        return [pscustomobject]@{ Invalid = $true; Pid = 0; StartTimeUtcTicks = [long]0; ExecutablePath = $null }
    }
    if ($raw -match '^\d+$') {
        return [pscustomobject]@{
            Invalid = $false
            Pid = [int]$raw
            StartTimeUtcTicks = [long]0
            ExecutablePath = $null
        }
    }
    try {
        $parsed = $raw | ConvertFrom-Json
        $parsedPid = [int]$parsed.pid
        if ($parsedPid -le 0) {
            return [pscustomobject]@{ Invalid = $true; Pid = 0; StartTimeUtcTicks = [long]0; ExecutablePath = $null }
        }
        $ticks = [long]0
        if ($null -ne $parsed.PSObject.Properties['startTimeUtcTicks'] -and $null -ne $parsed.startTimeUtcTicks) {
            $ticks = [long]$parsed.startTimeUtcTicks
        }
        $trackedExe = $null
        if ($null -ne $parsed.PSObject.Properties['executablePath'] -and $null -ne $parsed.executablePath) {
            $trackedExe = [string]$parsed.executablePath
        }
        return [pscustomobject]@{
            Invalid = $false
            Pid = $parsedPid
            StartTimeUtcTicks = $ticks
            ExecutablePath = $trackedExe
        }
    } catch {
        return [pscustomobject]@{ Invalid = $true; Pid = 0; StartTimeUtcTicks = [long]0; ExecutablePath = $null }
    }
}

function Write-PidState {
    param(
        [int]$ProcessId,
        [long]$StartTimeUtcTicks
    )
    [System.IO.Directory]::CreateDirectory($launcherDir) | Out-Null
    $payload = [ordered]@{
        pid = $ProcessId
        startTimeUtcTicks = $StartTimeUtcTicks
        executablePath = $exePath
    } | ConvertTo-Json -Depth 2
    $utf8NoBom = New-Object System.Text.UTF8Encoding($false)
    [System.IO.File]::WriteAllText($pidFile, $payload + [Environment]::NewLine, $utf8NoBom)
}

function Remove-PidState {
    if (Test-Path -LiteralPath $pidFile -PathType Leaf) {
        Remove-Item -LiteralPath $pidFile -Force -ErrorAction SilentlyContinue
    }
}

function Get-SyncedLauncherState {
    $live = @(Get-LiveZebotProcesses)
    $tracked = Read-PidState
    $staleCleaned = $false

    if ($null -ne $tracked) {
        $matchesLive = $false
        if (-not $tracked.Invalid) {
            foreach ($proc in $live) {
                if ([int]$proc.ProcessId -eq $tracked.Pid) {
                    $liveTicks = Get-ProcessCreationTicks $proc
                    if ($tracked.StartTimeUtcTicks -le 0 -or $liveTicks -eq 0 -or $liveTicks -eq $tracked.StartTimeUtcTicks) {
                        $matchesLive = $true
                        break
                    }
                }
            }
        }
        if (-not $matchesLive) {
            Remove-PidState
            $staleCleaned = $true
        }
    }

    if ($live.Count -gt 0 -and -not (Test-Path -LiteralPath $pidFile -PathType Leaf)) {
        $primary = $live[0]
        Write-PidState -ProcessId ([int]$primary.ProcessId) -StartTimeUtcTicks (Get-ProcessCreationTicks $primary)
    }

    return [pscustomobject]@{
        Running = ($live.Count -gt 0)
        Processes = $live
        StaleCleaned = $staleCleaned
    }
}

function Get-DescendantProcessInfo {
    param([int[]]$RootPids)
    $all = @(Get-CimInstance -ClassName Win32_Process -ErrorAction SilentlyContinue)
    $childrenByParent = @{}
    foreach ($proc in $all) {
        $ppid = [int]$proc.ParentProcessId
        if (-not $childrenByParent.ContainsKey($ppid)) {
            $childrenByParent[$ppid] = New-Object System.Collections.Generic.List[object]
        }
        $childrenByParent[$ppid].Add($proc)
    }

    $visited = New-Object 'System.Collections.Generic.HashSet[int]'
    $queue = New-Object 'System.Collections.Generic.Queue[int]'
    foreach ($rootPid in $RootPids) {
        if ($visited.Add($rootPid)) {
            $queue.Enqueue($rootPid)
        }
    }

    $descendants = New-Object System.Collections.Generic.List[object]
    while ($queue.Count -gt 0) {
        $current = $queue.Dequeue()
        if (-not $childrenByParent.ContainsKey($current)) {
            continue
        }
        foreach ($child in $childrenByParent[$current]) {
            $childPid = [int]$child.ProcessId
            if ($visited.Add($childPid)) {
                $descendants.Add([pscustomobject]@{
                    Pid = $childPid
                    Name = [string]$child.Name
                    Ticks = Get-ProcessCreationTicks $child
                })
                $queue.Enqueue($childPid)
            }
        }
    }

    return @($descendants.ToArray())
}

function Test-TrackedProcessAlive {
    param(
        [int]$ProcessId,
        [long]$ExpectedTicks
    )
    $proc = Get-CimInstance -ClassName Win32_Process -Filter "ProcessId = $ProcessId" -ErrorAction SilentlyContinue
    if ($null -eq $proc) {
        return $false
    }
    if ($ExpectedTicks -gt 0) {
        $actualTicks = Get-ProcessCreationTicks $proc
        if ($actualTicks -gt 0 -and $actualTicks -ne $ExpectedTicks) {
            return $false
        }
    }
    return $true
}

function Wait-TrackedProcessesExit {
    param(
        [object[]]$Tracked,
        [int]$TimeoutMs
    )
    $deadline = [DateTime]::UtcNow.AddMilliseconds($TimeoutMs)
    while ([DateTime]::UtcNow -lt $deadline) {
        $anyAlive = $false
        foreach ($item in $Tracked) {
            if (Test-TrackedProcessAlive -ProcessId $item.Pid -ExpectedTicks $item.Ticks) {
                $anyAlive = $true
                break
            }
        }
        if (-not $anyAlive) {
            return $true
        }
        Start-Sleep -Milliseconds 150
    }
    foreach ($item in $Tracked) {
        if (Test-TrackedProcessAlive -ProcessId $item.Pid -ExpectedTicks $item.Ticks) {
            return $false
        }
    }
    return $true
}

function Invoke-Status {
    $state = Get-SyncedLauncherState
    if ($state.StaleCleaned) {
        Write-Output 'Cleaned stale PID file (temp\launcher\zebot.pid).'
    }
    if ($state.Running) {
        $pids = ($state.Processes | ForEach-Object { [int]$_.ProcessId }) -join ', '
        Write-Output "Zebot is running (PID $pids)."
        if ((Test-Path -LiteralPath $stdoutLog -PathType Leaf) -or (Test-Path -LiteralPath $stderrLog -PathType Leaf)) {
            Write-Output 'Logs: temp\launcher\zebot.stdout.log, temp\launcher\zebot.stderr.log'
        }
        exit 0
    }
    Write-Output 'Zebot is stopped.'
    exit 3
}

function Invoke-CheckRunning {
    $state = Get-SyncedLauncherState
    if ($state.Running) {
        $pids = ($state.Processes | ForEach-Object { [int]$_.ProcessId }) -join ', '
        Write-Output "Zebot is already running (PID $pids)."
        exit 0
    }
    exit 3
}

function Invoke-Start {
    $state = Get-SyncedLauncherState
    if ($state.Running) {
        $pids = ($state.Processes | ForEach-Object { [int]$_.ProcessId }) -join ', '
        Write-Output "Zebot is already running (PID $pids)."
        exit 0
    }

    [System.IO.Directory]::CreateDirectory($launcherDir) | Out-Null

    $spawnResultFile = Join-Path $launcherDir ("spawn-" + [Guid]::NewGuid().ToString('N') + '.json')
    Remove-Item -LiteralPath $stdoutLog -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $stderrLog -Force -ErrorAction SilentlyContinue

    $psExe = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
    if (-not (Test-Path -LiteralPath $psExe -PathType Leaf)) {
        $psExe = 'powershell.exe'
    }

    $envMap = [System.Collections.Generic.Dictionary[string, string]]::new([System.StringComparer]::OrdinalIgnoreCase)
    foreach ($entry in [System.Environment]::GetEnvironmentVariables().GetEnumerator()) {
        $k = [string]$entry.Key
        if ([string]::IsNullOrWhiteSpace($k) -or $k.StartsWith('=')) {
            continue
        }
        $envMap[$k] = [string]$entry.Value
    }
    $envMap['ZEBOT_SPAWN_EXE'] = $exePath
    $envMap['ZEBOT_SPAWN_WORKDIR'] = $repoRoot
    $envMap['ZEBOT_SPAWN_STDOUT'] = $stdoutLog
    $envMap['ZEBOT_SPAWN_STDERR'] = $stderrLog
    $envMap['ZEBOT_SPAWN_RESULT'] = $spawnResultFile

    $envArray = New-Object 'string[]' $envMap.Count
    $envIdx = 0
    foreach ($kv in $envMap.GetEnumerator()) {
        $envArray[$envIdx] = "$($kv.Key)=$($kv.Value)"
        $envIdx++
    }

    $helperScript = @'
$ErrorActionPreference = 'Stop'
$utf8NoBom = New-Object System.Text.UTF8Encoding($false)
try {
    $p = Start-Process -FilePath $env:ZEBOT_SPAWN_EXE -WorkingDirectory $env:ZEBOT_SPAWN_WORKDIR -RedirectStandardOutput $env:ZEBOT_SPAWN_STDOUT -RedirectStandardError $env:ZEBOT_SPAWN_STDERR -WindowStyle Hidden -PassThru
    $res = [ordered]@{ pid = $p.Id; error = $null } | ConvertTo-Json -Compress
} catch {
    $res = [ordered]@{ pid = 0; error = $_.Exception.Message } | ConvertTo-Json -Compress
}
[System.IO.File]::WriteAllText($env:ZEBOT_SPAWN_RESULT, $res, $utf8NoBom)
'@
    $encodedHelper = [Convert]::ToBase64String([System.Text.Encoding]::Unicode.GetBytes($helperScript))
    $helperCmd = "`"$psExe`" -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -EncodedCommand $encodedHelper"

    try {
        $startupInfo = New-CimInstance -ClassName Win32_ProcessStartup -Property @{
            ShowWindow = [uint16]0
            EnvironmentVariables = $envArray
        } -ClientOnly
        $createRes = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{
            CommandLine = $helperCmd
            CurrentDirectory = $repoRoot
            ProcessStartupInformation = $startupInfo
        }
    } catch {
        Remove-Item -LiteralPath $spawnResultFile -Force -ErrorAction SilentlyContinue
        Write-Output "Error: failed to launch $exePath`: $($_.Exception.Message)"
        exit 1
    }

    if ($null -eq $createRes -or [int]$createRes.ReturnValue -ne 0) {
        Remove-Item -LiteralPath $spawnResultFile -Force -ErrorAction SilentlyContinue
        $code = if ($null -ne $createRes) { $createRes.ReturnValue } else { 'unknown' }
        Write-Output "Error: failed to launch detached helper for $exePath (WMI ReturnValue $code)."
        exit 1
    }

    $spawnInfo = $null
    $spawnDeadline = [DateTime]::UtcNow.AddSeconds(10)
    while ([DateTime]::UtcNow -lt $spawnDeadline) {
        if (Test-Path -LiteralPath $spawnResultFile -PathType Leaf) {
            $rawResult = (Read-SharedFileText $spawnResultFile).Trim()
            if ($rawResult.Length -gt 0) {
                try {
                    $spawnInfo = $rawResult | ConvertFrom-Json
                    break
                } catch {
                }
            }
        }
        Start-Sleep -Milliseconds 50
    }
    Remove-Item -LiteralPath $spawnResultFile -Force -ErrorAction SilentlyContinue

    if ($null -eq $spawnInfo) {
        Write-Output "Error: timed out waiting for $exePath to launch."
        exit 1
    }
    if (-not [string]::IsNullOrWhiteSpace([string]$spawnInfo.error) -or [int]$spawnInfo.pid -le 0) {
        Write-Output "Error: failed to launch $exePath`: $($spawnInfo.error)"
        exit 1
    }

    $spawnedPid = [int]$spawnInfo.pid
    $proc = $null
    try {
        $proc = [System.Diagnostics.Process]::GetProcessById($spawnedPid)
    } catch {
        $proc = $null
    }

    $cim = Get-CimInstance -ClassName Win32_Process -Filter "ProcessId = $spawnedPid" -ErrorAction SilentlyContinue
    $trackedTicks = Get-ProcessCreationTicks $cim
    if ($null -ne $cim) {
        Write-PidState -ProcessId $spawnedPid -StartTimeUtcTicks $trackedTicks
    }

    $preflightPassed = $false
    $exitedEarly = $false
    $deadline = [DateTime]::UtcNow.AddSeconds(15)
    while ([DateTime]::UtcNow -lt $deadline) {
        if ($null -ne $proc) {
            if ($proc.HasExited) {
                $exitedEarly = $true
                break
            }
        } elseif (-not (Test-TrackedProcessAlive -ProcessId $spawnedPid -ExpectedTicks $trackedTicks)) {
            $exitedEarly = $true
            break
        }

        $stdoutText = Read-SharedFileText $stdoutLog
        if ($stdoutText.IndexOf('zebot preflight checks passed', [System.StringComparison]::OrdinalIgnoreCase) -ge 0) {
            $preflightPassed = $true
            Start-Sleep -Milliseconds 500
            break
        }
        Start-Sleep -Milliseconds 150
    }

    if (-not $exitedEarly) {
        if ($null -ne $proc -and $proc.HasExited) {
            $exitedEarly = $true
        } elseif (-not (Test-TrackedProcessAlive -ProcessId $spawnedPid -ExpectedTicks $trackedTicks)) {
            $exitedEarly = $true
        }
    }

    if ($exitedEarly) {
        $exitCodeText = 'non-zero'
        if ($null -ne $proc) {
            try {
                $null = $proc.WaitForExit(1000)
                $exitCodeText = [string]$proc.ExitCode
            } catch {
            }
        }
        Remove-PidState
        Write-Output "Error: Zebot failed to start (exit code $exitCodeText)."
        $stderrText = (Read-SharedFileText $stderrLog).Trim()
        $stdoutText = (Read-SharedFileText $stdoutLog).Trim()
        if ($stderrText.Length -gt 0) {
            Write-Output $stderrText
        } elseif ($stdoutText.Length -gt 0) {
            Write-Output $stdoutText
        }
        Write-Output 'See temp\launcher\zebot.stderr.log and docs\quick-start.md for details.'
        exit 1
    }

    $cimAfter = Get-CimInstance -ClassName Win32_Process -Filter "ProcessId = $spawnedPid" -ErrorAction SilentlyContinue
    if ($null -eq $cimAfter) {
        Remove-PidState
        Write-Output 'Error: Zebot exited unexpectedly right after startup.'
        $stderrText = (Read-SharedFileText $stderrLog).Trim()
        if ($stderrText.Length -gt 0) {
            Write-Output $stderrText
        }
        exit 1
    }

    Write-PidState -ProcessId $spawnedPid -StartTimeUtcTicks (Get-ProcessCreationTicks $cimAfter)
    Write-Output "Zebot started (PID $spawnedPid)."
    Write-Output 'Stdout log: temp\launcher\zebot.stdout.log'
    Write-Output 'Stderr log: temp\launcher\zebot.stderr.log'
    Write-Output 'Stop Zebot with: launch.bat stop'
    if (-not $preflightPassed) {
        Write-Output 'Note: Zebot is still initializing; inspect the log files above if it does not connect.'
    }
    exit 0
}

function Invoke-TaskKill {
    param([string[]]$Arguments)
    $previousPreference = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $null = & taskkill.exe @Arguments 2>&1
        return [int]$LASTEXITCODE
    } finally {
        $ErrorActionPreference = $previousPreference
    }
}

function Send-GracefulCtrlC {
    param([int]$TargetPid)
    $psExe = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
    if (-not (Test-Path -LiteralPath $psExe -PathType Leaf)) {
        $psExe = 'powershell.exe'
    }
    $sigScript = @"
`$ErrorActionPreference = 'SilentlyContinue'
Add-Type -Name CtrlSig -Namespace ZebotLauncher -MemberDefinition '
[DllImport("kernel32.dll", SetLastError=true)] public static extern bool FreeConsole();
[DllImport("kernel32.dll", SetLastError=true)] public static extern bool AttachConsole(uint dwProcessId);
[DllImport("kernel32.dll", SetLastError=true)] public static extern bool SetConsoleCtrlHandler(IntPtr HandlerRoutine, bool Add);
[DllImport("kernel32.dll", SetLastError=true)] public static extern bool GenerateConsoleCtrlEvent(uint dwCtrlEvent, uint dwProcessGroupId);
'
`$null = [ZebotLauncher.CtrlSig]::FreeConsole()
if ([ZebotLauncher.CtrlSig]::AttachConsole([uint32]$TargetPid)) {
    `$null = [ZebotLauncher.CtrlSig]::SetConsoleCtrlHandler([IntPtr]::Zero, `$true)
    `$ok = [ZebotLauncher.CtrlSig]::GenerateConsoleCtrlEvent(0, 0)
    Start-Sleep -Milliseconds 100
    `$null = [ZebotLauncher.CtrlSig]::FreeConsole()
    if (`$ok) { exit 0 }
}
exit 1
"@
    $encoded = [Convert]::ToBase64String([System.Text.Encoding]::Unicode.GetBytes($sigScript))
    try {
        $helper = Start-Process -FilePath $psExe `
            -ArgumentList '-NoLogo', '-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-EncodedCommand', $encoded `
            -WindowStyle Hidden `
            -PassThru
        if ($helper.WaitForExit(5000)) {
            return ($helper.ExitCode -eq 0)
        }
        try { $helper.Kill() } catch {}
    } catch {
    }
    return $false
}

function Stop-ProcessTreeList {
    param([object[]]$Targets)
    $rootPids = @($Targets | ForEach-Object { [int]$_.Pid })
    $descendants = @(Get-DescendantProcessInfo -RootPids $rootPids)
    $allTracked = @($Targets) + @($descendants)

    foreach ($target in $Targets) {
        if (Send-GracefulCtrlC -TargetPid $target.Pid) {
            $null = Wait-TrackedProcessesExit -Tracked @($target) -TimeoutMs 5000
        }
        if (Test-TrackedProcessAlive -ProcessId $target.Pid -ExpectedTicks $target.Ticks) {
            $gracefulExit = Invoke-TaskKill -Arguments @('/PID', [string]$target.Pid, '/T')
            if ($gracefulExit -eq 0) {
                $null = Wait-TrackedProcessesExit -Tracked @($target) -TimeoutMs 2000
            }
        }
        if (Test-TrackedProcessAlive -ProcessId $target.Pid -ExpectedTicks $target.Ticks) {
            $null = Invoke-TaskKill -Arguments @('/PID', [string]$target.Pid, '/T', '/F')
        }
    }

    foreach ($desc in $descendants) {
        if (Test-TrackedProcessAlive -ProcessId $desc.Pid -ExpectedTicks $desc.Ticks) {
            $null = Invoke-TaskKill -Arguments @('/PID', [string]$desc.Pid, '/T', '/F')
        }
    }

    $allGone = Wait-TrackedProcessesExit -Tracked $allTracked -TimeoutMs 3000
    $survivors = @()
    if (-not $allGone) {
        $survivors = @($allTracked | Where-Object {
            Test-TrackedProcessAlive -ProcessId $_.Pid -ExpectedTicks $_.Ticks
        })
    }

    $userChildCount = @($descendants | Where-Object {
        -not $_.Name.Equals('conhost.exe', [System.StringComparison]::OrdinalIgnoreCase)
    }).Count

    return [pscustomobject]@{
        Survivors = $survivors
        UserChildCount = $userChildCount
    }
}

function Read-SharedFileTail {
    param(
        [string]$Path,
        [ref]$ByteOffset
    )
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        return ''
    }
    $stream = $null
    try {
        $stream = [System.IO.File]::Open(
            $Path,
            [System.IO.FileMode]::Open,
            [System.IO.FileAccess]::Read,
            [System.IO.FileShare]::ReadWrite
        )
        if ($stream.Length -le $ByteOffset.Value) {
            return ''
        }
        $null = $stream.Seek($ByteOffset.Value, [System.IO.SeekOrigin]::Begin)
        $toRead = [int]($stream.Length - $ByteOffset.Value)
        $buffer = New-Object 'byte[]' $toRead
        $readCount = $stream.Read($buffer, 0, $toRead)
        if ($readCount -le 0) {
            return ''
        }
        $ByteOffset.Value += $readCount
        return [System.Text.Encoding]::UTF8.GetString($buffer, 0, $readCount)
    } catch {
        return ''
    } finally {
        if ($null -ne $stream) { $stream.Dispose() }
    }
}

function Register-InteractiveCloseGuard {
    param(
        [int]$TargetPid,
        [string]$PidFilePath
    )
    try {
        if (-not ('ZebotLauncher.ConsoleCloseGuard' -as [type])) {
            Add-Type -TypeDefinition @'
using System;
using System.Diagnostics;
using System.IO;
using System.Runtime.InteropServices;

namespace ZebotLauncher {
    public static class ConsoleCloseGuard {
        private delegate bool ConsoleCtrlHandler(uint ctrlType);
        private static ConsoleCtrlHandler _handler;
        private static int _targetPid;
        private static string _pidFile;

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool SetConsoleCtrlHandler(ConsoleCtrlHandler handler, bool add);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool SetConsoleCtrlHandler(IntPtr handler, bool add);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool FreeConsole();

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool AttachConsole(uint dwProcessId);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool GenerateConsoleCtrlEvent(uint dwCtrlEvent, uint dwProcessGroupId);

        public static void Install(int targetPid, string pidFile) {
            _targetPid = targetPid;
            _pidFile = pidFile;
            _handler = OnCtrlEvent;
            SetConsoleCtrlHandler(_handler, true);
        }

        public static void Clear() {
            _targetPid = 0;
        }

        private static bool OnCtrlEvent(uint ctrlType) {
            if (ctrlType != 2 && ctrlType != 5 && ctrlType != 6) {
                return false;
            }
            int pid = _targetPid;
            if (pid <= 0) {
                return false;
            }
            _targetPid = 0;
            try {
                Process proc = null;
                try { proc = Process.GetProcessById(pid); } catch { }
                if (proc != null && !proc.HasExited) {
                    FreeConsole();
                    if (AttachConsole((uint)pid)) {
                        SetConsoleCtrlHandler(IntPtr.Zero, true);
                        GenerateConsoleCtrlEvent(0, 0);
                        FreeConsole();
                    }
                    if (!proc.WaitForExit(3500)) {
                        try {
                            var tk = new ProcessStartInfo("taskkill.exe", "/PID " + pid + " /T /F");
                            tk.CreateNoWindow = true;
                            tk.UseShellExecute = false;
                            using (var k = Process.Start(tk)) {
                                if (k != null) { k.WaitForExit(1500); }
                            }
                        } catch { }
                    }
                }
                if (!string.IsNullOrEmpty(_pidFile) && File.Exists(_pidFile)) {
                    try { File.Delete(_pidFile); } catch { }
                }
            } catch { }
            return false;
        }
    }
}
'@
        }
        [ZebotLauncher.ConsoleCloseGuard]::Install($TargetPid, $PidFilePath)
    } catch {
    }
}

function Invoke-Stop {
    $state = Get-SyncedLauncherState
    if (-not $state.Running) {
        if ($state.StaleCleaned) {
            Write-Output 'Cleaned stale PID file (temp\launcher\zebot.pid).'
        }
        Write-Output 'Zebot is already stopped.'
        exit 0
    }

    $targets = @($state.Processes | ForEach-Object {
        [pscustomobject]@{
            Pid = [int]$_.ProcessId
            Name = [string]$_.Name
            Ticks = Get-ProcessCreationTicks $_
        }
    })

    $result = Stop-ProcessTreeList -Targets $targets
    if ($result.Survivors.Count -gt 0) {
        $remPids = ($result.Survivors | ForEach-Object { $_.Pid }) -join ', '
        Write-Output "Error: failed to stop process(es): $remPids."
        exit 1
    }

    Remove-PidState
    $stoppedPids = ($targets | ForEach-Object { $_.Pid }) -join ', '
    Write-Output "Zebot stopped (PID $stoppedPids)."
    if ($result.UserChildCount -gt 0) {
        Write-Output "Terminated $($result.UserChildCount) child process(es)."
    }
    exit 0
}

function Invoke-InteractiveRun {
    $state = Get-SyncedLauncherState
    if ($state.Running) {
        $pids = ($state.Processes | ForEach-Object { [int]$_.ProcessId }) -join ', '
        Write-Output "Zebot is already running (PID $pids)."
        exit 0
    }

    [System.IO.Directory]::CreateDirectory($launcherDir) | Out-Null
    Remove-Item -LiteralPath $stdoutLog -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $stderrLog -Force -ErrorAction SilentlyContinue

    $proc = $null
    $trackedTicks = [long]0
    $stopRequested = $false
    [long]$stdoutOffset = 0
    [long]$stderrOffset = 0

    try {
        $proc = Start-Process -FilePath $exePath `
            -WorkingDirectory $repoRoot `
            -RedirectStandardOutput $stdoutLog `
            -RedirectStandardError $stderrLog `
            -WindowStyle Hidden `
            -PassThru

        $cim = Get-CimInstance -ClassName Win32_Process -Filter "ProcessId = $($proc.Id)" -ErrorAction SilentlyContinue
        $trackedTicks = Get-ProcessCreationTicks $cim
        Write-PidState -ProcessId $proc.Id -StartTimeUtcTicks $trackedTicks
        Register-InteractiveCloseGuard -TargetPid $proc.Id -PidFilePath $pidFile

        Write-Output "Starting Zebot (PID $($proc.Id))..."
        Write-Output 'Press Enter or close this window to stop Zebot.'
        Write-Output '(To run in the background, use: launch.bat start)'
        Write-Output ''

        $inputRedirected = [Console]::IsInputRedirected
        $stdinTask = $null
        if ($inputRedirected) {
            $stdinTask = [Console]::In.ReadLineAsync()
        } else {
            while ([Console]::KeyAvailable) {
                $null = [Console]::ReadKey($true)
            }
        }

        while (-not $proc.HasExited) {
            $outChunk = Read-SharedFileTail -Path $stdoutLog -ByteOffset ([ref]$stdoutOffset)
            if ($outChunk.Length -gt 0) {
                [Console]::Out.Write($outChunk)
            }
            $errChunk = Read-SharedFileTail -Path $stderrLog -ByteOffset ([ref]$stderrOffset)
            if ($errChunk.Length -gt 0) {
                [Console]::Out.Write($errChunk)
            }

            if ($inputRedirected) {
                if ($null -ne $stdinTask -and $stdinTask.IsCompleted) {
                    $stopRequested = $true
                    break
                }
            } else {
                while ([Console]::KeyAvailable) {
                    $key = [Console]::ReadKey($true)
                    if ($key.Key -eq [ConsoleKey]::Enter) {
                        $stopRequested = $true
                        break
                    }
                }
                if ($stopRequested) {
                    break
                }
            }

            Start-Sleep -Milliseconds 100
        }

        if ($stopRequested) {
            if ('ZebotLauncher.ConsoleCloseGuard' -as [type]) {
                [ZebotLauncher.ConsoleCloseGuard]::Clear()
            }
            Write-Output ''
            Write-Output "Stopping Zebot (PID $($proc.Id))..."
            $target = [pscustomobject]@{
                Pid = [int]$proc.Id
                Name = 'zebot.exe'
                Ticks = $trackedTicks
            }
            $stopResult = Stop-ProcessTreeList -Targets @($target)
            $outTail = Read-SharedFileTail -Path $stdoutLog -ByteOffset ([ref]$stdoutOffset)
            if ($outTail.Length -gt 0) {
                [Console]::Out.Write($outTail)
            }
            $errTail = Read-SharedFileTail -Path $stderrLog -ByteOffset ([ref]$stderrOffset)
            if ($errTail.Length -gt 0) {
                [Console]::Out.Write($errTail)
            }
            Remove-PidState
            if ($stopResult.Survivors.Count -gt 0) {
                $remPids = ($stopResult.Survivors | ForEach-Object { $_.Pid }) -join ', '
                Write-Output "Error: failed to stop process(es): $remPids."
                exit 1
            }
            Write-Output "Zebot stopped (PID $($proc.Id))."
            exit 0
        }

        $null = $proc.WaitForExit(1000)
        $outTail = Read-SharedFileTail -Path $stdoutLog -ByteOffset ([ref]$stdoutOffset)
        if ($outTail.Length -gt 0) {
            [Console]::Out.Write($outTail)
        }
        $errTail = Read-SharedFileTail -Path $stderrLog -ByteOffset ([ref]$stderrOffset)
        if ($errTail.Length -gt 0) {
            [Console]::Out.Write($errTail)
        }

        $exitCode = [int]$proc.ExitCode
        Remove-PidState
        if ($exitCode -ne 0) {
            Write-Output ''
            Write-Output "Error: Zebot exited with code $exitCode."
            Write-Output 'See temp\launcher\zebot.stderr.log and docs\quick-start.md for details.'
            exit 1
        }
        Write-Output ''
        Write-Output 'Zebot stopped.'
        exit 0
    } finally {
        if ('ZebotLauncher.ConsoleCloseGuard' -as [type]) {
            try { [ZebotLauncher.ConsoleCloseGuard]::Clear() } catch {}
        }
        if ($null -ne $proc) {
            try {
                if (-not $proc.HasExited) {
                    $target = [pscustomobject]@{
                        Pid = [int]$proc.Id
                        Name = 'zebot.exe'
                        Ticks = $trackedTicks
                    }
                    $null = Stop-ProcessTreeList -Targets @($target)
                }
            } catch {
            }
            Remove-PidState
            try { $proc.Dispose() } catch {}
        }
    }
}

switch ($action) {
    'status' { Invoke-Status }
    'check-running' { Invoke-CheckRunning }
    'start' { Invoke-Start }
    'stop' { Invoke-Stop }
    'run-interactive' { Invoke-InteractiveRun }
    default {
        Write-Output "Error: unknown internal action '$action'."
        exit 1
    }
}
