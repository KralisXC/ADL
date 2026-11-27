# ADL 下载器 —— 一键打包安装程序
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\build-installer.ps1
#
# 依次做四件事：
#   1. cargo build --release（adl-gui + adl 命令行）
#   2. 把 exe / 插件 / 浏览器扩展 / LICENSE / README 拷到 dist\stage\
#   3. 用 scripts\sign.ps1 给两个 exe 签名（没有证书就跳过并警告）
#   4. 调 Inno Setup 的 ISCC.exe 生成 dist\ADL-Setup-<版本>.exe
#
# 为什么打包用白名单而不是把 target\release 整个塞进去：
# target 里有几万个中间文件、几 GB 的 deps，而且混着 .pdb 和旧的
# 测试产物。安装包里只该有用户真正需要的东西。

param(
    # 跳过编译，直接用现有 exe 打包（调试安装脚本时省时间）
    [switch]$NoBuild,
    # 跳过签名
    [switch]$NoSign,
    # 同时把生成的安装程序也签上名
    [switch]$SignSetup,
    # Inno Setup 的 ISCC.exe 路径
    [string]$Iscc = 'D:\Inno Setup 7\ISCC.exe',
    [string]$TargetDir = 'E:\ADL\target'
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

function Step($text) { Write-Host ""; Write-Host "=== $text ===" -ForegroundColor Cyan }
function Fail($text) { Write-Host "错误：$text" -ForegroundColor Red; exit 1 }

# ── 0. 版本号 ────────────────────────────────────────────────────────
# 从工作区 Cargo.toml 里抠 version（不引额外的 TOML 解析器：只认第一处
# `version = "..."` 就够了，workspace 里那是 [workspace.package] 的版本）。
$cargoToml = Get-Content (Join-Path $root 'Cargo.toml') -Raw -Encoding UTF8
if ($cargoToml -notmatch '(?m)^version\s*=\s*"([^"]+)"') { Fail 'Cargo.toml 里找不到 version' }
$version = $Matches[1]
Write-Host "ADL 版本：$version"

$issPath = Join-Path $root 'installer\adl.iss'
if (-not (Test-Path $issPath)) { Fail "找不到安装脚本：$issPath" }
# .iss 里的 MyAppVersion 是硬编码的，和 Cargo.toml 对不上就是有人忘了改。
$issText = Get-Content $issPath -Raw -Encoding UTF8
if ($issText -notmatch '#define MyAppVersion "([^"]+)"') { Fail '.iss 里找不到 MyAppVersion' }
if ($Matches[1] -ne $version) {
    Fail ".iss 的 MyAppVersion 是 $($Matches[1])，Cargo.toml 是 $version —— 先改成一致"
}

# ── 1. 编译 ─────────────────────────────────────────────────────────
if (-not $NoBuild) {
    Step '编译 release'
    # 发行包要 LTO + strip（Cargo.toml 里已经配好），所以这里不再覆盖
    # CARGO_PROFILE_* —— 开发时用 probe\eguiprobe\target 那套快速配置，
    # 出安装包用默认的 E:\ADL\target。
    $env:CARGO_TARGET_DIR = $TargetDir
    & cargo build --release -p adl-gui -p adl-app 2>&1 | ForEach-Object { Write-Host $_ }
    if ($LASTEXITCODE -ne 0) { Fail "cargo build 失败（exit $LASTEXITCODE）" }
}

$rel = Join-Path $TargetDir 'release'
foreach ($f in 'adl-gui.exe', 'adl.exe') {
    $p = Join-Path $rel $f
    if (-not (Test-Path $p)) { Fail "缺少编译产物：$p" }
}

# ── 2. 暂存 ─────────────────────────────────────────────────────────
Step '准备暂存目录'
$stage = Join-Path $root 'dist\stage'
if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
New-Item -ItemType Directory -Force -Path $stage | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $stage 'plugins') | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $stage 'browser-extension') | Out-Null

Copy-Item (Join-Path $rel 'adl-gui.exe') $stage -Force
Copy-Item (Join-Path $rel 'adl.exe') $stage -Force
Copy-Item (Join-Path $root 'LICENSE') $stage -Force
Copy-Item (Join-Path $root 'README.md') $stage -Force

# 插件：整目录拷，但跳过 .git / 笔记之类（用户装的插件不在仓库里，
# 这里拷的是随程序分发的六个内置/示例插件）。
Get-ChildItem (Join-Path $root 'plugins') -Directory | ForEach-Object {
    Copy-Item $_.FullName (Join-Path $stage 'plugins') -Recurse -Force
}
Copy-Item (Join-Path $root 'browser-extension\*') (Join-Path $stage 'browser-extension') -Recurse -Force

# 打包前把不该发的东西清掉：调试符号、备份、编辑器临时文件。
Get-ChildItem $stage -Recurse -File -Include *.pdb, *.bak, *.tmp, *.old, .DS_Store |
    Remove-Item -Force -ErrorAction SilentlyContinue

$files = Get-ChildItem $stage -Recurse -File
$totalMb = ($files | Measure-Object -Sum Length).Sum / 1MB
Write-Host ("暂存完成：{0} 个文件，{1:N1} MB" -f $files.Count, $totalMb)
Write-Host ("  adl-gui.exe  {0:N1} MB" -f ((Get-Item (Join-Path $stage 'adl-gui.exe')).Length / 1MB))
Write-Host ("  adl.exe      {0:N1} MB" -f ((Get-Item (Join-Path $stage 'adl.exe')).Length / 1MB))
Write-Host ("  plugins      {0:N1} MB（大部分是 media-converter 自带的 ffmpeg）" -f ((Get-ChildItem (Join-Path $stage 'plugins') -Recurse -File | Measure-Object -Sum Length).Sum / 1MB))

# ── 3. 签名 ─────────────────────────────────────────────────────────
if (-not $NoSign) {
    Step '签名'
    $signScript = Join-Path $root 'scripts\sign.ps1'
    foreach ($f in 'adl-gui.exe', 'adl.exe') {
        $p = Join-Path $stage $f
        Write-Host "-- $f"
        & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $signScript -Path $p
        if ($LASTEXITCODE -ne 0) {
            # 自签名证书没装、时间戳服务器抖动都会走到这儿。不中止：
            # 没有签名的安装包照样能装，只是 SmartScreen 会警告。
            Write-Host "   （签名失败，继续打包 —— 会是没有签名的版本）" -ForegroundColor Yellow
        }
    }
}

# ── 4. 打安装包 ─────────────────────────────────────────────────────
Step '调用 Inno Setup'
if (-not (Test-Path $Iscc)) { Fail "找不到 ISCC.exe：$Iscc（用 -Iscc 参数指定）" }
Write-Host "ISCC：$Iscc"
& $Iscc (Join-Path $root 'installer\adl.iss') 2>&1 | ForEach-Object { Write-Host $_ }
if ($LASTEXITCODE -ne 0) { Fail "ISCC 失败（exit $LASTEXITCODE）" }

$setup = Join-Path $root "dist\ADL-Setup-$version.exe"
if (-not (Test-Path $setup)) { Fail "没生成 $setup" }

if ($SignSetup) {
    Step '签名安装程序'
    & powershell.exe -NoProfile -ExecutionPolicy Bypass -File (Join-Path $root 'scripts\sign.ps1') -Path $setup
}

$fi = Get-Item $setup
$hash = (Get-FileHash $setup -Algorithm SHA256).Hash.ToLower()
Write-Host ""
Write-Host "完成：" -ForegroundColor Green
Write-Host ("  {0}" -f $fi.FullName)
Write-Host ("  {0:N1} MB" -f ($fi.Length / 1MB))
Write-Host ("  sha256 {0}" -f $hash)
$sig = Get-AuthenticodeSignature $setup
Write-Host ("  签名 {0}" -f $sig.Status)
