param(
    [Parameter(Mandatory = $true)][string]$Exe,
    [string]$OutputDir = 'dist/ui-smoke'
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName System.Windows.Forms
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class FlotillaSmokeWindow {
    [StructLayout(LayoutKind.Sequential)]
    public struct Rect { public int Left, Top, Right, Bottom; }
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr window, out Rect rect);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr window);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr window, int command);
}
'@

New-Item -ItemType Directory -Force $OutputDir | Out-Null
$output = (Resolve-Path $OutputDir).Path
$process = Start-Process -FilePath (Resolve-Path $Exe).Path -PassThru

function Capture-Window([IntPtr]$Window, [string]$Name) {
    $rect = New-Object FlotillaSmokeWindow+Rect
    if (-not [FlotillaSmokeWindow]::GetWindowRect($Window, [ref]$rect)) {
        throw 'Cannot read the native window bounds'
    }
    $screen = [System.Windows.Forms.SystemInformation]::VirtualScreen
    $left = [Math]::Max($rect.Left, $screen.Left)
    $top = [Math]::Max($rect.Top, $screen.Top)
    $width = [Math]::Min($rect.Right, $screen.Right) - $left
    $height = [Math]::Min($rect.Bottom, $screen.Bottom) - $top
    if ($width -lt 600 -or $height -lt 400) { throw "Window not visible: ${width}x${height}" }
    $bitmap = New-Object System.Drawing.Bitmap($width, $height)
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    try {
        $graphics.CopyFromScreen($left, $top, 0, 0, $bitmap.Size)
        $bitmap.Save((Join-Path $output "$Name.png"), [System.Drawing.Imaging.ImageFormat]::Png)
        # A native window alone does not prove that the GPU painted its contents.
        $colors = [System.Collections.Generic.HashSet[int]]::new()
        for ($y = 40; $y -lt $height; $y += 8) {
            for ($x = 0; $x -lt $width; $x += 8) {
                [void]$colors.Add($bitmap.GetPixel($x, $y).ToArgb())
            }
        }
        if ($colors.Count -lt 16) { throw "Renderer did not paint varied content ($($colors.Count) colors)" }
        return (Get-FileHash (Join-Path $output "$Name.png") -Algorithm SHA256).Hash
    } finally {
        $graphics.Dispose()
        $bitmap.Dispose()
    }
}

try {
    $deadline = [DateTime]::UtcNow.AddSeconds(60)
    do {
        $process.Refresh()
        if ($process.HasExited) { throw "Flotilla exited during startup: $($process.ExitCode)" }
        if ($process.MainWindowHandle -ne [IntPtr]::Zero) { break }
        Start-Sleep -Milliseconds 200
    } while ([DateTime]::UtcNow -lt $deadline)
    $window = $process.MainWindowHandle
    if ($window -eq [IntPtr]::Zero) { throw 'Flotilla did not create a native window within 60 seconds' }
    [void][FlotillaSmokeWindow]::ShowWindow($window, 3)
    [void][FlotillaSmokeWindow]::SetForegroundWindow($window)
    Start-Sleep -Seconds 3
    if ([FlotillaSmokeWindow]::GetForegroundWindow() -ne $window) {
        throw 'Cannot focus the native window for the keyboard smoke'
    }
    $initial = Capture-Window $window '01-workspace'
    [System.Windows.Forms.SendKeys]::SendWait('^+n')
    Start-Sleep -Seconds 2
    $addHost = Capture-Window $window '02-add-host'
    if ($initial -eq $addHost) { throw 'Add host shortcut did not change the rendered surface' }
    # Reopening setup must reuse the draft, not drop the entity that owns an install.
    [System.Windows.Forms.SendKeys]::SendWait('127.0.0.1')
    [System.Windows.Forms.SendKeys]::SendWait('^+n')
    Start-Sleep -Milliseconds 300
    [void](Capture-Window $window '03-draft-reopened')
    [System.Windows.Forms.Clipboard]::SetText('clipboard-sentinel')
    [System.Windows.Forms.SendKeys]::SendWait('^a')
    [System.Windows.Forms.SendKeys]::SendWait('^c')
    $copyDeadline = [DateTime]::UtcNow.AddSeconds(5)
    while ([System.Windows.Forms.Clipboard]::GetText() -ne '127.0.0.1') {
        if ([DateTime]::UtcNow -ge $copyDeadline) {
            throw 'Reopening Add host lost the existing draft'
        }
        Start-Sleep -Milliseconds 100
    }
    @{
        window_title = $process.MainWindowTitle
        workspace_sha256 = $initial
        add_host_sha256 = $addHost
        draft_preserved = $true
        ok = $true
    } | ConvertTo-Json | Set-Content (Join-Path $output 'report.json')
    Get-Content (Join-Path $output 'report.json')
} finally {
    $process.Refresh()
    if (-not $process.HasExited) {
        [void]$process.CloseMainWindow()
        if (-not $process.WaitForExit(10000)) { $process.Kill() }
    }
}
