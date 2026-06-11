param(
    [string]$Version = "dev"
)

$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
Set-Location $Root

$Arch = "x86_64"
if ($env:PROCESSOR_ARCHITECTURE -eq "ARM64") {
    $Arch = "aarch64"
}

$BinUi = Join-Path $Root "target\release\multerm-ui.exe"
$BinTerm = Join-Path $Root "target\release\multerm.exe"
if (-not (Test-Path $BinUi)) {
    Write-Error "Missing release binary. Run: cargo build --release -p multerm-app --bins"
}

$DirName = "multerm-$Version-windows-$Arch"
$Stage = Join-Path $Root "dist\$DirName"
$BinDir = Join-Path $Stage "bin"

Remove-Item -Recurse -Force $Stage -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $BinDir | Out-Null

Copy-Item $BinUi (Join-Path $BinDir "multerm-ui.exe")
if (Test-Path $BinTerm) {
    Copy-Item $BinTerm (Join-Path $BinDir "multerm.exe")
}

@(
    "Multerm $Version (windows-$Arch)",
    "",
    "Run the workspace UI:",
    "  .\bin\multerm-ui.exe",
    "",
    "Optional lean GPU terminal:",
    "  .\bin\multerm.exe",
    "",
    "Requires a GPU with a DirectX 12 / Vulkan compatible wgpu backend."
) | Set-Content -Path (Join-Path $Stage "README.txt") -Encoding UTF8

$Zip = Join-Path $Root "dist\multerm-$Version-windows-$Arch.zip"
if (Test-Path $Zip) { Remove-Item $Zip -Force }
Compress-Archive -Path $Stage -DestinationPath $Zip
Write-Host "Created $Zip"
