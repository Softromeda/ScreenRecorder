; Softromeda Screen Recorder
; Copyright (C) 2026 Softromeda <https://softromeda.com>
; Originally created by Softromeda - https://softromeda.com/screen-recorder
; SPDX-License-Identifier: GPL-3.0-or-later
;
; This program is free software: you can redistribute it and/or modify it under the terms of
; the GNU General Public License as published by the Free Software Foundation, either version 3
; of the License, or (at your option) any later version, with the additional terms listed in
; the NOTICE file. It is distributed WITHOUT ANY WARRANTY; see the LICENSE and NOTICE files.

; Inno Setup script for Softromeda Screen Recorder (Inno Setup 6.3 or newer).
;
; Build the program first (cargo build --release), then compile this script:
;   iscc /DAppVersion=1.0.0 installer\SoftromedaScreenRecorder.iss
; or simply run installer\build.ps1, which does both and reads the version
; from Cargo.toml. The installer is written to installer\output\.

#ifndef AppVersion
  #define AppVersion "1.0.0"
#endif
#define AppName      "Softromeda Screen Recorder"
#define AppPublisher "Softromeda"
#define AppExe       "softromeda-screen-recorder.exe"
#define AppSite      "https://softromeda.com/screen-recorder"

[Setup]
; Never change the AppId: Windows uses it to find the installed program
; when updating or uninstalling.
AppId={{ACD06354-CE75-453B-9ADF-FF1FE8D98FB1}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher={#AppPublisher}
AppPublisherURL=https://softromeda.com
AppSupportURL={#AppSite}
AppUpdatesURL={#AppSite}
AppCopyright=© 2026 {#AppPublisher}
VersionInfoVersion={#AppVersion}
VersionInfoCompany={#AppPublisher}
VersionInfoDescription={#AppName} Setup

DefaultDirName={autopf}\Softromeda\Recorder
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
UninstallDisplayName={#AppName}
UninstallDisplayIcon={app}\{#AppExe}
LicenseFile=..\LICENSE

; Always install for all users (into Program Files); asks for admin rights.
PrivilegesRequired=admin

; 64-bit Windows 10 or 11 only.
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0

; Close a running copy before updating it.
CloseApplications=yes
RestartApplications=no

SetupIconFile=..\assets\icon.ico
WizardStyle=modern
Compression=lzma2/max
SolidCompression=yes
OutputDir=output
OutputBaseFilename=SoftromedaScreenRecorder-Setup-{#AppVersion}-x64
ShowLanguageDialog=yes

[Languages]
Name: "english";    MessagesFile: "compiler:Default.isl"
Name: "russian";    MessagesFile: "compiler:Languages\Russian.isl"
Name: "spanish";    MessagesFile: "compiler:Languages\Spanish.isl"
Name: "french";     MessagesFile: "compiler:Languages\French.isl"
Name: "portuguese"; MessagesFile: "compiler:Languages\Portuguese.isl"
Name: "brazilian";  MessagesFile: "compiler:Languages\BrazilianPortuguese.isl"
; Added in recent Inno Setup versions: used when available.
#if FileExists(CompilerPath + "Languages\Korean.isl")
  #define HaveKorean
Name: "korean";     MessagesFile: "compiler:Languages\Korean.isl"
#endif
#if FileExists(CompilerPath + "Languages\Arabic.isl")
  #define HaveArabic
Name: "arabic";     MessagesFile: "compiler:Languages\Arabic.isl"
#endif

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"

[Files]
Source: "..\target\release\{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\README.md";               DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE";                 DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion
Source: "..\NOTICE";                  DestDir: "{app}"; DestName: "NOTICE.txt"; Flags: ignoreversion

[InstallDelete]
; Shortcuts made under the program's former name, "Softromeda Recorder".
Type: files; Name: "{autoprograms}\Softromeda Recorder.lnk"
Type: files; Name: "{autodesktop}\Softromeda Recorder.lnk"
; The program file under its former name (CloseApplications also closes it if it is running).
Type: files; Name: "{app}\softromeda-recorder.exe"

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#AppExe}"
Name: "{autodesktop}\{#AppName}";  Filename: "{app}\{#AppExe}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#AppExe}"; Description: "{cm:LaunchProgram,{#StringChange(AppName, '&', '&&')}}"; Flags: nowait postinstall skipifsilent

[Code]
const
  RunKey = 'Software\Microsoft\Windows\CurrentVersion\Run';
  // Keeps the program's former name; see RUN_VALUE in src\tray.rs.
  RunValue = 'Softromeda Recorder';

// If "Start with Windows" is on, point it at the newly installed program file, which
// may have a different name than the one it was set up with.
procedure CurStepChanged(CurStep: TSetupStep);
begin
  if (CurStep = ssPostInstall) and RegValueExists(HKEY_CURRENT_USER, RunKey, RunValue) then
    RegWriteStringValue(HKEY_CURRENT_USER, RunKey, RunValue,
      '"' + ExpandConstant('{app}\{#AppExe}') + '" --tray');
end;

// Settings, logs and unfinished recordings are removed only if the user
// agrees; recordings are never touched.
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Roaming, Local: String;
begin
  if CurUninstallStep = usPostUninstall then
  begin
    // Remove "Start with Windows" (set from the program's options).
    RegDeleteValue(HKEY_CURRENT_USER, RunKey, RunValue);
    Roaming := ExpandConstant('{userappdata}\Softromeda\Recorder');
    Local := ExpandConstant('{localappdata}\Softromeda\Recorder');
    if (DirExists(Roaming) or DirExists(Local)) and not UninstallSilent then
      if MsgBox(ExpandConstant('{cm:RemoveSettings}'), mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES then
      begin
        DelTree(Roaming, True, True, True);
        DelTree(Local, True, True, True);
      end;
  end;
end;

[CustomMessages]
english.RemoveSettings=Also remove Softromeda Screen Recorder settings and logs?%n%nYour recordings are not affected.
russian.RemoveSettings=Удалить также настройки и журналы Softromeda Screen Recorder?%n%nВаши записи не будут затронуты.
spanish.RemoveSettings=¿Eliminar también la configuración y los registros de Softromeda Screen Recorder?%n%nTus grabaciones no se verán afectadas.
french.RemoveSettings=Supprimer aussi les paramètres et journaux de Softromeda Screen Recorder ?%n%nVos enregistrements ne sont pas concernés.
portuguese.RemoveSettings=Remover também as definições e registos do Softromeda Screen Recorder?%n%nAs suas gravações não são afetadas.
brazilian.RemoveSettings=Remover também as configurações e logs do Softromeda Screen Recorder?%n%nSuas gravações não serão afetadas.
#ifdef HaveKorean
korean.RemoveSettings=Softromeda Screen Recorder 설정과 로그도 삭제할까요?%n%n녹화 파일은 삭제되지 않습니다.
#endif
#ifdef HaveArabic
arabic.RemoveSettings=هل تريد أيضًا إزالة إعدادات وسجلات Softromeda Screen Recorder؟%n%nلن تتأثر تسجيلاتك.
#endif
