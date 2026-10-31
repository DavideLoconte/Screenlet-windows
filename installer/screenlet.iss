; Screenlet's installer: iscc installer\screenlet.iss, from the published files in ..\dist
#ifndef Version
  #define Version "0.1.0"
#endif

[Setup]
AppName=Screenlet
AppVersion={#Version}
AppPublisher=Davide Loconte
DefaultDirName={autopf}\Screenlet
DefaultGroupName=Screenlet
DisableProgramGroupPage=yes
OutputDir=..\dist
OutputBaseFilename=Screenlet-{#Version}-setup
SetupIconFile=..\data\screenlet.ico
UninstallDisplayIcon={app}\screenlet.exe
Compression=lzma2/ultra64
SolidCompression=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
PrivilegesRequired=admin
CloseApplications=yes

[Files]
Source: "..\dist\screenlet.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\dist\window\*"; DestDir: "{app}\window"; Flags: ignoreversion recursesubdirs
Source: "..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\driver\LICENSE"; DestDir: "{app}\driver"; Flags: ignoreversion

[Icons]
Name: "{group}\Screenlet"; Filename: "{app}\screenlet.exe"

[Run]
; Extend's monitor: the signed driver, which screenlet.exe carries.
Filename: "{app}\screenlet.exe"; Parameters: "--install-driver"; Flags: runhidden
; The cable: Microsoft's WinUSB for the tablet as an accessory.
Filename: "{app}\screenlet.exe"; Parameters: "--install-cable"; Flags: runhidden
; To hear the tablets' announcements (mDNS) and their packets on any network.
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall add rule name=""Screenlet"" dir=in action=allow program=""{app}\screenlet.exe"" enable=yes"; Flags: runhidden
Filename: "{app}\screenlet.exe"; Description: "Open Screenlet"; Flags: postinstall nowait skipifsilent runasoriginaluser

[UninstallRun]
Filename: "{sys}\taskkill.exe"; Parameters: "/f /im screenlet.exe"; Flags: runhidden; RunOnceId: "stop"
Filename: "{app}\screenlet.exe"; Parameters: "--uninstall-driver"; Flags: runhidden; RunOnceId: "driver"
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall delete rule name=""Screenlet"""; Flags: runhidden; RunOnceId: "firewall"
