param(
    [ValidateRange(10,600)][int]$Seconds = 60,
    [string]$ExecutablePath = (Join-Path $PSScriptRoot '..\target\x86_64-pc-windows-msvc\release\NetBar.exe'),
    [string]$OutputPath = (Join-Path $PSScriptRoot '..\artifacts\performance-2.0.json')
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$executable = (Resolve-Path -LiteralPath $ExecutablePath).Path
$candidates = @(Get-Process -Name NetBar -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $executable })
if ($candidates.Count -ne 1) { throw 'Start the selected NetBar executable before measuring.' }
$netbarProcess = $candidates[0]
Start-Sleep -Seconds 2
$netbarProcess.Refresh()
$cpuStart = $netbarProcess.TotalProcessorTime.TotalSeconds
$watch = [System.Diagnostics.Stopwatch]::StartNew()
$samples = [System.Collections.Generic.List[object]]::new()
for ($index = 0; $index -lt $Seconds; $index++) {
    Start-Sleep -Seconds 1
    $netbarProcess.Refresh()
    if ($netbarProcess.HasExited) { throw 'NetBar stopped while measuring.' }
    $samples.Add([pscustomobject]@{
        elapsed_seconds = [Math]::Round($watch.Elapsed.TotalSeconds, 3)
        working_set_bytes = $netbarProcess.WorkingSet64
        private_bytes = $netbarProcess.PrivateMemorySize64
        handles = $netbarProcess.HandleCount
        threads = $netbarProcess.Threads.Count
    })
}
$netbarProcess.Refresh()
$elapsed = $watch.Elapsed.TotalSeconds
$cpuSeconds = $netbarProcess.TotalProcessorTime.TotalSeconds - $cpuStart
$processors = [Environment]::ProcessorCount
$report = [ordered]@{
    version = (Get-Item -LiteralPath $executable).VersionInfo.ProductVersion
    duration_seconds = [Math]::Round($elapsed, 3)
    logical_processors = $processors
    cpu_seconds = [Math]::Round($cpuSeconds, 6)
    cpu_percent_total = [Math]::Round(100 * $cpuSeconds / $elapsed / $processors, 4)
    cpu_percent_one_core = [Math]::Round(100 * $cpuSeconds / $elapsed, 4)
    executable_bytes = (Get-Item -LiteralPath $executable).Length
    working_set_average_bytes = [long](($samples | Measure-Object -Property working_set_bytes -Average).Average)
    working_set_peak_bytes = ($samples | Measure-Object -Property working_set_bytes -Maximum).Maximum
    private_average_bytes = [long](($samples | Measure-Object -Property private_bytes -Average).Average)
    private_peak_bytes = ($samples | Measure-Object -Property private_bytes -Maximum).Maximum
    handles_first = $samples[0].handles
    handles_last = $samples[$samples.Count - 1].handles
    threads_peak = ($samples | Measure-Object -Property threads -Maximum).Maximum
    samples = $samples
}
$resolvedOutput = [System.IO.Path]::GetFullPath($OutputPath)
New-Item -ItemType Directory -Path ([System.IO.Path]::GetDirectoryName($resolvedOutput)) -Force | Out-Null
$report | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $resolvedOutput -Encoding utf8
Write-Output ('Average CPU, all logical processors: ' + $report.cpu_percent_total + '%')
Write-Output ('Average working set: ' + [Math]::Round($report.working_set_average_bytes / 1MB, 2) + ' MiB')
Write-Output ('Average private bytes: ' + [Math]::Round($report.private_average_bytes / 1MB, 2) + ' MiB')
Write-Output ('Report: ' + $resolvedOutput)
