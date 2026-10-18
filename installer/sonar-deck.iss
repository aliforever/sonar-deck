; Inno Setup script for Sonar Deck. Built by .github/workflows/release.yml:
;   iscc /DAppVersion=0.1.0 /DSourceExe=..\target\release\sonar-deck.exe installer\sonar-deck.iss

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef SourceExe
  #define SourceExe "..\target\release\sonar-deck.exe"
#endif

[Setup]
AppId={{6F1E2B6C-4E7A-4E0B-9C51-5D3B7A0E9A21}
AppName=Sonar Deck
AppVersion={#AppVersion}
AppPublisher=aliforever
AppPublisherURL=https://github.com/aliforever/sonar-deck
AppSupportURL=https://github.com/aliforever/sonar-deck/issues
AppUpdatesURL=https://github.com/aliforever/sonar-deck/releases
DefaultDirName={localappdata}\Programs\Sonar Deck
DefaultGroupName=Sonar Deck
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
OutputDir=..\dist
OutputBaseFilename=SonarDeck-Setup-{#AppVersion}
SetupIconFile=..\assets\icon.ico
UninstallDisplayIcon={app}\sonar-deck.exe
LicenseFile=..\LICENSE
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
CloseApplications=yes

[Tasks]
Name: "autostart"; Description: "Start Sonar Deck when I sign in (in the tray)"
Name: "desktopicon"; Description: "Create a desktop shortcut"; Flags: unchecked

[Files]
Source: "{#SourceExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"
Source: "..\README.md"; DestDir: "{app}"

[Icons]
Name: "{group}\Sonar Deck"; Filename: "{app}\sonar-deck.exe"
; Ctrl+Alt+F12 toggles headset/speakers system-wide, even when Sonar Deck isn't running.
Name: "{group}\Sonar Toggle"; Filename: "{app}\sonar-deck.exe"; Parameters: "toggle"; HotKey: ctrl+alt+f12; Comment: "Toggle Sonar Deck output: headset / speakers"
Name: "{autodesktop}\Sonar Deck"; Filename: "{app}\sonar-deck.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\sonar-deck.exe"; Parameters: "login on"; Flags: runhidden; Tasks: autostart
Filename: "{app}\sonar-deck.exe"; Description: "Launch Sonar Deck"; Flags: nowait postinstall skipifsilent

[UninstallRun]
Filename: "{cmd}"; Parameters: "/C taskkill /IM sonar-deck.exe /F"; Flags: runhidden; RunOnceId: "StopApp"
Filename: "{app}\sonar-deck.exe"; Parameters: "uninstall"; Flags: runhidden; RunOnceId: "CleanUp"
