param([string]$Out = 'C:\Users\Public\t\shot.png')
Add-Type -AssemblyName System.Windows.Forms, System.Drawing
Add-Type -Name W -Namespace C -MemberDefinition '[DllImport("kernel32.dll")] public static extern System.IntPtr GetConsoleWindow(); [DllImport("user32.dll")] public static extern bool ShowWindow(System.IntPtr h, int c);'
[C.W]::ShowWindow([C.W]::GetConsoleWindow(), 0) | Out-Null; Start-Sleep -Milliseconds 700
$b = [Windows.Forms.Screen]::PrimaryScreen.Bounds
$bmp = New-Object Drawing.Bitmap $b.Width, $b.Height
$g = [Drawing.Graphics]::FromImage($bmp); $g.CopyFromScreen($b.Location, [Drawing.Point]::Empty, $b.Size)
$bmp.Save($Out, [Drawing.Imaging.ImageFormat]::Png); "saved $Out $($b.Width)x$($b.Height)"
