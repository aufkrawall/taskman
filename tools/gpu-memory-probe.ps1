<#
.SYNOPSIS
    Measure TaskMan's Windows GPU Process Memory counters for one isolated child.

.DESCRIPTION
    Launches one TaskMan process with fresh temporary data/config directories and
    samples the GPU Process Memory dedicated/shared counters for that PID and
    every LUID instance it owns. The process is started hidden to the tray by
    default; use -Visible for a normal window.

    Counter names are discovered through Get-Counter and matched to their
    language-neutral Perflib identities, so this does not depend on the localized
    GPU Process Memory object or counter labels. The probe starts no shell and
    changes no persistent environment variables. It stops only the Process
    object it started and removes only its own unique temporary directory.

.EXAMPLE
    powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\gpu-memory-probe.ps1 -Renderer d3d12 -Tray

.EXAMPLE
    powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\gpu-memory-probe.ps1 -Renderer d3d12 -Visible -Seconds 30 -Json
#>
[CmdletBinding()]
param(
    [string]$Executable = "target/release/taskman.exe",

    [ValidateSet("d3d12", "vulkan", "cpu")]
    [string]$Renderer = "d3d12",

    [switch]$Tray,

    [switch]$Visible,

    [switch]$TaskbarMonitor,

    [ValidateRange(1, 3600)]
    [int]$Seconds = 20,

    [ValidateRange(250, 60000)]
    [int]$IntervalMs = 1000,

    [switch]$Json
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$script:InstancePattern = '^pid_(?<Pid>[0-9]+)_luid_(?<LuidHigh>0x[0-9A-Fa-f]+)_(?<LuidLow>0x[0-9A-Fa-f]+)_phys_(?<Phys>[0-9]+)(?:#[0-9]+)?$'
$script:StartupTimeoutSeconds = 20
$script:BaselineSettleMilliseconds = 1000
$script:MainWindowMinimumWidth = 320
$script:MainWindowMinimumHeight = 200
$script:RepoRoot = Split-Path -Parent $PSScriptRoot

function Limit-Text {
    param(
        [AllowNull()][string]$Text,
        [int]$MaximumLength = 16384
    )

    if ($null -eq $Text) { return "" }
    if ($Text.Length -le $MaximumLength) { return $Text }
    return $Text.Substring(0, $MaximumLength) + "`n[truncated]"
}

function Format-ByteCount {
    param([AllowNull()]$Bytes)

    if ($null -eq $Bytes) { return "Unknown" }
    $value = [double]$Bytes
    if ($value -lt 1024) { return ("{0} B" -f [long]$value) }
    if ($value -lt 1MB) { return ("{0:N1} KiB" -f ($value / 1KB)) }
    if ($value -lt 1GB) { return ("{0:N1} MiB" -f ($value / 1MB)) }
    return ("{0:N2} GiB" -f ($value / 1GB))
}

function ConvertTo-ByteCount {
    param([AllowNull()]$Value)

    if ($null -eq $Value) { return $null }
    try {
        $number = [double]$Value
    } catch {
        return $null
    }
    if ([double]::IsNaN($number) -or [double]::IsInfinity($number) -or $number -lt 0) {
        return $null
    }
    return [long][Math]::Round($number, [MidpointRounding]::AwayFromZero)
}

function Read-PerfLibPairs {
    param([string]$RegistryPath)

    $raw = (Get-ItemProperty -LiteralPath $RegistryPath -Name Counter -ErrorAction Stop).Counter
    $values = @($raw)
    $pairs = @()
    for ($index = 0; $index + 1 -lt $values.Count; $index += 2) {
        $id = 0
        if (-not [int]::TryParse([string]$values[$index], [ref]$id)) { continue }
        $pairs += [pscustomobject]@{
            Id = $id
            Name = ([string]$values[$index + 1]).Trim()
        }
    }
    return $pairs
}

function Get-PerfLibNameMap {
    $englishById = @{}
    $idsByLocalName = @{}
    $warnings = @()
    $perflibRoot = "HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Perflib"

    try {
        foreach ($pair in @(Read-PerfLibPairs -RegistryPath (Join-Path $perflibRoot "009"))) {
            $englishById[$pair.Id] = $pair.Name
        }
    } catch {
        $warnings += "The English Perflib name table is unavailable: $($_.Exception.Message)"
    }

    try {
        foreach ($pair in @(Read-PerfLibPairs -RegistryPath (Join-Path $perflibRoot "CurrentLanguage"))) {
            if (-not $idsByLocalName.ContainsKey($pair.Name)) {
                $idsByLocalName[$pair.Name] = New-Object System.Collections.ArrayList
            }
            [void]$idsByLocalName[$pair.Name].Add($pair.Id)
        }
    } catch {
        $warnings += "The current-language Perflib name table is unavailable: $($_.Exception.Message)"
    }

    return [pscustomobject]@{
        EnglishById = $englishById
        IdsByLocalName = $idsByLocalName
        Warnings = $warnings
    }
}

function Resolve-CanonicalPerfName {
    param(
        [string]$Name,
        $NameMap
    )

    $trimmed = $Name.Trim()
    $wanted = @("Dedicated Usage", "Shared Usage", "GPU Process Memory")
    if ($wanted -contains $trimmed) { return $trimmed }
    if ($null -eq $NameMap -or -not $NameMap.IdsByLocalName.ContainsKey($trimmed)) {
        return $trimmed
    }

    foreach ($id in @($NameMap.IdsByLocalName[$trimmed])) {
        $numericId = [int]$id
        if (-not $NameMap.EnglishById.ContainsKey($numericId)) { continue }
        $english = [string]$NameMap.EnglishById[$numericId]
        if ($wanted -contains $english) { return $english }
    }
    return $trimmed
}

function Get-CounterPathParts {
    param([string]$Path)

    $marker = "(*)\"
    $markerIndex = $Path.IndexOf($marker, [StringComparison]::Ordinal)
    if ($markerIndex -lt 0) { return $null }
    return [pscustomobject]@{
        CounterName = $Path.Substring($markerIndex + $marker.Length)
    }
}

function Get-InstanceNameFromCounterPath {
    param([string]$Path)

    $match = [regex]::Match($Path, '\((?<Instance>pid_[^)]+)\)')
    if (-not $match.Success) { return $null }
    return $match.Groups["Instance"].Value
}

function Find-GpuProcessCounterSet {
    param($NameMap)

    $discoveryErrors = @()
    $sets = @(Get-Counter -ListSet * -ErrorAction SilentlyContinue -ErrorVariable discoveryErrors)
    if ($sets.Count -eq 0) {
        $detail = "Get-Counter did not enumerate any performance counter sets."
        if ($discoveryErrors.Count -gt 0) {
            $detail += " " + (($discoveryErrors | Select-Object -First 3 | ForEach-Object {
                $_.Exception.Message
            }) -join " ")
        }
        throw $detail
    }

    $candidates = @()
    $possibleSets = @()
    foreach ($set in $sets) {
        $observedInstance = $false
        foreach ($rawPath in @($set.PathsWithInstances)) {
            $instanceName = Get-InstanceNameFromCounterPath -Path ([string]$rawPath)
            if ($null -ne $instanceName -and $instanceName -match $script:InstancePattern) {
                $observedInstance = $true
                break
            }
        }

        $objectName = Resolve-CanonicalPerfName -Name ([string]$set.CounterSetName) -NameMap $NameMap
        $dedicated = $null
        $shared = $null
        foreach ($rawPath in @($set.Paths)) {
            $parts = Get-CounterPathParts -Path ([string]$rawPath)
            if ($null -eq $parts) { continue }
            $canonicalCounter = Resolve-CanonicalPerfName -Name $parts.CounterName -NameMap $NameMap
            if ($canonicalCounter -eq "Dedicated Usage") {
                $dedicated = [pscustomobject]@{
                    Label = $parts.CounterName
                    Template = [string]$rawPath
                }
            } elseif ($canonicalCounter -eq "Shared Usage") {
                $shared = [pscustomobject]@{
                    Label = $parts.CounterName
                    Template = [string]$rawPath
                }
            }
        }

        $isGpuProcessMemory = $objectName -eq "GPU Process Memory" -or $observedInstance
        if ($isGpuProcessMemory) {
            $possibleSets += [string]$set.CounterSetName
        }
        if ($isGpuProcessMemory -and $null -ne $dedicated -and $null -ne $shared) {
            $candidates += [pscustomobject]@{
                CounterSetName = [string]$set.CounterSetName
                ObjectName = $objectName
                DedicatedLabel = $dedicated.Label
                SharedLabel = $shared.Label
                DedicatedTemplate = $dedicated.Template
                SharedTemplate = $shared.Template
            }
        }
    }

    if ($candidates.Count -eq 0) {
        $detail = "No GPU Process Memory counter set with recognized Dedicated Usage and Shared Usage counters was found."
        if ($possibleSets.Count -gt 0) {
            $detail += " Candidate set(s): " + (($possibleSets | Select-Object -Unique) -join ", ") + "."
        }
        if ($discoveryErrors.Count -gt 0) {
            $detail += " Get-Counter reported: " + (($discoveryErrors | Select-Object -First 3 | ForEach-Object {
                $_.Exception.Message
            }) -join " ")
        }
        throw $detail
    }

    if ($candidates.Count -gt 1) {
        $exact = @($candidates | Where-Object { $_.ObjectName -eq "GPU Process Memory" })
        if ($exact.Count -eq 1) {
            return $exact[0]
        }
        throw "GPU Process Memory counter discovery was ambiguous: " +
            (($candidates | ForEach-Object { $_.CounterSetName }) -join ", ")
    }
    return $candidates[0]
}

function Get-TaskManLogText {
    param([string]$DataDirectory)

    $logDirectory = Join-Path $DataDirectory "logs"
    if (-not (Test-Path -LiteralPath $logDirectory -PathType Container)) { return "" }

    $builder = New-Object System.Text.StringBuilder
    foreach ($file in @(Get-ChildItem -LiteralPath $logDirectory -File -Filter "taskman.log*" -ErrorAction SilentlyContinue)) {
        $stream = $null
        $reader = $null
        try {
            $stream = [IO.File]::Open(
                $file.FullName,
                [IO.FileMode]::Open,
                [IO.FileAccess]::Read,
                [IO.FileShare]::ReadWrite
            )
            $reader = New-Object System.IO.StreamReader($stream)
            [void]$builder.Append($reader.ReadToEnd())
        } catch {
            # A partially rotated log is diagnostic only. Startup failure below
            # still reports the process exit and captured stderr.
        } finally {
            if ($null -ne $reader) { $reader.Dispose() }
            if ($null -ne $stream) { $stream.Dispose() }
        }
    }
    return $builder.ToString()
}

function Wait-ForTaskManReady {
    param(
        $Process,
        [string]$DataDirectory
    )

    $deadline = [DateTime]::UtcNow.AddSeconds($script:StartupTimeoutSeconds)
    while ([DateTime]::UtcNow -lt $deadline) {
        $Process.Refresh()
        if ($Process.HasExited) {
            return [pscustomobject]@{
                Ready = $false
                Reason = "TaskMan exited before its renderer reported startup."
                Evidence = ""
            }
        }

        $logText = Get-TaskManLogText -DataDirectory $DataDirectory
        $engineMatch = [regex]::Match($logText, "(?im)^.*engine started.*$")
        $subpixelMatch = [regex]::Match(
            $logText,
            '(?im)^.*phase="subpixel_capability_resolved".*$'
        )
        if ($engineMatch.Success -or $subpixelMatch.Success) {
            $readinessMatch = if ($engineMatch.Success) { $engineMatch } else { $subpixelMatch }
            $readinessMarker = if ($engineMatch.Success) {
                "engine started"
            } else {
                "subpixel_capability_resolved"
            }
            $rendererMatch = [regex]::Match($logText, "(?im)^.*trying renderer.*$")
            return [pscustomobject]@{
                Ready = $true
                Reason = "Renderer readiness was confirmed by '$readinessMarker'."
                Marker = $readinessMarker
                Evidence = $readinessMatch.Value.Trim()
                RendererEvidence = $rendererMatch.Value.Trim()
            }
        }
        Start-Sleep -Milliseconds 100
    }

    return [pscustomobject]@{
        Ready = $false
        Reason = "Timed out waiting for the isolated TaskMan log to report 'engine started' or phase=`"subpixel_capability_resolved`"."
        Evidence = ""
    }
}

function Start-IsolatedTaskMan {
    param(
        [string]$ExecutablePath,
        [string]$DataDirectory,
        [string]$ConfigDirectory,
        [string]$RendererName,
        [bool]$StartHidden
    )

    $arguments = @("--instance-unresponsive")
    if ($StartHidden) { $arguments += "--minimized-to-tray" }
    $startInfo = New-Object System.Diagnostics.ProcessStartInfo
    $startInfo.FileName = $ExecutablePath
    $startInfo.Arguments = $arguments -join " "
    $startInfo.WorkingDirectory = $script:RepoRoot
    $startInfo.UseShellExecute = $false
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    $childEnvironment = $startInfo.EnvironmentVariables
    $inheritedKeysToRemove = @()
    foreach ($key in @($childEnvironment.Keys)) {
        if ($key -like "TASKMAN_*" -or
            $key -like "WGPU_*" -or
            [string]::Equals($key, "RUST_LOG", [StringComparison]::OrdinalIgnoreCase) -or
            [string]::Equals($key, "RUST_BACKTRACE", [StringComparison]::OrdinalIgnoreCase)) {
            $inheritedKeysToRemove += $key
        }
    }
    foreach ($key in $inheritedKeysToRemove) {
        [void]$childEnvironment.Remove($key)
    }
    $childEnvironment["TASKMAN_DATA_DIR"] = $DataDirectory
    $childEnvironment["TASKMAN_CONFIG_DIR"] = $ConfigDirectory
    $childEnvironment["TASKMAN_RENDERER"] = $RendererName
    # Performance demand avoids per-process network/disk ETW sessions.
    $childEnvironment["TASKMAN_TAB"] = "performance"

    $process = New-Object System.Diagnostics.Process
    $process.StartInfo = $startInfo
    try {
        if (-not $process.Start()) {
            throw "Process.Start() returned false."
        }
    } catch {
        $process.Dispose()
        throw "Could not launch $ExecutablePath`: $($_.Exception.Message)"
    }

    return [pscustomobject]@{
        Process = $process
        StdoutTask = $process.StandardOutput.ReadToEndAsync()
        StderrTask = $process.StandardError.ReadToEndAsync()
    }
}

function Assert-SpawnedTaskManAlive {
    param(
        $Process,
        [string]$Phase
    )

    $Process.Refresh()
    if ($Process.HasExited) {
        $exitCode = "unavailable"
        try {
            $exitCode = [string]$Process.ExitCode
        } catch {
            # The exit itself is the actionable failure; leave its code unknown.
        }
        throw "Spawned TaskMan PID $($Process.Id) exited $Phase (exit code $exitCode)."
    }
}

function Stop-IsolatedTaskMan {
    param(
        $Process,
        [bool]$TrayMode
    )

    $warnings = @()
    $closeRequested = $false
    $closeTimedOut = $false
    $Process.Refresh()
    if ($Process.HasExited) {
        return [pscustomobject]@{
            StopReason = "ProcessExitedBeforeProbeStop"
            Warnings = @()
        }
    }

    if (-not $TrayMode) {
        try {
            $closeRequested = $Process.CloseMainWindow()
        } catch {
            $warnings += "CloseMainWindow() failed; falling back to exact-PID Kill(): $($_.Exception.Message)"
        }

        if ($closeRequested -and $Process.WaitForExit(5000)) {
            $Process.WaitForExit()
            return [pscustomobject]@{
                StopReason = "VisibleWindowClosedGracefully"
                Warnings = $warnings
            }
        }
        if ($closeRequested) {
            $closeTimedOut = $true
        } elseif (-not $Process.HasExited) {
            $warnings += "Visible TaskMan had no main window to close; falling back to exact-PID Kill()."
        }

        $Process.Refresh()
        if ($Process.HasExited) {
            if ($closeRequested) {
                $warnings += "Visible TaskMan exited just after the five-second CloseMainWindow grace period."
                return [pscustomobject]@{
                    StopReason = "VisibleWindowClosedGracefullyLate"
                    Warnings = $warnings
                }
            }
            return [pscustomobject]@{
                StopReason = "ProcessExitedBeforeProbeStop"
                Warnings = $warnings
            }
        }
        if ($closeTimedOut) {
            $warnings += "Visible TaskMan did not exit within 5 seconds after CloseMainWindow(); falling back to exact-PID Kill()."
        }
    }

    try {
        $Process.Kill()
    } catch {
        $Process.Refresh()
        if (-not $Process.HasExited) { throw }
        if ($closeRequested) {
            return [pscustomobject]@{
                StopReason = "VisibleWindowClosedGracefullyLate"
                Warnings = $warnings
            }
        }
        return [pscustomobject]@{
            StopReason = "ProcessExitedBeforeProbeStop"
            Warnings = $warnings
        }
    }

    if (-not $Process.WaitForExit(5000)) {
        $warnings += "The exact child process did not exit within 5 seconds after Kill()."
        $stopReason = "ProbeKillTimedOut"
    } else {
        $Process.WaitForExit()
        $stopReason = if ($TrayMode) {
            "TrayStoppedExactChild"
        } else {
            "VisibleCloseFallbackKilledExactChild"
        }
    }

    return [pscustomobject]@{
        StopReason = $stopReason
        Warnings = $warnings
    }
}

function New-GpuCounterSampleResult {
    param(
        [int]$Sequence,
        [long]$ElapsedMilliseconds,
        [long]$QueryMilliseconds,
        [ValidateSet("Observed", "Absent", "Incomplete", "Unavailable")]
        [string]$Outcome,
        [AllowNull()]$DedicatedBytes = $null,
        [AllowNull()]$SharedBytes = $null,
        [int]$InstanceCount = 0,
        [int]$CompleteInstanceCount = 0,
        [int]$RawSampleCount = 0,
        [AllowNull()][string]$ErrorMessage = "",
        [object[]]$Instances = @()
    )

    return [pscustomobject]@{
        Sequence = $Sequence
        ElapsedMilliseconds = $ElapsedMilliseconds
        QueryMilliseconds = $QueryMilliseconds
        Outcome = $Outcome
        DedicatedBytes = $DedicatedBytes
        SharedBytes = $SharedBytes
        InstanceCount = $InstanceCount
        CompleteInstanceCount = $CompleteInstanceCount
        RawSampleCount = $RawSampleCount
        Error = $ErrorMessage
        Instances = @($Instances)
    }
}

function Measure-GpuCounterSample {
    param(
        $CounterSet,
        [int]$ProcessId,
        [int]$Sequence,
        [long]$ElapsedMilliseconds
    )

    $queryTimer = [Diagnostics.Stopwatch]::StartNew()
    $queryPaths = @(
        ([string]$CounterSet.DedicatedTemplate).Replace("(*)", "(pid_$($ProcessId)_*)"),
        ([string]$CounterSet.SharedTemplate).Replace("(*)", "(pid_$($ProcessId)_*)")
    )

    try {
        $counterData = Get-Counter -Counter $queryPaths -MaxSamples 1 -ErrorAction Stop
    } catch {
        $queryTimer.Stop()
        return New-GpuCounterSampleResult `
            -Sequence $Sequence `
            -ElapsedMilliseconds $ElapsedMilliseconds `
            -QueryMilliseconds $queryTimer.ElapsedMilliseconds `
            -Outcome "Unavailable" `
            -ErrorMessage (Limit-Text -Text $_.Exception.Message -MaximumLength 2048)
    }

    if ($null -eq $counterData) {
        $queryTimer.Stop()
        return New-GpuCounterSampleResult `
            -Sequence $Sequence `
            -ElapsedMilliseconds $ElapsedMilliseconds `
            -QueryMilliseconds $queryTimer.ElapsedMilliseconds `
            -Outcome "Absent"
    }

    $rawSamples = @($counterData.CounterSamples | Where-Object { $null -ne $_ })
    if ($rawSamples.Count -eq 0) {
        $queryTimer.Stop()
        return New-GpuCounterSampleResult `
            -Sequence $Sequence `
            -ElapsedMilliseconds $ElapsedMilliseconds `
            -QueryMilliseconds $queryTimer.ElapsedMilliseconds `
            -Outcome "Absent"
    }

    $rows = @{}
    $unparsedSamples = 0
    foreach ($sample in $rawSamples) {
        $instanceName = [string]$sample.InstanceName
        $instanceMatch = [regex]::Match($instanceName, $script:InstancePattern)
        if (-not $instanceMatch.Success -or [int]$instanceMatch.Groups["Pid"].Value -ne $ProcessId) {
            $unparsedSamples++
            continue
        }

        $samplePath = [string]$sample.Path
        $separatorIndex = $samplePath.LastIndexOf(")\", [StringComparison]::Ordinal)
        if ($separatorIndex -lt 0) {
            $unparsedSamples++
            continue
        }
        $counterName = $samplePath.Substring($separatorIndex + 2)

        if (-not $rows.ContainsKey($instanceName)) {
            $rows[$instanceName] = [ordered]@{
                InstanceName = $instanceName
                Pid = [int]$instanceMatch.Groups["Pid"].Value
                LuidHigh = $instanceMatch.Groups["LuidHigh"].Value
                LuidLow = $instanceMatch.Groups["LuidLow"].Value
                PhysicalAdapter = [int]$instanceMatch.Groups["Phys"].Value
                DedicatedBytes = $null
                SharedBytes = $null
            }
        }

        $row = $rows[$instanceName]
        if ($counterName -eq [string]$CounterSet.DedicatedLabel) {
            $row["DedicatedBytes"] = ConvertTo-ByteCount -Value $sample.CookedValue
        } elseif ($counterName -eq [string]$CounterSet.SharedLabel) {
            $row["SharedBytes"] = ConvertTo-ByteCount -Value $sample.CookedValue
        } else {
            $unparsedSamples++
        }
    }

    $queryTimer.Stop()
    $instances = @($rows.Values | ForEach-Object { [pscustomobject]$_ })
    if ($instances.Count -eq 0) {
        $reason = "Get-Counter returned $($rawSamples.Count) sample(s), but none matched pid_$ProcessId with the expected LUID/phys instance shape."
        return New-GpuCounterSampleResult `
            -Sequence $Sequence `
            -ElapsedMilliseconds $ElapsedMilliseconds `
            -QueryMilliseconds $queryTimer.ElapsedMilliseconds `
            -Outcome "Incomplete" `
            -RawSampleCount $rawSamples.Count `
            -ErrorMessage $reason
    }

    $completeInstances = @($instances | Where-Object {
        $null -ne $_.DedicatedBytes -and $null -ne $_.SharedBytes
    })
    if ($completeInstances.Count -ne $instances.Count) {
        return New-GpuCounterSampleResult `
            -Sequence $Sequence `
            -ElapsedMilliseconds $ElapsedMilliseconds `
            -QueryMilliseconds $queryTimer.ElapsedMilliseconds `
            -Outcome "Incomplete" `
            -InstanceCount $instances.Count `
            -CompleteInstanceCount $completeInstances.Count `
            -RawSampleCount $rawSamples.Count `
            -ErrorMessage "At least one target LUID instance lacked a valid dedicated or shared counter value." `
            -Instances $instances
    }

    [long]$dedicatedTotal = 0
    [long]$sharedTotal = 0
    foreach ($instance in $instances) {
        $dedicatedTotal += [long]$instance.DedicatedBytes
        $sharedTotal += [long]$instance.SharedBytes
    }

    $note = if ($unparsedSamples -gt 0) {
        "$unparsedSamples non-target/unrecognized counter sample(s) were ignored."
    } else {
        ""
    }
    return New-GpuCounterSampleResult `
        -Sequence $Sequence `
        -ElapsedMilliseconds $ElapsedMilliseconds `
        -QueryMilliseconds $queryTimer.ElapsedMilliseconds `
        -Outcome "Observed" `
        -DedicatedBytes $dedicatedTotal `
        -SharedBytes $sharedTotal `
        -InstanceCount $instances.Count `
        -CompleteInstanceCount $instances.Count `
        -RawSampleCount $rawSamples.Count `
        -ErrorMessage $note `
        -Instances $instances
}

function Get-InterpretedMetric {
    param(
        $Sample,
        [bool]$TrayMode,
        [ValidateSet("DedicatedBytes", "SharedBytes")]
        [string]$Metric
    )

    if ($Sample.Outcome -eq "Observed") {
        return [long]$Sample.$Metric
    }
    if ($TrayMode -and $Sample.Outcome -eq "Absent") {
        return [long]0
    }
    return $null
}

function Get-MaximumMetric {
    param(
        $Samples,
        [bool]$TrayMode,
        [ValidateSet("DedicatedBytes", "SharedBytes")]
        [string]$Metric
    )

    $values = @(
        foreach ($sample in $Samples) {
            $value = Get-InterpretedMetric -Sample $sample -TrayMode $TrayMode -Metric $Metric
            if ($null -ne $value) { [long]$value }
        }
    )
    if ($values.Count -eq 0) { return $null }
    [long]$maximum = 0
    foreach ($value in $values) {
        if ($value -gt $maximum) { $maximum = $value }
    }
    return $maximum
}

function Get-InstancePeaks {
    param($Samples)

    $peaks = @{}
    foreach ($sample in $Samples) {
        foreach ($instance in @($sample.Instances)) {
            if (-not $peaks.ContainsKey($instance.InstanceName)) {
                $peaks[$instance.InstanceName] = [ordered]@{
                    InstanceName = $instance.InstanceName
                    Pid = $instance.Pid
                    LuidHigh = $instance.LuidHigh
                    LuidLow = $instance.LuidLow
                    PhysicalAdapter = $instance.PhysicalAdapter
                    PeakDedicatedBytes = $null
                    PeakSharedBytes = $null
                    SamplesObserved = 0
                }
            }
            $peak = $peaks[$instance.InstanceName]
            $peak["SamplesObserved"] = [int]$peak["SamplesObserved"] + 1
            if ($null -ne $instance.DedicatedBytes -and
                ($null -eq $peak["PeakDedicatedBytes"] -or $instance.DedicatedBytes -gt $peak["PeakDedicatedBytes"])) {
                $peak["PeakDedicatedBytes"] = $instance.DedicatedBytes
            }
            if ($null -ne $instance.SharedBytes -and
                ($null -eq $peak["PeakSharedBytes"] -or $instance.SharedBytes -gt $peak["PeakSharedBytes"])) {
                $peak["PeakSharedBytes"] = $instance.SharedBytes
            }
        }
    }
    return @($peaks.Values | ForEach-Object { [pscustomobject]$_ } |
        Sort-Object LuidHigh, LuidLow, PhysicalAdapter)
}

function Complete-TextTask {
    param($Task)

    if ($null -eq $Task) { return "" }
    try {
        if (-not $Task.Wait(5000)) { return "[process stream did not finish within 5 seconds]" }
        return Limit-Text -Text ([string]$Task.Result)
    } catch {
        return "Could not read process output: $($_.Exception.Message)"
    }
}

function Test-PathUnderTemp {
    param(
        [string]$Path,
        [string]$TempRoot
    )

    $fullPath = [IO.Path]::GetFullPath($Path)
    $prefix = [IO.Path]::GetFullPath($TempRoot)
    if (-not $prefix.EndsWith([string][IO.Path]::DirectorySeparatorChar)) {
        $prefix += [IO.Path]::DirectorySeparatorChar
    }
    return $fullPath.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)
}

function Initialize-TaskManWindowInspector {
    if ($null -ne ("TaskManGpuProbe.WindowInspector" -as [type])) { return }

    $source = @'
using System;
using System.Collections.Generic;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Text;

namespace TaskManGpuProbe
{
    public sealed class TopLevelWindowInfo
    {
        public long Handle { get; set; }
        public string Title { get; set; }
        public bool Visible { get; set; }
        public bool ToolWindow { get; set; }
        public int Left { get; set; }
        public int Top { get; set; }
        public int Width { get; set; }
        public int Height { get; set; }
    }

    public static class WindowInspector
    {
        private delegate bool EnumWindowsProc(IntPtr hWnd, IntPtr lParam);

        [DllImport("user32.dll", SetLastError = true)]
        private static extern bool EnumWindows(EnumWindowsProc callback, IntPtr lParam);

        [DllImport("user32.dll")]
        private static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint processId);

        [DllImport("user32.dll")]
        private static extern bool IsWindowVisible(IntPtr hWnd);

        [DllImport("user32.dll", CharSet = CharSet.Unicode)]
        private static extern int GetWindowTextLengthW(IntPtr hWnd);

        [DllImport("user32.dll", CharSet = CharSet.Unicode)]
        private static extern int GetWindowTextW(IntPtr hWnd, StringBuilder text, int count);

        [DllImport("user32.dll", SetLastError = true)]
        private static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);

        [DllImport("user32.dll", EntryPoint = "GetWindowLongPtrW", SetLastError = true)]
        private static extern IntPtr GetWindowLongPtrW(IntPtr hWnd, int index);

        [DllImport("user32.dll", EntryPoint = "GetWindowLongW", SetLastError = true)]
        private static extern int GetWindowLongW(IntPtr hWnd, int index);

        [StructLayout(LayoutKind.Sequential)]
        private struct RECT
        {
            public int Left;
            public int Top;
            public int Right;
            public int Bottom;
        }

        private static long GetExtendedStyle(IntPtr hWnd)
        {
            if (IntPtr.Size == 8) return GetWindowLongPtrW(hWnd, -20).ToInt64();
            return GetWindowLongW(hWnd, -20);
        }

        public static TopLevelWindowInfo[] GetTopLevelWindows(uint processId)
        {
            List<TopLevelWindowInfo> results = new List<TopLevelWindowInfo>();
            bool enumerated = EnumWindows(delegate(IntPtr hWnd, IntPtr lParam)
            {
                uint windowProcessId;
                GetWindowThreadProcessId(hWnd, out windowProcessId);
                if (windowProcessId != processId) return true;

                int length = GetWindowTextLengthW(hWnd);
                StringBuilder title = new StringBuilder(length + 1);
                if (length > 0) GetWindowTextW(hWnd, title, title.Capacity);

                RECT rect;
                bool hasRect = GetWindowRect(hWnd, out rect);
                TopLevelWindowInfo info = new TopLevelWindowInfo();
                info.Handle = hWnd.ToInt64();
                info.Title = title.ToString();
                info.Visible = IsWindowVisible(hWnd);
                info.ToolWindow = (GetExtendedStyle(hWnd) & 0x80L) != 0;
                info.Left = hasRect ? rect.Left : 0;
                info.Top = hasRect ? rect.Top : 0;
                info.Width = hasRect ? Math.Max(0, rect.Right - rect.Left) : 0;
                info.Height = hasRect ? Math.Max(0, rect.Bottom - rect.Top) : 0;
                results.Add(info);
                return true;
            }, IntPtr.Zero);

            if (!enumerated)
            {
                throw new Win32Exception(
                    Marshal.GetLastWin32Error(),
                    "EnumWindows failed."
                );
            }
            return results.ToArray();
        }
    }
}
'@

    try {
        Add-Type -TypeDefinition $source -Language CSharp -ErrorAction Stop
    } catch {
        throw "Could not initialize the exact-child window inspector: $($_.Exception.Message)"
    }
}

function ConvertTo-TaskManWindowEvidence {
    param($Window)

    $title = [string]$Window.Title
    $hasTitle = -not [string]::IsNullOrWhiteSpace($title)
    $mainSized = (
        $Window.Width -ge $script:MainWindowMinimumWidth -and
        $Window.Height -ge $script:MainWindowMinimumHeight
    )
    $candidate = (
        $Window.Visible -and
        $hasTitle -and
        -not $Window.ToolWindow -and
        $mainSized
    )
    return [pscustomobject]@{
        Handle = "0x{0:X}" -f [uint64]$Window.Handle
        Title = $title
        Visible = [bool]$Window.Visible
        ToolWindow = [bool]$Window.ToolWindow
        Width = [int]$Window.Width
        Height = [int]$Window.Height
        MainSized = [bool]$mainSized
        MainWindowCandidate = [bool]$candidate
    }
}

function Get-ExactChildWindowCheck {
    param(
        [int]$ProcessId,
        [int]$Sequence,
        [string]$Phase
    )

    $timestamp = [DateTime]::UtcNow.ToString("o")
    try {
        $rawWindows = @(
            [TaskManGpuProbe.WindowInspector]::GetTopLevelWindows([uint32]$ProcessId)
        )
    } catch {
        return [pscustomobject]@{
            Sequence = $Sequence
            Phase = $Phase
            TimestampUtc = $timestamp
            CheckSucceeded = $false
            Error = $_.Exception.Message
            TopLevelWindowCount = 0
            VisibleMainWindow = $false
            MainWindowCandidates = @()
            Windows = @()
        }
    }

    $windows = @(
        foreach ($window in $rawWindows) {
            ConvertTo-TaskManWindowEvidence -Window $window
        }
    )
    $candidates = @($windows | Where-Object { $_.MainWindowCandidate })

    return [pscustomobject]@{
        Sequence = $Sequence
        Phase = $Phase
        TimestampUtc = $timestamp
        CheckSucceeded = $true
        Error = ""
        TopLevelWindowCount = $windows.Count
        VisibleMainWindow = $candidates.Count -gt 0
        MainWindowCandidates = $candidates
        Windows = $windows
    }
}

function Add-WindowCheckEvidence {
    param(
        [System.Collections.ArrayList]$Checks,
        [System.Collections.IDictionary]$Result,
        [int]$ProcessId,
        [int]$Sequence,
        [string]$Phase,
        [bool]$EnforceHidden
    )

    $check = Get-ExactChildWindowCheck -ProcessId $ProcessId -Sequence $Sequence -Phase $Phase
    [void]$Checks.Add($check)
    $Result["WindowChecks"] = @($Checks.ToArray())
    $Result["VisibleMainWindowObserved"] = @(
        $Checks | Where-Object { $_.VisibleMainWindow }
    ).Count -gt 0
    $Result["WindowChecksSucceeded"] = @(
        $Checks | Where-Object { -not $_.CheckSucceeded }
    ).Count -eq 0
    $Result["HiddenWindowEvidenceValid"] = (
        $Checks.Count -gt 0 -and
        [bool]$Result["WindowChecksSucceeded"] -and
        -not [bool]$Result["VisibleMainWindowObserved"]
    )

    if ($EnforceHidden -and -not $check.CheckSucceeded) {
        $Result["Assessment"] = "HiddenWindowCheckUnavailable"
        throw "Tray-mode hidden-window check failed during '$Phase': $($check.Error)"
    }
    if ($EnforceHidden -and [bool]$Result["VisibleMainWindowObserved"]) {
        $Result["Assessment"] = "HiddenMainWindowVisible"
        throw "Tray mode expected a hidden main window, but spawned PID $ProcessId exposed a visible main-sized window during '$Phase'."
    }
    return $check
}

if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
    throw "gpu-memory-probe.ps1 requires Windows performance counters."
}
if ($Tray -and $Visible) {
    throw "Use either -Tray or -Visible, not both."
}
if ($TaskbarMonitor -and $Visible) {
    throw "-TaskbarMonitor requires -Tray; visible mode does not model the tray-overlay scenario."
}

$trayMode = -not $Visible.IsPresent
$modeName = if ($trayMode) { "Tray" } else { "Visible" }
$rendererName = $Renderer.ToLowerInvariant()
$executablePath = if ([IO.Path]::IsPathRooted($Executable)) {
    [IO.Path]::GetFullPath($Executable)
} else {
    [IO.Path]::GetFullPath((Join-Path $script:RepoRoot $Executable))
}
if (-not (Test-Path -LiteralPath $executablePath -PathType Leaf)) {
    throw "$executablePath not found. Build it first; this diagnostic never compiles or downloads TaskMan."
}

$result = [ordered]@{
    Succeeded = $false
    Mode = $modeName
    Renderer = $rendererName
    TaskbarMonitor = $TaskbarMonitor.IsPresent
    ForcedTab = "performance"
    Executable = $executablePath
    ProcessId = $null
    CounterSetName = $null
    CounterObject = $null
    DedicatedCounterLabel = $null
    SharedCounterLabel = $null
    DedicatedCounterPath = $null
    SharedCounterPath = $null
    StartupEvidence = $null
    ReadinessMarker = $null
    RendererEvidence = $null
    BaselineSettleMilliseconds = $script:BaselineSettleMilliseconds
    WindowMinimumWidth = $script:MainWindowMinimumWidth
    WindowMinimumHeight = $script:MainWindowMinimumHeight
    WindowChecks = @()
    WindowChecksSucceeded = $false
    VisibleMainWindowObserved = $false
    HiddenWindowEvidenceValid = $false
    BaselineState = $null
    BaselineDedicatedBytes = $null
    BaselineSharedBytes = $null
    PeakState = "Unknown"
    PeakDedicatedBytes = $null
    PeakSharedBytes = $null
    FinalState = $null
    FinalDedicatedBytes = $null
    FinalSharedBytes = $null
    FinalInstanceCount = $null
    Assessment = "NotRun"
    SampleCount = 0
    ObservedSampleCount = 0
    AbsentSampleCount = 0
    IncompleteSampleCount = 0
    UnavailableSampleCount = 0
    Samples = @()
    InstancePeaks = @()
    FinalInstances = @()
    ExitCode = $null
    StopReason = "NotStarted"
    Stdout = ""
    Stderr = ""
    LogErrors = ""
    ErrorText = ""
    Warnings = @()
    TempCleanupSucceeded = $true
}

$warnings = @()
$fatalError = $null
$started = $null
$samples = @()
$runRoot = $null
$dataDirectory = $null
$configDirectory = $null
$cleanupSafe = $false
$stdoutText = ""
$stderrText = ""
$logText = ""
$stopReason = "NotStarted"

try {
    $nameMap = Get-PerfLibNameMap
    $warnings += @($nameMap.Warnings)

    $counterSet = Find-GpuProcessCounterSet -NameMap $nameMap
    $result["CounterSetName"] = $counterSet.CounterSetName
    $result["CounterObject"] = $counterSet.ObjectName
    $result["DedicatedCounterLabel"] = $counterSet.DedicatedLabel
    $result["SharedCounterLabel"] = $counterSet.SharedLabel

    $tempRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
    $runRoot = [IO.Path]::GetFullPath((Join-Path $tempRoot (
        "taskman-gpu-memory-$PID-" + [Guid]::NewGuid().ToString("N")
    )))
    if (-not (Test-PathUnderTemp -Path $runRoot -TempRoot $tempRoot)) {
        throw "Refusing to create a probe directory outside the temporary directory: $runRoot"
    }
    $cleanupSafe = $true
    $dataDirectory = Join-Path $runRoot "data"
    $configDirectory = Join-Path $runRoot "config"
    New-Item -ItemType Directory -Path $dataDirectory -Force | Out-Null
    New-Item -ItemType Directory -Path $configDirectory -Force | Out-Null
    if ($TaskbarMonitor) {
        # The taskbar overlay keeps the hidden UI alive; close_to_tray models
        # the user-visible close-to-tray scenario without touching real config.
        $seedConfig = "[general]`r`ntaskbar_monitor_enabled=true`r`nclose_to_tray=true`r`n"
        Set-Content -LiteralPath (Join-Path $configDirectory "config.ini") -Value $seedConfig -Encoding Ascii
    }

    Initialize-TaskManWindowInspector
    $windowChecks = New-Object System.Collections.ArrayList

    $started = Start-IsolatedTaskMan `
        -ExecutablePath $executablePath `
        -DataDirectory $dataDirectory `
        -ConfigDirectory $configDirectory `
        -RendererName $rendererName `
        -StartHidden $trayMode
    $process = $started.Process
    $result["ProcessId"] = $process.Id

    $result["DedicatedCounterPath"] = ([string]$counterSet.DedicatedTemplate).Replace(
        "(*)",
        "(pid_$($process.Id)_*)"
    )
    $result["SharedCounterPath"] = ([string]$counterSet.SharedTemplate).Replace(
        "(*)",
        "(pid_$($process.Id)_*)"
    )

    $ready = Wait-ForTaskManReady -Process $process -DataDirectory $dataDirectory
    if (-not $ready.Ready) {
        throw $ready.Reason
    }
    Assert-SpawnedTaskManAlive -Process $process -Phase "after reporting renderer startup"
    $result["StartupEvidence"] = $ready.Evidence
    $result["ReadinessMarker"] = $ready.Marker
    $result["RendererEvidence"] = $ready.RendererEvidence

    Start-Sleep -Milliseconds $script:BaselineSettleMilliseconds
    Assert-SpawnedTaskManAlive -Process $process -Phase "after the baseline settle window"
    $null = Add-WindowCheckEvidence `
        -Checks $windowChecks `
        -Result $result `
        -ProcessId $process.Id `
        -Sequence -1 `
        -Phase "after readiness and settle, before baseline" `
        -EnforceHidden $trayMode

    $measureTimer = [Diagnostics.Stopwatch]::StartNew()
    $baseline = Measure-GpuCounterSample `
        -CounterSet $counterSet `
        -ProcessId $process.Id `
        -Sequence 0 `
        -ElapsedMilliseconds 0
    Assert-SpawnedTaskManAlive -Process $process -Phase "during the baseline GPU memory sample"
    $samples += $baseline
    $null = Add-WindowCheckEvidence `
        -Checks $windowChecks `
        -Result $result `
        -ProcessId $process.Id `
        -Sequence $baseline.Sequence `
        -Phase "after baseline sample" `
        -EnforceHidden $trayMode

    while ($measureTimer.Elapsed.TotalSeconds -lt $Seconds) {
        Assert-SpawnedTaskManAlive -Process $process -Phase "before the next GPU memory sample"

        $remainingMilliseconds = [Math]::Max(
            1,
            [Math]::Ceiling(($Seconds - $measureTimer.Elapsed.TotalSeconds) * 1000)
        )
        $sleepMilliseconds = [int][Math]::Min($IntervalMs, $remainingMilliseconds)
        Start-Sleep -Milliseconds $sleepMilliseconds

        Assert-SpawnedTaskManAlive -Process $process -Phase "after the measurement interval"
        $sample = Measure-GpuCounterSample `
            -CounterSet $counterSet `
            -ProcessId $process.Id `
            -Sequence $samples.Count `
            -ElapsedMilliseconds $measureTimer.ElapsedMilliseconds
        $samples += $sample
        Assert-SpawnedTaskManAlive -Process $process -Phase "during GPU memory sampling"
        $null = Add-WindowCheckEvidence `
            -Checks $windowChecks `
            -Result $result `
            -ProcessId $process.Id `
            -Sequence $sample.Sequence `
            -Phase "after sample $($sample.Sequence)" `
            -EnforceHidden $trayMode
    }
    $measureTimer.Stop()

    $finalSample = $samples[$samples.Count - 1]

    $baselineDedicated = Get-InterpretedMetric -Sample $baseline -TrayMode $trayMode -Metric "DedicatedBytes"
    $baselineShared = Get-InterpretedMetric -Sample $baseline -TrayMode $trayMode -Metric "SharedBytes"
    $finalDedicated = Get-InterpretedMetric -Sample $finalSample -TrayMode $trayMode -Metric "DedicatedBytes"
    $finalShared = Get-InterpretedMetric -Sample $finalSample -TrayMode $trayMode -Metric "SharedBytes"

    $result["BaselineState"] = $baseline.Outcome
    $result["BaselineDedicatedBytes"] = $baselineDedicated
    $result["BaselineSharedBytes"] = $baselineShared
    $result["PeakDedicatedBytes"] = Get-MaximumMetric -Samples $samples -TrayMode $trayMode -Metric "DedicatedBytes"
    $result["PeakSharedBytes"] = Get-MaximumMetric -Samples $samples -TrayMode $trayMode -Metric "SharedBytes"
    $result["FinalState"] = $finalSample.Outcome
    $result["FinalDedicatedBytes"] = $finalDedicated
    $result["FinalSharedBytes"] = $finalShared
    $result["FinalInstanceCount"] = $finalSample.InstanceCount
    $result["FinalInstances"] = @($finalSample.Instances)
    $result["InstancePeaks"] = @(Get-InstancePeaks -Samples $samples)
    $result["SampleCount"] = $samples.Count
    $result["ObservedSampleCount"] = @($samples | Where-Object { $_.Outcome -eq "Observed" }).Count
    $result["AbsentSampleCount"] = @($samples | Where-Object { $_.Outcome -eq "Absent" }).Count
    $result["IncompleteSampleCount"] = @($samples | Where-Object { $_.Outcome -eq "Incomplete" }).Count
    $result["UnavailableSampleCount"] = @($samples | Where-Object { $_.Outcome -eq "Unavailable" }).Count
    $result["Samples"] = @(
        foreach ($sample in $samples) {
            [ordered]@{
                Sequence = $sample.Sequence
                ElapsedMilliseconds = $sample.ElapsedMilliseconds
                QueryMilliseconds = $sample.QueryMilliseconds
                Outcome = $sample.Outcome
                DedicatedBytes = $sample.DedicatedBytes
                SharedBytes = $sample.SharedBytes
                InstanceCount = $sample.InstanceCount
                CompleteInstanceCount = $sample.CompleteInstanceCount
                RawSampleCount = $sample.RawSampleCount
                Error = $sample.Error
            }
        }
    )

    $sampleErrors = @($samples | Where-Object {
        -not [string]::IsNullOrWhiteSpace([string]$_.Error)
    } | ForEach-Object {
        "Sample $($_.Sequence) ($($_.Outcome)): $($_.Error)"
    })
    $warnings += @($sampleErrors | Select-Object -Unique)

    $invalidPeakSamples = (
        $result["IncompleteSampleCount"] +
        $result["UnavailableSampleCount"] +
        $(if ($trayMode) { 0 } else { $result["AbsentSampleCount"] })
    )
    if ($null -eq $result["PeakDedicatedBytes"] -or $null -eq $result["PeakSharedBytes"]) {
        $result["PeakState"] = "Unknown"
    } elseif ($invalidPeakSamples -gt 0) {
        $result["PeakState"] = "LowerBound"
        $warnings += "Peak values are a lower bound because $invalidPeakSamples sample(s) were unavailable, incomplete, or absent without a byte value."
    } else {
        $result["PeakState"] = "Exact"
    }

    if ($finalSample.Outcome -eq "Unavailable" -or $finalSample.Outcome -eq "Incomplete") {
        $result["Assessment"] = "FinalSample$($finalSample.Outcome)"
        throw "The final GPU Process Memory sample was $($finalSample.Outcome): $($finalSample.Error)"
    }
    if ($trayMode -and -not [bool]$result["HiddenWindowEvidenceValid"]) {
        $result["Assessment"] = "HiddenWindowEvidenceUnavailable"
        throw "No HiddenZero assessment is possible without successful exact-child window evidence."
    }

    if ($trayMode) {
        if ($finalSample.Outcome -eq "Absent") {
            $result["Assessment"] = "HiddenZeroByCounterAbsence"
        } elseif ($finalDedicated -eq 0 -and $finalShared -eq 0) {
            $result["Assessment"] = "HiddenZero"
        } else {
            $result["Assessment"] = "HiddenNonzero"
        }
    } elseif ($finalSample.Outcome -eq "Observed") {
        $result["Assessment"] = "VisibleFootprintObserved"
    } else {
        $result["Assessment"] = "VisibleFootprintUnknown"
    }

    $result["Succeeded"] = $true
} catch {
    $fatalError = $_.Exception.Message
    $result["ErrorText"] = Limit-Text -Text $fatalError
} finally {
    if ($null -ne $started) {
        $process = $started.Process
        try {
            $stop = Stop-IsolatedTaskMan -Process $process -TrayMode $trayMode
            $stopReason = $stop.StopReason
            $warnings += @($stop.Warnings)

            if ($process.HasExited) {
                $result["ExitCode"] = $process.ExitCode
            }
        } catch {
            $warnings += "Could not finalize the child process: $($_.Exception.Message)"
            $stopReason = "StopFailed"
        }

        $stdoutText = Complete-TextTask -Task $started.StdoutTask
        $stderrText = Complete-TextTask -Task $started.StderrTask
        $started.Process.Dispose()
    }
    $result["StopReason"] = $stopReason
    $result["Stdout"] = $stdoutText
    $result["Stderr"] = $stderrText

    if ($null -ne $dataDirectory) {
        $logText = Get-TaskManLogText -DataDirectory $dataDirectory
        $logErrors = @(
            $logText -split "`r?`n" |
                Where-Object { $_ -match "\[ERROR\]" } |
                Select-Object -Last 20
        )
        $result["LogErrors"] = Limit-Text -Text ($logErrors -join "`n")
    }

    if (-not [string]::IsNullOrWhiteSpace($stderrText)) {
        $errorParts = @(
            @($result["ErrorText"], $stderrText) |
                Where-Object { -not [string]::IsNullOrWhiteSpace([string]$_) }
        )
        $result["ErrorText"] = Limit-Text -Text ($errorParts -join "`n")
    }
    if (-not [string]::IsNullOrWhiteSpace([string]$result["LogErrors"])) {
        $errorParts = @(
            @($result["ErrorText"], [string]$result["LogErrors"]) |
                Where-Object { -not [string]::IsNullOrWhiteSpace([string]$_) }
        )
        $result["ErrorText"] = Limit-Text -Text ($errorParts -join "`n")
    }

    if ($cleanupSafe -and $null -ne $runRoot -and (Test-Path -LiteralPath $runRoot)) {
        try {
            $tempRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
            if (-not (Test-PathUnderTemp -Path $runRoot -TempRoot $tempRoot)) {
                throw "resolved probe directory is outside the temporary directory"
            }
            Remove-Item -LiteralPath $runRoot -Recurse -Force -ErrorAction Stop
        } catch {
            $result["TempCleanupSucceeded"] = $false
            $warnings += "Could not remove the probe's own temporary directory: $($_.Exception.Message)"
        }
    }
}

if ($stopReason -eq "ProcessExitedBeforeProbeStop" -or
    $stopReason -eq "ProbeKillTimedOut" -or
    $stopReason -eq "StopFailed") {
    $result["Succeeded"] = $false
    $errorParts = @(
        @(
            [string]$result["ErrorText"],
            "The spawned TaskMan process did not remain alive until the probe stopped it (state: $stopReason)."
        ) | Where-Object { -not [string]::IsNullOrWhiteSpace([string]$_) }
    )
    $result["ErrorText"] = Limit-Text -Text ($errorParts -join "`n")
}
if (-not [bool]$result["TempCleanupSucceeded"]) {
    $result["Succeeded"] = $false
    $errorParts = @(
        @(
            [string]$result["ErrorText"],
            "The probe's isolated temporary directory could not be removed."
        ) | Where-Object { -not [string]::IsNullOrWhiteSpace([string]$_) }
    )
    $result["ErrorText"] = Limit-Text -Text ($errorParts -join "`n")
}

$result["Warnings"] = @($warnings | Select-Object -Unique)

if ($Json) {
    $result | ConvertTo-Json -Depth 10
} else {
    Write-Output ("TaskMan GPU memory probe: {0}, renderer {1}, PID {2}" -f
        $result["Mode"], $result["Renderer"], $result["ProcessId"])
    Write-Output ("Counter set: {0}" -f $result["CounterSetName"])
    Write-Output ("Counters: {0} / {1}" -f
        $result["DedicatedCounterLabel"], $result["SharedCounterLabel"])
    Write-Output ("Forced tab: {0}; taskbar overlay config: {1}" -f
        $result["ForcedTab"], $result["TaskbarMonitor"])
    Write-Output ("Startup: {0} (marker: {1}; settle: {2} ms)" -f
        $result["StartupEvidence"],
        $result["ReadinessMarker"],
        $result["BaselineSettleMilliseconds"])

    $windowCheckCount = @($result["WindowChecks"]).Count
    $summary = [pscustomobject][ordered]@{
        Baseline = "{0} dedicated, {1} shared ({2})" -f
            (Format-ByteCount $result["BaselineDedicatedBytes"]),
            (Format-ByteCount $result["BaselineSharedBytes"]),
            $result["BaselineState"]
        Peak = "{0} dedicated, {1} shared ({2})" -f
            (Format-ByteCount $result["PeakDedicatedBytes"]),
            (Format-ByteCount $result["PeakSharedBytes"]),
            $result["PeakState"]
        Final = "{0} dedicated, {1} shared ({2})" -f
            (Format-ByteCount $result["FinalDedicatedBytes"]),
            (Format-ByteCount $result["FinalSharedBytes"]),
            $result["FinalState"]
        WindowEvidence = "checks={0}; succeeded={1}; visibleMain={2}; hiddenValid={3}" -f
            $windowCheckCount,
            $result["WindowChecksSucceeded"],
            $result["VisibleMainWindowObserved"],
            $result["HiddenWindowEvidenceValid"]
        Assessment = $result["Assessment"]
        Samples = $result["SampleCount"]
        Stop = $result["StopReason"]
    }
    $summary | Format-List

    $instancePeaks = @($result["InstancePeaks"])
    if ($instancePeaks.Count -gt 0) {
        Write-Output "LUID instances observed:"
        $instancePeaks |
            Select-Object InstanceName, LuidHigh, LuidLow, PhysicalAdapter,
                PeakDedicatedBytes, PeakSharedBytes, SamplesObserved |
            Format-Table -AutoSize
    } else {
        Write-Output "LUID instances observed: none"
    }

    $warnings = @($result["Warnings"])
    if ($warnings.Count -gt 0) {
        Write-Output "Warnings:"
        $warnings | ForEach-Object { Write-Output ("  " + $_) }
    }
    if (-not [string]::IsNullOrWhiteSpace([string]$result["ErrorText"])) {
        Write-Output ("Error: " + $result["ErrorText"])
    }
}

if (-not [bool]$result["Succeeded"]) {
    exit 1
}
