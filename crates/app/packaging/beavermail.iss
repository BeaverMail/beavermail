; SPDX-License-Identifier: AGPL-3.0-only

; The Windows installer, made by Inno Setup in the Windows workflow:
;   iscc /DVersion=0.1.0 /DBinary=path\to\beavermail.exe beavermail.iss
; It installs for everyone, or only for the user who runs it without being an administrator.

[Setup]
AppId={{25001220-C445-420E-9DAC-CB04C67494DB}
AppName=BeaverMail
AppVersion={#Version}
AppPublisher=BeaverMail
DefaultDirName={autopf}\BeaverMail
DisableProgramGroupPage=yes
PrivilegesRequired=admin
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
UninstallDisplayIcon={app}\beavermail.exe
OutputBaseFilename=beavermail-setup-{#Version}
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
SetupIconFile=..\assets\beavermail.ico

[Tasks]
Name: desktopicon; Description: "{cm:CreateDesktopIcon}"; Flags: unchecked

[Files]
Source: "{#Binary}"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\BeaverMail"; Filename: "{app}\beavermail.exe"
Name: "{autodesktop}\BeaverMail"; Filename: "{app}\beavermail.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\beavermail.exe"; Description: "{cm:LaunchProgram,BeaverMail}"; Flags: nowait postinstall skipifsilent
