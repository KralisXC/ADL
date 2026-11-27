; ─────────────────────────────────────────────────────────────────────
;  ADL 下载器 —— Inno Setup 安装脚本
;
;  用 scripts\build-installer.ps1 来调它（脚本会先编译、暂存、签名，
;  再执行 ISCC）。直接双击本文件会因为没有暂存目录而失败。
;
;  安装范围默认是「当前用户」（{localappdata}\Programs\ADL）：
;    · 全程不需要 UAC —— 用户 m22554 的想法是「安装时提权一次」，
;      但其实按用户安装连那一次都不用；
;    · 关键原因是 data\ 目录：程序把配置、任务表、日志、缓存全放在
;      exe 同级的 data\ 里（用户 m21598）。装到 Program Files 的话
;      普通用户写不进去，会退回到 %LOCALAPPDATA%\ADL，等于把用户
;      刚要求的「数据放自己文件夹」又破坏掉了。
;    · 想要机器级安装就把下面的 InstallScope 改成 "machine"。
; ─────────────────────────────────────────────────────────────────────

#define MyAppName "ADL 下载器"
#define MyAppVersion "0.1.0"
#define MyAppPublisher "KralisXC"
#define MyAppURL "https://github.com/KralisXC/ADL"
#define MyAppExeName "adl-gui.exe"

; 与 platform.rs 里的 `Global\ADL_SingleInstance_v1` 对齐：ADL 正在跑时
; 安装程序会提示先关掉，避免覆盖正在使用的 exe。
#define MyAppMutex "Global\ADL_SingleInstance_v1"

; 固定 AppId：升级安装会沿用同一个 GUID，控制面板里不会出现两条 ADL。
#define MyAppId "{{8E4C1F5A-2B7D-4C9E-9A31-6D2F7B8E4C10}"

; ── Edge 扩展 ────────────────────────────────────────────────────────
; 商店里的 CRX ID（Edge 合作伙伴中心分配的，不是本地算出来的那个）。
#define EdgeExtId "ojmgmjjeikafpmldfmlebakickaknmoe"
; 还没在商店发布时把它设成 0：安装页里的「强制安装扩展」选项会隐藏，
; 因为指向商店的强制安装策略在未发布状态下只会让 Edge 报错。
#define EdgeExtPublished 0

#define InstallScope "user"

#define StagingDir "..\dist\stage"

[Setup]
AppId={#MyAppId}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppVerName={#MyAppName} {#MyAppVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppURL}
AppSupportURL={#MyAppURL}/issues
AppUpdatesURL={#MyAppURL}/releases
VersionInfoVersion={#MyAppVersion}
VersionInfoCompany={#MyAppPublisher}
VersionInfoDescription={#MyAppName} 安装程序
VersionInfoProductName={#MyAppName}
VersionInfoProductVersion={#MyAppVersion}

#if InstallScope == "machine"
PrivilegesRequired=admin
DefaultDirName={autopf}\ADL
DefaultGroupName=ADL
#else
PrivilegesRequired=lowest
DefaultDirName={localappdata}\Programs\ADL
DefaultGroupName=ADL
#endif

; ADL 的窗口是无边框自绘的，装完直接启动它比让人回头找快捷方式顺手。
DisableProgramGroupPage=yes
DisableWelcomePage=no
AllowNoIcons=yes
OutputDir=..\dist
OutputBaseFilename=ADL-Setup-{#MyAppVersion}
SetupIconFile=..\crates\adl-gui\assets\adl.ico
UninstallDisplayIcon={app}\{#MyAppExeName}
UninstallDisplayName={#MyAppName}
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
AppMutex={#MyAppMutex}
CloseApplications=yes
RestartApplications=no

[Languages]
Name: "chinesesimplified"; MessagesFile: "compiler:Languages\ChineseSimplified.isl"
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"
#if EdgeExtPublished
Name: "edgeext"; Description: "在 Edge 里自动安装「ADL 下载桥」扩展（强制安装，不会被用户误删）"; GroupDescription: "附加任务："; Flags: checkedonce
#endif

[Files]
Source: "{#StagingDir}\adl-gui.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StagingDir}\adl.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StagingDir}\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StagingDir}\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StagingDir}\plugins\*"; DestDir: "{app}\plugins"; Flags: ignoreversion recursesubdirs createallsubdirs
Source: "{#StagingDir}\browser-extension\*"; DestDir: "{app}\browser-extension"; Flags: ignoreversion recursesubdirs createallsubdirs
; 开发期残留（.pdb / 别的东西）一律不打包 —— 暂存目录是白名单制的，
; 见 build-installer.ps1。

[Icons]
Name: "{autoprograms}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon

[Registry]
; ── Edge 扩展强制安装 ────────────────────────────────────────────────
; 用 ExtensionSettings（按扩展 ID 做子键）而不是 ExtensionInstallForcelist：
; 后者靠 "1"/"2"/"3" 这类序号定位，机器上已经装过别的强制扩展时
; 很容易把别人的项覆盖掉。ExtensionSettings 按 ID 索引，不冲突。
;
; 写 HKCU 而不是 HKLM：Edge 两个位置都读，HKCU 下不需要管理员权限，
; 正好配合「按用户安装」—— 整个安装过程一次 UAC 都不弹。
;
; 两行都挂在 #if EdgeExtPublished 里：扩展还没上架时指向商店的强制
; 安装只会让 Edge 报错（ID 查不到），所以整个块一起开关，不能只关 [Tasks]。
#if EdgeExtPublished
Root: HKCU; Subkey: "Software\Policies\Microsoft\Edge\ExtensionSettings\{#EdgeExtId}"; ValueType: string; ValueName: "installation_mode"; ValueData: "force_installed"; Tasks: edgeext; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Policies\Microsoft\Edge\ExtensionSettings\{#EdgeExtId}"; ValueType: string; ValueName: "update_url"; ValueData: "https://edge.microsoft.com/extensionwebstorebase/v1/crx"; Tasks: edgeext
#endif

; ── 协议 ─────────────────────────────────────────────────────────────
; adl:// 由程序自己首次运行时注册（platform::register_url_scheme），
; 这里也写一份，保证「装完立刻能从浏览器拉起」而不必先手动开一次。
Root: HKCU; Subkey: "Software\Classes\adl"; ValueType: string; ValueName: ""; ValueData: "URL:ADL Download"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\adl"; ValueType: string; ValueName: "URL Protocol"; ValueData: ""
Root: HKCU; Subkey: "Software\Classes\adl\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\{#MyAppExeName},0"
Root: HKCU; Subkey: "Software\Classes\adl\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#MyAppExeName}"" --from-browser ""%1"""

; ── 卸载时清干净自己加的东西 ─────────────────────────────────────────
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: none; ValueName: "ADL"; Flags: uninsdeletevalue

[Run]
Filename: "{app}\{#MyAppExeName}"; Description: "{cm:LaunchProgram,{#MyAppName}}"; Flags: nowait postinstall skipifsilent

[UninstallRun]
; 卸载前先把还在跑的 ADL 关掉，否则 exe 被占用删不掉。
Filename: "{cmd}"; Parameters: "/C taskkill /IM {#MyAppExeName} /F >nul 2>&1"; Flags: runhidden waituntilterminated; RunOnceId: "KillAdl"

[Code]
// 装之前检查一下目标目录里有没有正在跑的 ADL —— AppMutex 已经能拦，
// 但 AppMutex 只认「本程序的互斥量」，如果有人把 exe 换个名字跑就漏了。
// 这里再按进程名兜一层，避免覆盖正在使用的二进制。
function InitializeSetup(): Boolean;
var
  ResultCode: Integer;
begin
  Result := True;
  if Exec(ExpandConstant('{cmd}'), '/C tasklist /FI "IMAGENAME eq adl-gui.exe" /NH | find /I "adl-gui.exe" >nul 2>&1',
          '', SW_HIDE, ewWaitUntilTerminated, ResultCode) then
  begin
    if ResultCode = 0 then
    begin
      if MsgBox('ADL 正在运行。安装程序需要先把它关掉，继续吗？', mbConfirmation, MB_YESNO) = IDYES then
        Exec(ExpandConstant('{cmd}'), '/C taskkill /IM adl-gui.exe /F >nul 2>&1', '', SW_HIDE, ewWaitUntilTerminated, ResultCode)
      else
        Result := False;
    end;
  end;
end;

// 卸载完提示一下 data\ 还在（用户数据，我们故意不删）。
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
  begin
    if DirExists(ExpandConstant('{app}\data')) then
      MsgBox('你的配置、任务列表和日志还留在：' + #13#10 +
             ExpandConstant('{app}\data') + #13#10#13#10 +
             '这是有意保留的 —— 重装后能接着用。不需要的话直接删掉这个目录即可。',
             mbInformation, MB_OK);
  end;
end;
