# Embedded by process.rs; $helperScript and $powerShellExecutable are quoted data.
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
try {
    $encodedHelper = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($helperScript))
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]::new($identity)
    if ($principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        & $powerShellExecutable -NoLogo -NoProfile -NonInteractive -EncodedCommand $encodedHelper
        exit $LASTEXITCODE
    }
    $helperProcess = Start-Process -FilePath $powerShellExecutable -ArgumentList @('-NoLogo', '-NoProfile', '-NonInteractive', '-EncodedCommand', $encodedHelper) -Verb RunAs -WindowStyle Hidden -PassThru
    $helperProcess.WaitForExit()
    $helperProcess.Refresh()
    $helperExitCode = $helperProcess.ExitCode
    if ($helperExitCode -eq 42) {
        [Console]::Error.WriteLine('USB device identity changed after UAC; bind was cancelled. Refresh and select again.')
    } elseif ($helperExitCode -eq 124) {
        [Console]::Error.WriteLine('Authorization or usbipd bind timed out. Refresh before retrying.')
    } elseif ($helperExitCode -ne 0) {
        [Console]::Error.WriteLine('Elevated bind failed. Run usbipd bind --busid <BUSID> in an Administrator terminal for details.')
    }
    $helperProcess.Dispose()
    exit $helperExitCode
} catch {
    [Console]::Error.WriteLine($_.Exception.Message)
    exit 1
}
