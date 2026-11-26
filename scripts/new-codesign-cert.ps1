<#
.SYNOPSIS
    创建 ADL 的自签名代码签名证书，并装进本机信任存储。

.DESCRIPTION
    一次性操作。已经在跑过就不需要再跑（会重复创建一堆同名证书）。

    做了三件事：
      1. 在 Cert:\CurrentUser\My 建一张 10 年期的代码签名证书（CN=棱汐 Risk）
      2. 导出公钥 .cer 到 certs\，导出带私钥的 .pfx 备份到 certs\
      3. 用 certutil 把 .cer 装进 CurrentUser 的 Root 与 TrustedPublisher

    为什么不用 Import-Certificate 装 Root：PowerShell 5.1 的那个 cmdlet 走的是
    UI 型导入，在非交互会话里直接抛 "UI is not allowed in this operation"。
    certutil -user -addstore 没这个问题。

    安全性提醒：certs\adl-codesign.pfx 里是**私钥**，密码写在脚本输出里。
    这个目录已在 .gitignore 里，别把它传到任何地方。
#>
param(
    [string] $Subject = 'CN=棱汐 Risk, O=KralisXC',
    [string] $OutDir = 'E:\ADL\certs',
    [string] $PfxPassword = 'ADL-CodeSign-2026',
    [int] $Years = 10
)

$ErrorActionPreference = 'Stop'

# 已经有一张能用的就别再造一张 —— 同名证书多了之后 signtool 按主体名选会挑花眼
$existing = Get-ChildItem Cert:\CurrentUser\My -CodeSigningCert -ErrorAction SilentlyContinue |
    Where-Object { $_.Subject -eq $Subject -and $_.NotAfter -gt (Get-Date).AddDays(30) }
if ($existing) {
    Write-Host "已存在可用的证书，跳过创建：" -ForegroundColor Yellow
    $existing | Select-Object Subject, Thumbprint, NotAfter | Format-List
    Write-Host "要重建请先删除：Remove-Item Cert:\CurrentUser\My\$($existing[0].Thumbprint)"
    return
}

Write-Host "== 1/3 创建证书 ==" -ForegroundColor Cyan
$cert = New-SelfSignedCertificate `
    -Type CodeSigningCert `
    -Subject $Subject `
    -KeyUsage DigitalSignature `
    -KeyAlgorithm RSA -KeyLength 4096 `
    -HashAlgorithm SHA256 `
    -FriendlyName 'ADL 代码签名（自签）' `
    -CertStoreLocation 'Cert:\CurrentUser\My' `
    -NotAfter (Get-Date).AddYears($Years) `
    -TextExtension @('2.5.29.37={text}1.3.6.1.5.5.7.3.3')   # EKU = Code Signing
Write-Host "   主体     : $($cert.Subject)"
Write-Host "   指纹     : $($cert.Thumbprint)"
Write-Host "   有效期至 : $($cert.NotAfter.ToString('yyyy-MM-dd'))"

Write-Host "== 2/3 导出 ==" -ForegroundColor Cyan
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$cerPath = Join-Path $OutDir 'adl-codesign.cer'
$pfxPath = Join-Path $OutDir 'adl-codesign.pfx'
Export-Certificate -Cert $cert -FilePath $cerPath -Type CERT | Out-Null
$sec = ConvertTo-SecureString -String $PfxPassword -AsPlainText -Force
Export-PfxCertificate -Cert $cert -FilePath $pfxPath -Password $sec | Out-Null
Write-Host "   公钥 : $cerPath"
Write-Host "   私钥 : $pfxPath （密码 $PfxPassword，别外传）"

Write-Host "== 3/3 装进信任存储 ==" -ForegroundColor Cyan
& certutil.exe -user -addstore Root $cerPath | Out-Null
if ($LASTEXITCODE -ne 0) { throw "装 Root 失败（certutil exit $LASTEXITCODE）" }
& certutil.exe -user -addstore TrustedPublisher $cerPath | Out-Null
if ($LASTEXITCODE -ne 0) { throw "装 TrustedPublisher 失败（certutil exit $LASTEXITCODE）" }

Write-Host ''
Write-Host "完成。现在可以跑 scripts\sign.ps1 给 exe 签名了。" -ForegroundColor Green
Write-Host "提醒：这张证书只在本机可信，别人的机器上仍然显示「未知发布者」。"
