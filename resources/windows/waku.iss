; Waku's Windows installer.
;
; Per-user by design: %LOCALAPPDATA%\Programs needs no elevation.
; See docs/windows.md for the installer layout.
;
; Built by scripts/bundle-windows.ts, which supplies:
;   /DAppVersion=<version>  /DArch=<x86_64|aarch64>
;   /DStageDir=<dir with the built executables>  /DOutputDir=<dir>

#ifndef AppVersion
  #error AppVersion must be defined (ISCC /DAppVersion=...)
#endif
#ifndef Arch
  #error Arch must be defined (ISCC /DArch=...)
#endif
#ifndef StageDir
  #error StageDir must be defined (ISCC /DStageDir=...)
#endif
#ifndef OutputDir
  #define OutputDir "."
#endif

; An x64 build is worth allowing on Arm, where it runs emulated; an arm64
; build on x64 is not, so refuse it up front rather than installing something
; that cannot start.
#if Arch == "aarch64"
  #define Architectures "arm64"
#else
  #define Architectures "x64compatible"
#endif

[Setup]
; Never change AppId: it is how Windows and later installers recognize an
; existing install.
AppId={{8B6C6E4A-3E0F-4F0B-9C5F-2E0E9C4B7A11}
AppName=Waku
AppVersion={#AppVersion}
VersionInfoVersion={#AppVersion}
AppPublisher=Waku
DefaultDirName={autopf}\Waku
DefaultGroupName=Waku
UninstallDisplayName=Waku
UninstallDisplayIcon={app}\waku.exe
LicenseFile={#StageDir}\LICENSE
OutputDir={#OutputDir}
OutputBaseFilename=Waku-{#AppVersion}-{#Arch}-Setup
SetupIconFile=AppIcon.ico
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed={#Architectures}
ArchitecturesInstallIn64BitMode={#Architectures}
; What docs/windows.md promises. Enforcing it here beats installing onto a
; system that cannot run the result.
MinVersion=10.0.17763
; Two installers must not race while an install is already applying.
SetupMutex=WakuSetup
; No elevation, so an update never has to ask for it either.
PrivilegesRequired=lowest
DisableProgramGroupPage=yes
DisableReadyPage=yes
; A reinstall should land where the previous one did rather than asking again.
UsePreviousAppDir=yes
; Waku persists continuously to SQLite, so closing it is safe and a locked
; waku.exe does not make the installer duplicate the install.
CloseApplications=force
RestartApplications=no

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Files]
Source: "{#StageDir}\waku.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\waku-daemon.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\LICENSE"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\Waku"; Filename: "{app}\waku.exe"
Name: "{userdesktop}\Waku"; Filename: "{app}\waku.exe"; Tasks: desktopicon

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; Flags: unchecked

[Run]
Filename: "{app}\waku.exe"; Description: "{cm:LaunchProgram,Waku}"; Flags: nowait postinstall
