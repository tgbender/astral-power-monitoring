[CmdletBinding()]
param()
$ErrorActionPreference = 'Stop'
$taskStartupFolder = [Environment]::GetFolderPath('Startup')
if ([string]::IsNullOrWhiteSpace($taskStartupFolder)) { throw 'Windows Startup folder unavailable.' }
$taskShortcutPath = Join-Path $taskStartupFolder 'Astral Power Monitor.lnk'
$taskTarget = Join-Path ([Environment]::GetFolderPath('LocalApplicationData')) 'AstralPowerMonitor\AstralPowerMonitor.exe'
if (Test-Path -LiteralPath $taskShortcutPath) {
    $taskShell = New-Object -ComObject WScript.Shell
    $taskShortcut = $taskShell.CreateShortcut($taskShortcutPath)
    if ($taskShortcut.TargetPath -ne $taskTarget -or $taskShortcut.Arguments -ne '--tray') {
        throw 'Shortcut identity differs; no files were removed.'
    }
    Remove-Item -LiteralPath $taskShortcutPath
}
Write-Output 'Startup disabled. The executable and all history/recovery files are preserved.'
