# 把 linux-mixum-micro 配置为 WSL 2 的 guest 内核。
#
# 用法（Windows PowerShell，建议管理员）：
#   .\install-wsl2.ps1 -Kernel C:\path\to\linux-mixum-micro
#
# 脚本会备份 %USERPROFILE%\.wslconfig，写入 [wsl2] kernel=...，然后 wsl --shutdown。
param(
    [Parameter(Mandatory = $true)][string]$Kernel,
    [switch]$Yes
)

$ErrorActionPreference = "Stop"

if (-not (Test-Path $Kernel)) {
    throw "内核文件不存在: $Kernel"
}
$Kernel = (Resolve-Path $Kernel).Path

$cfg = Join-Path $env:USERPROFILE ".wslconfig"
if (Test-Path $cfg) {
    $backup = "$cfg.bak-$(Get-Date -Format yyyyMMddHHmmss)"
    Copy-Item $cfg $backup
    Write-Host "已备份: $backup"
}

$lines = @()
if (Test-Path $cfg) {
    $lines = @(Get-Content $cfg)
}

# 去掉旧的 kernel= 行，避免重复。
$filtered = @()
foreach ($line in $lines) {
    if ($line -match '^\s*kernel\s*=') { continue }
    $filtered += $line
}

# 转义 Windows 路径给 .wslconfig（反斜杠需要双写）。
$kernelValue = $Kernel.Replace('\', '\\')

$hasWsl2 = $false
foreach ($line in $filtered) {
    if ($line -match '^\s*\[wsl2\]\s*$') { $hasWsl2 = $true; break }
}

if ($hasWsl2) {
    $out = @()
    $inserted = $false
    foreach ($line in $filtered) {
        $out += $line
        if (-not $inserted -and $line -match '^\s*\[wsl2\]\s*$') {
            $out += "kernel=$kernelValue"
            $inserted = $true
        }
    }
}
else {
    $out = $filtered + @("", "[wsl2]", "kernel=$kernelValue")
}

if (-not $Yes) {
    Write-Host "即将写入 $cfg :"
    Write-Host "[wsl2]"
    Write-Host "kernel=$kernelValue"
    $answer = Read-Host "继续？[y/N]"
    if ($answer -notin @("y", "Y")) { Write-Host "已取消"; exit 1 }
}

$out | Set-Content -Path $cfg -Encoding UTF8
Write-Host "已写入 $cfg"
Write-Host "重启 WSL: wsl --shutdown"
wsl --shutdown
Write-Host "完成。重新进入 WSL 后执行 uname -r，应看到 6.18.35-mixum-micro。"
