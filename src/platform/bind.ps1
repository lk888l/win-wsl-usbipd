# Embedded by process.rs. All inputs are emitted as quoted PowerShell literals.
# Runs elevated only for the bind operation; never changes WSL user context.
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)

function Invoke-UsbipdBounded([string] $UsbArguments) {
    $remaining = $expiresAt - [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
    if ($remaining -le 0) { throw [TimeoutException]::new('The authorization request has expired. Retry in wsl-usb.') }
    $waitMs = [int][Math]::Min($remaining, $commandTimeoutMs)
    $usbStartInfo = [Diagnostics.ProcessStartInfo]::new()
    $usbStartInfo.FileName = $usbExecutable
    # Only literal 'state' or 'bind --busid <validated digits-digits>' is passed.
    $usbStartInfo.Arguments = $UsbArguments
    $usbStartInfo.UseShellExecute = $false
    $usbStartInfo.CreateNoWindow = $true
    $usbStartInfo.RedirectStandardOutput = $true
    $usbStartInfo.RedirectStandardError = $true
    $usbStartInfo.StandardOutputEncoding = [Text.UTF8Encoding]::new($false)
    $usbStartInfo.StandardErrorEncoding = [Text.UTF8Encoding]::new($false)
    $usbProcess = [Diagnostics.Process]::new()
    $usbProcess.StartInfo = $usbStartInfo
    try {
        if (-not $usbProcess.Start()) { throw 'Unable to start usbipd.' }
        $stdoutTask = $usbProcess.StandardOutput.ReadToEndAsync()
        $stderrTask = $usbProcess.StandardError.ReadToEndAsync()
        if (-not $usbProcess.WaitForExit($waitMs)) {
            $usbProcess.Kill()
            $usbProcess.WaitForExit()
            throw [TimeoutException]::new('usbipd timed out; refresh device state before retrying.')
        }
        if (-not $stdoutTask.Wait(1000) -or -not $stderrTask.Wait(1000)) { throw 'usbipd output did not close.' }
        if ($usbProcess.ExitCode -ne 0) { throw "usbipd failed ($($usbProcess.ExitCode)): $($stderrTask.Result) $($stdoutTask.Result)" }
        return $stdoutTask.Result
    } finally {
        $usbProcess.Dispose()
    }
}

try {
    $state = Invoke-UsbipdBounded 'state' | ConvertFrom-Json
    $currentDevice = @($state.Devices | Where-Object { $_.BusId -ceq $busId })
    if ($currentDevice.Count -ne 1 -or $currentDevice[0].InstanceId -cne $expectedInstance) {
        [Console]::Error.WriteLine('The USB device changed while waiting for authorization. Refresh and select it again.')
        exit 42
    }
    if ($null -eq $currentDevice[0].PSObject.Properties['PersistedGuid']) { throw 'Invalid usbipd state schema.' }
    if ($null -eq $currentDevice[0].PersistedGuid) {
        Invoke-UsbipdBounded "bind --busid $busId" | Out-Null
    }
    exit 0
} catch [TimeoutException] {
    [Console]::Error.WriteLine($_.Exception.Message)
    exit 124
} catch {
    [Console]::Error.WriteLine($_.Exception.Message)
    exit 1
}
