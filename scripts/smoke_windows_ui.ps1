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
    [StructLayout(LayoutKind.Sequential)]
    public struct Point { public int X, Y; }
    [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr window, out Rect rect);
    [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr window, ref Point point);
    [DllImport("user32.dll")] public static extern uint GetDpiForWindow(IntPtr window);
    [DllImport("user32.dll")] public static extern bool IsZoomed(IntPtr window);
    [DllImport("user32.dll")] public static extern bool IsIconic(IntPtr window);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern void mouse_event(uint flags, uint dx, uint dy, uint data, UIntPtr extra);
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

# Screen point of a title-bar location, `FromRight` logical pixels left of the
# client area's right edge (caption buttons are 46 px wide, the bar 36 px tall).
function Get-TitlePoint([IntPtr]$Window, [double]$FromRight, [double]$FromLeft = -1) {
    $client = New-Object FlotillaSmokeWindow+Rect
    if (-not [FlotillaSmokeWindow]::GetClientRect($Window, [ref]$client)) { throw 'Cannot read the client area' }
    $origin = New-Object FlotillaSmokeWindow+Point
    if (-not [FlotillaSmokeWindow]::ClientToScreen($Window, [ref]$origin)) { throw 'Cannot map the client area' }
    $scale = [FlotillaSmokeWindow]::GetDpiForWindow($Window) / 96.0
    $x = if ($FromLeft -ge 0) { $origin.X + [int]($FromLeft * $scale) } else { $origin.X + $client.Right - [int]($FromRight * $scale) }
    $point = New-Object FlotillaSmokeWindow+Point
    $point.X = $x
    $point.Y = $origin.Y + [int](18 * $scale)
    return $point
}

function Click-At($Point) {
    [void][FlotillaSmokeWindow]::SetCursorPos($Point.X, $Point.Y)
    Start-Sleep -Milliseconds 150
    [FlotillaSmokeWindow]::mouse_event(0x0002, 0, 0, 0, [UIntPtr]::Zero)
    Start-Sleep -Milliseconds 80
    [FlotillaSmokeWindow]::mouse_event(0x0004, 0, 0, 0, [UIntPtr]::Zero)
}

function Wait-For([scriptblock]$Condition, [string]$What) {
    $until = [DateTime]::UtcNow.AddSeconds(5)
    while (-not (& $Condition)) {
        if ([DateTime]::UtcNow -ge $until) { throw "Caption smoke: $What" }
        Start-Sleep -Milliseconds 100
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
    $title = $process.MainWindowTitle
    # The client-drawn caption buttons must drive the real window: restore,
    # maximize, drag, minimize and finally close the app.
    [void][FlotillaSmokeWindow]::SetForegroundWindow($window)
    Wait-For { [FlotillaSmokeWindow]::IsZoomed($window) } 'the window did not start maximized'
    Click-At (Get-TitlePoint $window 69)
    Wait-For { -not [FlotillaSmokeWindow]::IsZoomed($window) } 'Maximize/Restore did not restore'
    Start-Sleep -Milliseconds 500
    $before = New-Object FlotillaSmokeWindow+Rect
    [void][FlotillaSmokeWindow]::GetWindowRect($window, [ref]$before)
    $grab = Get-TitlePoint $window 0 200
    [void][FlotillaSmokeWindow]::SetCursorPos($grab.X, $grab.Y)
    Start-Sleep -Milliseconds 150
    [FlotillaSmokeWindow]::mouse_event(0x0002, 0, 0, 0, [UIntPtr]::Zero)
    for ($step = 1; $step -le 10; $step++) {
        Start-Sleep -Milliseconds 40
        [void][FlotillaSmokeWindow]::SetCursorPos($grab.X + 12 * $step, $grab.Y + 6 * $step)
    }
    Start-Sleep -Milliseconds 150
    [FlotillaSmokeWindow]::mouse_event(0x0004, 0, 0, 0, [UIntPtr]::Zero)
    Start-Sleep -Milliseconds 300
    $after = New-Object FlotillaSmokeWindow+Rect
    [void][FlotillaSmokeWindow]::GetWindowRect($window, [ref]$after)
    if (($after.Left - $before.Left) -lt 60 -or ($after.Top - $before.Top) -lt 30) {
        throw "Caption smoke: dragging the title bar did not move the window ($($before.Left),$($before.Top) -> $($after.Left),$($after.Top))"
    }
    Click-At (Get-TitlePoint $window 69)
    Wait-For { [FlotillaSmokeWindow]::IsZoomed($window) } 'Maximize did not maximize'
    Click-At (Get-TitlePoint $window 115)
    Wait-For { [FlotillaSmokeWindow]::IsIconic($window) } 'Minimize did not minimize'
    [void][FlotillaSmokeWindow]::ShowWindow($window, 9)
    [void][FlotillaSmokeWindow]::SetForegroundWindow($window)
    Wait-For { -not [FlotillaSmokeWindow]::IsIconic($window) } 'the window did not come back'
    Start-Sleep -Milliseconds 500
    Click-At (Get-TitlePoint $window 23)
    if (-not $process.WaitForExit(15000)) { throw 'Caption smoke: Close did not exit the app' }

    @{
        window_title = $title
        workspace_sha256 = $initial
        add_host_sha256 = $addHost
        draft_preserved = $true
        caption_buttons = 'restore, drag, maximize, minimize, close'
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
