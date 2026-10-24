; Inno Setup script for Lemma's Windows installer; packaging/windows/build.sh compiles it:
;   ISCC /DVersion=X /DSourceDir=dist\Lemma /DOutputDir=dist /DOutputName=NAME
;        /DIconFile=lemma.ico /DLicenseFile=LICENSE lemma.iss
;
; Installs for the current user by default (no administrator rights needed), or for everyone.
; Optional tasks: a desktop icon, opening .tex and .bib files with Lemma, and Lemma's bin folder
; on PATH (for `lemma` in a terminal). Settings in %APPDATA%\lemma stay when Lemma is removed.

#ifndef Version
  #error Pass /DVersion=...
#endif

[Setup]
AppId={{6F1C2B9E-3D4A-4C7E-9A51-8E2B7D0C4F13}
AppName=Lemma
AppVersion={#Version}
AppVerName=Lemma {#Version}
AppPublisher=Lemma
AppPublisherURL=https://github.com/dharmavati213/lemma
AppSupportURL=https://github.com/dharmavati213/lemma/issues
AppUpdatesURL=https://github.com/dharmavati213/lemma/releases
DefaultDirName={autopf}\Lemma
DefaultGroupName=Lemma
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog commandline
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; Windows 10 1903: the UTF-8 code page in Lemma's manifest.
MinVersion=10.0.18362
LicenseFile={#LicenseFile}
SetupIconFile={#IconFile}
UninstallDisplayIcon={app}\bin\lemma.exe
UninstallDisplayName=Lemma
OutputDir={#OutputDir}
OutputBaseFilename={#OutputName}
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ChangesAssociations=yes
ChangesEnvironment=yes
CloseApplications=yes

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked
Name: "associate"; Description: "Open .tex and .bib files with Lemma"; GroupDescription: "Files:"
Name: "addtopath"; Description: "Add Lemma to PATH (the lemma command in a terminal)"; GroupDescription: "Terminal:"; Flags: unchecked

[Files]
Source: "{#SourceDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{autoprograms}\Lemma"; Filename: "{app}\bin\lemma.exe"; Comment: "LaTeX editor with review of AI agent edits"
Name: "{autodesktop}\Lemma"; Filename: "{app}\bin\lemma.exe"; Tasks: desktopicon

[Registry]
; The file type, and Lemma under "Open with" for .tex and .bib (HKA: the current user's or
; everyone's classes, as the install mode is). Windows lets only the user choose the default
; program (Open with > Always).
Root: HKA; Subkey: "Software\Classes\Lemma.Document"; ValueType: string; ValueName: ""; ValueData: "LaTeX document"; Flags: uninsdeletekey; Tasks: associate
Root: HKA; Subkey: "Software\Classes\Lemma.Document\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: """{app}\bin\lemma.exe"",0"; Tasks: associate
Root: HKA; Subkey: "Software\Classes\Lemma.Document\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\bin\lemma.exe"" ""%1"""; Tasks: associate
Root: HKA; Subkey: "Software\Classes\.tex\OpenWithProgids"; ValueType: string; ValueName: "Lemma.Document"; ValueData: ""; Flags: uninsdeletevalue; Tasks: associate
Root: HKA; Subkey: "Software\Classes\.bib\OpenWithProgids"; ValueType: string; ValueName: "Lemma.Document"; ValueData: ""; Flags: uninsdeletevalue; Tasks: associate
Root: HKA; Subkey: "Software\Classes\Applications\lemma.exe\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\bin\lemma.exe"" ""%1"""; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\Applications\lemma.exe\SupportedTypes"; ValueType: string; ValueName: ".tex"; ValueData: ""
Root: HKA; Subkey: "Software\Classes\Applications\lemma.exe\SupportedTypes"; ValueType: string; ValueName: ".bib"; ValueData: ""

[Run]
Filename: "{app}\bin\lemma.exe"; Description: "{cm:LaunchProgram,Lemma}"; Flags: nowait postinstall skipifsilent

[Code]
const
  UserEnvironment = 'Environment';
  SystemEnvironment = 'SYSTEM\CurrentControlSet\Control\Session Manager\Environment';

{ The registry root and key of PATH for this install mode: the user's or everyone's. }
procedure PathKey(var Root: Integer; var Key: String);
begin
  if IsAdminInstallMode then
  begin
    Root := HKEY_LOCAL_MACHINE;
    Key := SystemEnvironment;
  end
  else
  begin
    Root := HKEY_CURRENT_USER;
    Key := UserEnvironment;
  end;
end;

{ Whether the ;-separated list Paths holds Dir (ignoring case and a trailing backslash). }
function PathHas(Paths, Dir: String): Boolean;
begin
  Result := Pos(';' + Uppercase(RemoveBackslashUnlessRoot(Dir)) + ';',
    ';' + Uppercase(Paths) + ';') > 0;
end;

procedure AddToPath(Dir: String);
var
  Root: Integer;
  Key, Paths: String;
begin
  PathKey(Root, Key);
  if not RegQueryStringValue(Root, Key, 'Path', Paths) then
    Paths := '';
  if PathHas(Paths, Dir) then
    exit;
  if (Paths <> '') and (Copy(Paths, Length(Paths), 1) <> ';') then
    Paths := Paths + ';';
  RegWriteExpandStringValue(Root, Key, 'Path', Paths + Dir);
end;

procedure RemoveFromPath(Dir: String);
var
  Root: Integer;
  Key, Paths, Item, Kept: String;
  Sep: Integer;
begin
  PathKey(Root, Key);
  if not RegQueryStringValue(Root, Key, 'Path', Paths) then
    exit;
  Kept := '';
  while Paths <> '' do
  begin
    Sep := Pos(';', Paths);
    if Sep = 0 then
    begin
      Item := Paths;
      Paths := '';
    end
    else
    begin
      Item := Copy(Paths, 1, Sep - 1);
      Delete(Paths, 1, Sep);
    end;
    if (Item <> '') and not PathHas(Item, Dir) then
    begin
      if Kept <> '' then
        Kept := Kept + ';';
      Kept := Kept + Item;
    end;
  end;
  if Kept = '' then
    RegDeleteValue(Root, Key, 'Path')
  else
    RegWriteExpandStringValue(Root, Key, 'Path', Kept);
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if (CurStep = ssPostInstall) and WizardIsTaskSelected('addtopath') then
    AddToPath(ExpandConstant('{app}\bin'));
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
    RemoveFromPath(ExpandConstant('{app}\bin'));
end;
