# Install this user's tray monitor, always disarmed. No elevation or scheduled task.
[CmdletBinding()]
param()
$ErrorActionPreference = 'Stop'
$taskSource = Join-Path $PSScriptRoot '..\dist\AstralPowerMonitor.exe'
if (-not (Test-Path -LiteralPath $taskSource -PathType Leaf)) {
    throw 'Build and copy the release executable to dist\AstralPowerMonitor.exe first.'
}
$taskInstallFolder = Join-Path ([Environment]::GetFolderPath('LocalApplicationData')) 'AstralPowerMonitor'
$taskTarget = Join-Path $taskInstallFolder 'AstralPowerMonitor.exe'
$taskStartupFolder = [Environment]::GetFolderPath('Startup')
if ([string]::IsNullOrWhiteSpace($taskStartupFolder)) { throw 'Windows Startup folder unavailable.' }
$taskShortcutPath = Join-Path $taskStartupFolder 'Astral Power Monitor.lnk'
$taskShell = New-Object -ComObject WScript.Shell
if (Test-Path -LiteralPath $taskShortcutPath) {
    $taskExisting = $taskShell.CreateShortcut($taskShortcutPath)
    if ($taskExisting.TargetPath -ne $taskTarget -or $taskExisting.Arguments -ne '--tray') {
        throw 'A different shortcut already uses this name. It has been left unchanged.'
    }
}
New-Item -ItemType Directory -Path $taskInstallFolder -Force | Out-Null
# Refuse to overwrite a running image: quit the monitor before installing an update.
Copy-Item -LiteralPath $taskSource -Destination $taskTarget -Force
$taskShortcut = $taskShell.CreateShortcut($taskShortcutPath)
$taskShortcut.TargetPath = $taskTarget
$taskShortcut.Arguments = '--tray'
$taskShortcut.WorkingDirectory = $taskInstallFolder
$taskShortcut.Description = 'Per-pin monitoring and audio alerts; protection starts disarmed.'
$taskShortcut.IconLocation = "$taskTarget,0"
$taskShortcut.WindowStyle = 7
$taskShortcut.Save()
$taskVerified = $taskShell.CreateShortcut($taskShortcutPath)
if ($taskVerified.TargetPath -ne $taskTarget -or $taskVerified.Arguments -ne '--tray') {
    throw 'Startup shortcut verification failed.'
}
Write-Output "Installed: $taskTarget"
Write-Output "Startup: $taskShortcutPath"
Write-Output 'Starts disarmed with audio warnings enabled. No automatic arming or restart task.'
