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

------------------------------------------------------------------------------------------------------------------------

to build:

paste this:
npm run tauri build

---------------------------------------------------------------------------------------------------------------------------------

to use in exam:

# 0. image present and full size?
Get-Item "$env:ProgramFiles\protocol\resources\base.vhdx" | Select-Object Length

# 1. clear the previous guest and its diff disk — the name is fixed, so a
#    leftover VM gets reused with whatever the last session left on it
Get-Process vmconnect -ErrorAction SilentlyContinue | Stop-Process -Force
Remove-VM -Name Test-Nartan-Build -Force -ErrorAction SilentlyContinue
Remove-Item "$env:ProgramData\Protocol\VMs\Test-Nartan-Build.vhdx" -Force -ErrorAction SilentlyContinue

# 2. launch
& "$env:ProgramFiles\protocol\protocol.exe"

------------------------------------------------------------------------------------------------------------------------

to remove and re build:

Get-Process protocol -ErrorAction SilentlyContinue | Stop-Process -Force
Remove-Item -Recurse -Force .\src-tauri\target\release -ErrorAction SilentlyContinue
Remove-Item -Recurse -Force .\dist -ErrorAction SilentlyContinue
npm run tauri build

------------------------------------------------------

run fast:

$env:PROTOCOL_BASE_VHDX = "C:\Users\rippl\Downloads\The Nartan Build\Live Now\protocol\src-tauri\resources\base.vhdx"
& ".\src-tauri\target\release\protocol.exe"

---------------------------------------------------------------------

to kill protocol:

@'
Get-Process protocol -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 400
Get-Process vmconnect -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Remove-VM -Name Test-Nartan-Build -Force -ErrorAction SilentlyContinue
Remove-Item "$env:ProgramData\Protocol\VMs\*.vhdx" -Force -ErrorAction SilentlyContinue
Start-Process "$env:ProgramFiles\protocol\protocol.exe"
'@ | Set-Content -Path "C:\Users\rippl\protocol-session.ps1" -Encoding ASCII
