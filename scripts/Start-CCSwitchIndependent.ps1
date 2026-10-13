#Requires -Version 7.0
[CmdletBinding()]
param(
    [string]$ExecutablePath = (Join-Path $env:LOCALAPPDATA 'Programs\CC Switch\cc-switch.exe'),
    [ValidateRange(1, 15)][int]$TimeoutSeconds = 10
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if (-not $IsWindows) { throw 'CC Switch independent launch is supported only on Windows.' }
$file = Get-Item -LiteralPath $ExecutablePath -ErrorAction Stop
if ($file.PSIsContainer -or $file.Name -ine 'cc-switch.exe') {
    throw 'ExecutablePath must identify cc-switch.exe.'
}
$properties = 'ProcessId', 'Name', 'ParentProcessId', 'ExecutablePath', 'CreationDate', 'SessionId'
$existing = @(Get-CimInstance -ClassName Win32_Process -Filter "Name = 'cc-switch.exe'" -Property $properties -ErrorAction Stop)
if ($existing.Count -gt 1) {
    throw 'Multiple CC Switch processes already exist; check their state before launching another manager.'
}
if ($existing.Count -eq 1) {
    $current = $existing[0]
    if ([string]::IsNullOrWhiteSpace($current.ExecutablePath) -or $null -eq $current.CreationDate -or
        -not [string]::Equals($current.ExecutablePath, $file.FullName, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'Another or unreadable CC Switch instance already exists; no new process was launched.'
    }
    $caller = Get-CimInstance -ClassName Win32_Process -Filter "ProcessId = $PID" -Property $properties -ErrorAction Stop
    if ($null -eq $caller -or $current.SessionId -ne $caller.SessionId) {
        throw 'The existing CC Switch instance is not in the current Windows session; no new process was launched.'
    }
    $parent = Get-CimInstance -ClassName Win32_Process -Filter "ProcessId = $($current.ParentProcessId)" -Property $properties -ErrorAction Stop
    $explorerPid = $null
    if ($null -ne $parent -and $parent.Name -ieq 'explorer.exe' -and
        $null -ne $parent.CreationDate -and $parent.CreationDate -le $current.CreationDate -and
        $parent.SessionId -eq $current.SessionId) {
        $explorerPid = [uint32]$parent.ProcessId
    }
    return [pscustomobject]@{
        Pid = [uint32]$current.ProcessId
        Birth = $current.CreationDate.ToUniversalTime().ToString('o')
        ExplorerPid = $explorerPid
        ParentPid = [uint32]$current.ParentProcessId
        Existing = $true
    }
}
Import-Module -Name (Join-Path $PSScriptRoot 'WindowsIndependentLauncher.psm1') -Force -ErrorAction Stop
$result = Start-IndependentWindowsProcess -FilePath $file.FullName -WorkingDirectory $file.DirectoryName -TimeoutSeconds $TimeoutSeconds
$result | Add-Member -NotePropertyName Existing -NotePropertyValue $false
return $result
