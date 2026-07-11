$ErrorActionPreference = "Stop"

$root = Split-Path -Parent $PSScriptRoot
Push-Location $root
try {
    cargo test --lib
    cargo build --release --target x86_64-pc-windows-msvc
    New-Item -ItemType Directory -Force -Path "publish" | Out-Null
    Copy-Item -Force `
        "target\x86_64-pc-windows-msvc\release\GiantFileExchange.exe" `
        "publish\GiantFileExchange.exe"
    Write-Host "已生成 publish\GiantFileExchange.exe"
}
finally {
    Pop-Location
}
