; Flotilla per-user installer (no admin rights required).
; Build: cargo build --release --target x86_64-pc-windows-msvc -p flotilla
; Package: ISCC /DAppVersion=0.1.0 /DSourceExe=<abs path>\flotilla.exe /O<outdir> flotilla.iss

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef SourceExe
  #define SourceExe "..\..\target\x86_64-pc-windows-msvc\release\flotilla.exe"
#endif

[Setup]
AppId={{6B1F3C2E-8F4A-4C1D-9E7B-2A5D0C9F1E33}
AppName=Flotilla
AppVersion={#AppVersion}
AppVerName=Flotilla {#AppVersion}
AppPublisher=Coaxys
DefaultDirName={localappdata}\Programs\Flotilla
DisableDirPage=yes
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
OutputBaseFilename=Flotilla-Setup-x64
Compression=lzma2/ultra64
SolidCompression=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
WizardStyle=modern
UninstallDisplayIcon={app}\flotilla.exe
UninstallDisplayName=Flotilla
CloseApplications=yes

[Tasks]
Name: "desktopicon"; Description: "Create a &desktop shortcut"; Flags: unchecked

[Files]
Source: "{#SourceExe}"; DestDir: "{app}"; DestName: "flotilla.exe"; Flags: ignoreversion
Source: "..\..\crates\client\flc-ui\NOTICE"; DestDir: "{app}\licenses"; DestName: "NOTICE.txt"; Flags: ignoreversion
Source: "..\..\crates\client\flc-ui\assets\fonts\*.txt"; DestDir: "{app}\licenses"; Flags: ignoreversion
Source: "Apache-2.0.txt"; DestDir: "{app}\licenses"; Flags: ignoreversion

[Icons]
Name: "{userprograms}\Flotilla"; Filename: "{app}\flotilla.exe"; AppUserModelID: "Coaxys.Flotilla"
Name: "{userdesktop}\Flotilla"; Filename: "{app}\flotilla.exe"; AppUserModelID: "Coaxys.Flotilla"; Tasks: desktopicon

[Run]
Filename: "{app}\flotilla.exe"; Description: "Launch Flotilla"; Flags: nowait postinstall skipifsilent
