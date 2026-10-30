; Screenlet's installer: iscc installer\screenlet.iss, from the published files in ..\dist
[Setup]
AppName=Screenlet
AppVersion=0.1.0
AppPublisher=Davide Loconte
DefaultDirName={autopf}\Screenlet
DefaultGroupName=Screenlet
DisableProgramGroupPage=yes
OutputDir=..\dist
OutputBaseFilename=Screenlet-0.1.0-setup
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
Source: "..\driver\*"; DestDir: "{app}\driver"; Flags: ignoreversion

[Icons]
Name: "{group}\Screenlet"; Filename: "{app}\screenlet.exe"

[Run]
; Extend's monitor: the signed driver.
Filename: "{app}\screenlet.exe"; Parameters: "--install-driver"; Flags: runhidden
; To hear the tablets' announcements (mDNS) and their packets on any network.
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall add rule name=""Screenlet"" dir=in action=allow program=""{app}\screenlet.exe"" enable=yes"; Flags: runhidden
Filename: "{app}\screenlet.exe"; Description: "Open Screenlet"; Flags: postinstall nowait skipifsilent runasoriginaluser

[UninstallRun]
Filename: "{sys}\taskkill.exe"; Parameters: "/f /im screenlet.exe"; Flags: runhidden; RunOnceId: "stop"
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall delete rule name=""Screenlet"""; Flags: runhidden; RunOnceId: "firewall"
