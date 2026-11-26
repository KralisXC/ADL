<#
.SYNOPSIS
    给 ADL 的 exe 打自签名代码签名（含时间戳）。

.DESCRIPTION
    每次 `cargo build` 都会生成全新的 exe，之前的签名随之消失，所以这是
    一个"复制完 exe 之后顺手跑一下"的步骤，不是一次性操作。

    证书是自签的，只在本机（装了 adl-codesign.cer 的机器）可信。别人的
    机器上仍然显示"未知发布者"——要跨机器可信只能买证书或走 CI 签名。

.PARAMETER Path
    要签名的 exe，默认 E:\ADL\target\release\adl-gui.exe

.PARAMETER Thumbprint
    证书指纹。默认从 adl-codesign.cer 读，或按主体名去 CurrentUser\My 找。

.PARAMETER NoTimestamp
    跳过 RFC3161 时间戳。不推荐：没有时间戳的签名在证书过期后立刻失效。

.EXAMPLE
    .\scripts\sign.ps1
    .\scripts\sign.ps1 -Path .\target\release\adl.exe
#>
param(
    [string] $Path = 'E:\ADL\target\release\adl-gui.exe',
    [string] $Thumbprint = '',
    [switch] $NoTimestamp
)

$ErrorActionPreference = 'Stop'

# --- 找 signtool -------------------------------------------------------------
# Windows SDK 装在 D: 盘（不是默认的 C:\Program Files (x86)），所以这里
# 从注册表读 KitsRoot10 再拼，比写死路径稳。
function Find-SignTool {
    $candidates = @()
    foreach ($hive in @(
            'HKLM:\SOFTWARE\Microsoft\Windows Kits\Installed Roots',
            'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows Kits\Installed Roots')) {
        try {
            $root = (Get-ItemProperty -Path $hive -Name KitsRoot10 -ErrorAction Stop).KitsRoot10
            if ($root) {
                $bin = Join-Path $root 'bin'
                if (Test-Path $bin) {
                    $candidates += Get-ChildItem $bin -Directory |
                        Sort-Object Name -Descending |
                        ForEach-Object { Join-Path $_.FullName 'x64\signtool.exe' }
                }
            }
        } catch { }
    }
    $candidates += 'C:\Program Files (x86)\Windows Kits\10\bin\x64\signtool.exe'
    foreach ($c in $candidates) { if ($c -and (Test-Path $c)) { return $c } }
    $cmd = Get-Command signtool.exe -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    throw '找不到 signtool.exe（需要 Windows SDK）'
}

# --- 找证书 ------------------------------------------------------------------
function Find-Cert([string] $thumb) {
    if ($thumb) {
        $c = Get-ChildItem Cert:\CurrentUser\My -CodeSigningCert |
            Where-Object { $_.Thumbprint -eq $thumb }
        if (-not $c) { throw "CurrentUser\My 里没有指纹为 $thumb 的代码签名证书" }
        return $c
    }
    # 按主体名找：只认我们自己那张，避免把别家的证书拿来签
    $found = Get-ChildItem Cert:\CurrentUser\My -CodeSigningCert |
        Where-Object { $_.Subject -match '棱汐\s*Risk|KralisXC' } |
        Sort-Object NotAfter -Descending
    if (-not $found) {
        throw '没有找到代码签名证书。先跑一次 scripts\new-codesign-cert.ps1 创建。'
    }
    return $found[0]
}

$exe = (Resolve-Path -LiteralPath $Path).Path
if (-not (Test-Path -LiteralPath $exe)) { throw "文件不存在：$exe" }

$signtool = Find-SignTool
$cert = Find-Cert $Thumbprint

Write-Host "签名工具 : $signtool"
Write-Host "证书主体 : $($cert.Subject)"
Write-Host "有效期至 : $($cert.NotAfter.ToString('yyyy-MM-dd'))"
Write-Host "目标文件 : $exe"

$signArgs = @('sign', '/sha1', $cert.Thumbprint, '/fd', 'SHA256')
if (-not $NoTimestamp) {
    $signArgs += @('/tr', 'http://timestamp.digicert.com', '/td', 'SHA256')
}
$signArgs += @('/v', $exe)

& $signtool @signArgs
if ($LASTEXITCODE -ne 0) { throw "signtool 失败（exit $LASTEXITCODE）" }

$sig = Get-AuthenticodeSignature $exe
Write-Host ''
Write-Host "验证结果 : $($sig.Status) — $($sig.StatusMessage)"
if ($sig.Status -ne 'Valid') {
    throw "签名存在但验证不通过：$($sig.Status)"
}
Write-Host "签名者   : $($sig.SignerCertificate.Subject)"
if ($sig.TimeStamperCertificate) {
    Write-Host "时间戳   : $($sig.TimeStamperCertificate.Subject)"
}
