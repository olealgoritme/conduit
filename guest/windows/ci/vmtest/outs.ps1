Add-Type -TypeDefinition @'
using System; using System.Runtime.InteropServices; using System.Text;
public static class D {
 [StructLayout(LayoutKind.Sequential, CharSet=CharSet.Unicode)] public struct DD { public int cb; [MarshalAs(UnmanagedType.ByValTStr,SizeConst=32)] public string Name; [MarshalAs(UnmanagedType.ByValTStr,SizeConst=128)] public string Str; public int Flags; [MarshalAs(UnmanagedType.ByValTStr,SizeConst=128)] public string Id; [MarshalAs(UnmanagedType.ByValTStr,SizeConst=128)] public string Key; }
 [DllImport("user32.dll",CharSet=CharSet.Unicode)] public static extern bool EnumDisplayDevices(string dev, int i, ref DD dd, int flags);
 [StructLayout(LayoutKind.Sequential, CharSet=CharSet.Unicode)] public struct ADesc { [MarshalAs(UnmanagedType.ByValTStr,SizeConst=128)] public string Desc; public int Ven,Dev,Sub,Rev; public IntPtr V,S,Sh; public uint LuidLo; public int LuidHi; public int Flags; }
 [StructLayout(LayoutKind.Sequential, CharSet=CharSet.Unicode)] public struct ODesc { [MarshalAs(UnmanagedType.ByValTStr,SizeConst=32)] public string Name; public int L,T,R,B; public int Attached; public int Rot; public IntPtr Mon; }
 [ComImport, Guid("770aae78-f26f-4dba-a829-253c83d1b387"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)] public interface IF1 { void a(); void b(); void c(); void d(); void e(); void f(); void g(); void h(); void i(); [PreserveSig] int EnumAdapters1(uint i, out IA1 a); }
 [ComImport, Guid("29038f61-3839-4626-91fd-086879011a05"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)] public interface IA1 { void a(); void b(); void c(); void d(); [PreserveSig] int EnumOutputs(uint i, out IO o); void e(); void f(); [PreserveSig] int GetDesc1(out ADesc d); }
 [ComImport, Guid("ae02eedb-c735-4690-8d52-5a8dc20213aa"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)] public interface IO { void a(); void b(); void c(); void d(); [PreserveSig] int GetDesc(out ODesc d); [PreserveSig] int GetDisplayModeList(int fmt, uint flags, ref uint n, IntPtr modes); void fc(); [PreserveSig] int WaitForVBlank(); }
 [DllImport("dxgi.dll")] public static extern int CreateDXGIFactory1(ref Guid g, out IF1 f);
 public static string Run() { var sb=new StringBuilder(); Guid g=typeof(IF1).GUID; IF1 f; int hr=CreateDXGIFactory1(ref g,out f); sb.AppendLine("factory hr=0x"+hr.ToString("x"));
  for(uint i=0;;i++){ IA1 a; if(f.EnumAdapters1(i,out a)!=0) break; ADesc ad; a.GetDesc1(out ad); sb.AppendLine("adapter "+i+": "+ad.Desc+" luid="+ad.LuidHi+":"+ad.LuidLo+" flags="+ad.Flags);
   for(uint j=0;;j++){ IO o; int h=a.EnumOutputs(j,out o); if(h!=0){ sb.AppendLine("  outputs end hr=0x"+h.ToString("x")); break;} ODesc od; o.GetDesc(out od); sb.AppendLine("  output "+j+": "+od.Name+" attached="+od.Attached+" rect="+od.L+","+od.T+","+od.R+","+od.B); uint n=0; int mh=o.GetDisplayModeList(87,0,ref n,IntPtr.Zero); sb.AppendLine("    GetDisplayModeList(BGRA) hr=0x"+mh.ToString("x")+" count="+n); int vh=o.WaitForVBlank(); sb.AppendLine("    WaitForVBlank hr=0x"+vh.ToString("x")); } }
  for(int i=0;;i++){ DD d=new DD(); d.cb=Marshal.SizeOf(d); if(!EnumDisplayDevices(null,i,ref d,0)) break; sb.AppendLine("GDI "+d.Name+" | "+d.Str+" | flags=0x"+d.Flags.ToString("x")+" | "+d.Id); DD m=new DD(); m.cb=Marshal.SizeOf(m); if(EnumDisplayDevices(d.Name,0,ref m,0)) sb.AppendLine("   monitor: "+m.Str+" flags=0x"+m.Flags.ToString("x")+" "+m.Id); }
  return sb.ToString(); }
}
'@
$o = @(); $o += [D]::Run()
Add-Type -AssemblyName System.Windows.Forms; $o += [Windows.Forms.Screen]::AllScreens | % { "Screen $($_.DeviceName) primary=$($_.Primary) $($_.Bounds)" }
$o += Get-CimInstance -Namespace root\wmi -Class WmiMonitorBasicDisplayParams -EA 0 | % { "WmiMonitor $($_.InstanceName) active=$($_.Active)" }
$o += gcim Win32_VideoController | % { "$($_.Name) | $($_.PNPDeviceID) | $($_.CurrentHorizontalResolution)" }

$o | Out-File -Encoding ascii C:\Users\Public\t\outs.txt
