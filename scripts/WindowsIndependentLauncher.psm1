#Requires -Version 7.0
Set-StrictMode -Version Latest

function Get-IndependentLaunchProcess {
    param([Parameter(Mandatory)][uint32]$ProcessId)
    $properties = 'ProcessId', 'Name', 'ParentProcessId', 'ExecutablePath', 'CreationDate', 'SessionId'
    return Get-CimInstance -ClassName Win32_Process -Filter "ProcessId = $ProcessId" -Property $properties -ErrorAction Stop
}

function Get-IndependentLaunchTargets {
    param([Parameter(Mandatory)][string]$FullPath)
    $name = [IO.Path]::GetFileName($FullPath).Replace("'", "''")
    $properties = 'ProcessId', 'Name', 'ParentProcessId', 'ExecutablePath', 'CreationDate', 'SessionId'
    $rows = @(Get-CimInstance -ClassName Win32_Process -Filter "Name = '$name'" -Property $properties -ErrorAction Stop)
    foreach ($row in $rows) {
        if ([string]::IsNullOrWhiteSpace($row.ExecutablePath) -or $null -eq $row.CreationDate) {
            throw 'A matching executable name has unreadable process identity; no new launch is safe.'
        }
        if ([string]::Equals($row.ExecutablePath, $FullPath, [StringComparison]::OrdinalIgnoreCase)) {
            $row
        }
    }
}

function Get-IndependentLaunchKey {
    param([Parameter(Mandatory)]$Process)
    return '{0}:{1}' -f $Process.ProcessId, $Process.CreationDate.ToUniversalTime().Ticks
}

function Start-IndependentWindowsProcess {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][ValidateNotNullOrEmpty()][string]$FilePath,
        [AllowEmptyString()][string]$Arguments = '',
        [string]$WorkingDirectory,
        [ValidateRange(1, 15)][int]$TimeoutSeconds = 10
    )
    $ErrorActionPreference = 'Stop'
    if (-not $IsWindows) { throw 'Independent Explorer launch is supported only on Windows.' }
    $file = Get-Item -LiteralPath $FilePath -ErrorAction Stop
    if ($file.PSIsContainer) { throw 'FilePath must identify an executable file.' }
    $fullPath = $file.FullName
    if ([string]::IsNullOrWhiteSpace($WorkingDirectory)) {
        $WorkingDirectory = $file.DirectoryName
    }
    $directory = Get-Item -LiteralPath $WorkingDirectory -ErrorAction Stop
    if (-not $directory.PSIsContainer) { throw 'WorkingDirectory must identify a directory.' }
    if (-not ('CCSwitch.IndependentLauncher.WindowOwner' -as [type])) {
        Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
namespace CCSwitch.IndependentLauncher {
    public static class WindowOwner {
        [DllImport("user32.dll", SetLastError = true)]
        public static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint processId);
    }
}
'@
    }
    $shell = $null
    $windows = $null
    $desktop = $null
    $document = $null
    $application = $null
    try {
        $caller = Get-IndependentLaunchProcess -ProcessId $PID
        if ($null -eq $caller) { throw 'The current Windows session could not be verified.' }
        $shell = New-Object -ComObject Shell.Application
        $windows = $shell.Windows()
        [int]$desktopHwnd = 0
        $desktop = $windows.FindWindowSW(0, 0, 8, [ref]$desktopHwnd, 1)
        if ($null -eq $desktop -or $desktopHwnd -eq 0) {
            throw 'The existing Explorer desktop broker is unavailable; no child-process fallback was launched.'
        }
        [uint32]$explorerPid = 0
        $threadId = [CCSwitch.IndependentLauncher.WindowOwner]::GetWindowThreadProcessId(
            [IntPtr]::new($desktopHwnd), [ref]$explorerPid)
        $explorer = Get-IndependentLaunchProcess -ProcessId $explorerPid
        if ($threadId -eq 0 -or $null -eq $explorer -or
            $explorer.Name -ine 'explorer.exe' -or
            $null -eq $explorer.CreationDate -or $explorer.SessionId -ne $caller.SessionId) {
            throw 'The desktop window is not owned by Explorer in the current session; no launch was attempted.'
        }
        $explorerKey = Get-IndependentLaunchKey -Process $explorer
        $document = $desktop.Document
        if ($null -eq $document) { throw 'The existing Explorer desktop document is unavailable.' }
        $application = $document.Application
        if ($null -eq $application) { throw 'The existing Explorer desktop application is unavailable.' }
        $before = @{}
        foreach ($row in @(Get-IndependentLaunchTargets -FullPath $fullPath)) {
            $before[(Get-IndependentLaunchKey -Process $row)] = $true
        }
        $latestExplorer = Get-IndependentLaunchProcess -ProcessId $explorerPid
        [uint32]$latestOwner = 0
        $latestThread = [CCSwitch.IndependentLauncher.WindowOwner]::GetWindowThreadProcessId(
            [IntPtr]::new($desktopHwnd), [ref]$latestOwner)
        if ($null -eq $latestExplorer -or $latestThread -eq 0 -or $latestOwner -ne $explorerPid -or
            (Get-IndependentLaunchKey -Process $latestExplorer) -ne $explorerKey) {
            throw 'Explorer identity changed before launch; no launch was attempted.'
        }
        $notBefore = [DateTime]::UtcNow
        $watch = [Diagnostics.Stopwatch]::StartNew()
        try {
            # The existing desktop COM object executes inside Explorer, not this tool job.
            # ShellExecute is attempted once. An uncertain result must never be replayed.
            $application.ShellExecute($fullPath, $Arguments, $directory.FullName, 'open', 0)
        } catch {
            throw 'Explorer launch was attempted but its outcome is unconfirmed; do not retry without checking process state.'
        }
        $candidateKey = $null
        $candidateObservedAt = 0.0
        while ($watch.Elapsed.TotalSeconds -lt $TimeoutSeconds) {
            $newRows = @(Get-IndependentLaunchTargets -FullPath $fullPath | Where-Object {
                -not $before.ContainsKey((Get-IndependentLaunchKey -Process $_))
            })
            if ($newRows.Count -gt 1) {
                throw 'More than one new matching process appeared after launch; result is ambiguous. No retry or termination was attempted.'
            }
            if ($newRows.Count -eq 1) {
                $candidate = $newRows[0]
                if ($candidate.CreationDate.ToUniversalTime() -lt $notBefore -or
                    $candidate.SessionId -ne $caller.SessionId -or
                    $candidate.ParentProcessId -ne $explorerPid) {
                    throw 'The new process could not be bound to this Explorer launch; no retry or termination was attempted.'
                }
                $parent = Get-IndependentLaunchProcess -ProcessId $explorerPid
                if ($null -eq $parent -or (Get-IndependentLaunchKey -Process $parent) -ne $explorerKey -or
                    $parent.CreationDate.ToUniversalTime() -gt $candidate.CreationDate.ToUniversalTime()) {
                    throw 'Explorer parent identity changed after launch; result is unconfirmed. Do not retry automatically.'
                }
                $key = Get-IndependentLaunchKey -Process $candidate
                if ($candidateKey -ne $key) {
                    $candidateKey = $key
                    $candidateObservedAt = $watch.Elapsed.TotalMilliseconds
                } elseif (($watch.Elapsed.TotalMilliseconds - $candidateObservedAt) -ge 200) {
                    return [pscustomobject]@{
                        Pid = [uint32]$candidate.ProcessId
                        Birth = $candidate.CreationDate.ToUniversalTime().ToString('o')
                        ExplorerPid = $explorerPid
                        ParentPid = [uint32]$candidate.ParentProcessId
                    }
                }
            } elseif ($null -ne $candidateKey) {
                throw 'The observed new process disappeared before confirmation; no automatic retry was attempted.'
            }
            Start-Sleep -Milliseconds 100
        }
        throw 'Explorer launch outcome was not confirmed within the deadline; check process state before retrying. No fallback or termination was attempted.'
    } finally {
        foreach ($comObject in @($application, $document, $desktop, $windows, $shell)) {
            if ($null -ne $comObject -and [Runtime.InteropServices.Marshal]::IsComObject($comObject)) {
                [void][Runtime.InteropServices.Marshal]::ReleaseComObject($comObject)
            }
        }
    }
}

Export-ModuleMember -Function Start-IndependentWindowsProcess
