in dev mode:

paste this:
# 1. clear the port and any stale instance
Get-NetTCPConnection -LocalPort 1420 -State Listen -ErrorAction SilentlyContinue |
  Select-Object -ExpandProperty OwningProcess -Unique |
  ForEach-Object { Stop-Process -Id $_ -Force }
Get-Process protocol -ErrorAction SilentlyContinue | Stop-Process -Force

# 2. from the project root
cd "C:\Users\rippl\Downloads\The Nartan Build\Live Now\protocol"
$env:PROTOCOL_VM_NAME = "Test-Nartan-Build"
$env:PROTOCOL_VM_MEMORY_MB = "2048"
npm run tauri dev

to build:

paste this:
npm run tauri build