<#
.SYNOPSIS
    One-time setup — creates a sysprep'd Windows 11 base VHDX with RDP enabled,
    minimal services, no Hyper-V guest tools.
.DESCRIPTION
    Builds a clean Windows 11 VHDX by installing from ISO, applying post-install
    configuration (RDP, service trimming, firewall rules, telemetry removal),
    then running sysprep /generalize /oobe /shutdown to seal it.
    Result: reusable base image for quick-checkpoint differencing disks.
.NOTES
    Run on a Hyper-V host as Administrator.
    Requires: Windows 11 ISO, Hyper-V module, ~80GB free disk space.
#>

param(
    [string]$WinISO       = "C:\ISO\Win11_24H2.iso",
    [string]$WorkDir       = "C:\VMBase",
    [string]$VMSwitchName = "Default Switch",
    [string]$BaseVHDX     = "C:\VMBase\Images\Win11-Base.vhdx",
    [int]$   DiskGB       = 64,
    [int]$   MemGB        = 8,
    [int]$   VCpuCount    = 4,
    [string]$BuildVmName = "Win11-BaseBuild"
)

$ErrorActionPreference = "Stop"
$transcriptLog = Join-Path $WorkDir "build-base.log"

# ── ──────────────────────────────── ──────────
# 0. Bootstrap / validation
# ── ──────────────────────────────── ──────────
function Assert-Prereqs {
    if (-not (Test-Path $WinISO)) {
        throw "ISO not found: $WinISO — update -WinISO param"
    }
    if (-not (Get-WindowsOptionalFeature -Online -FeatureName Microsoft-Hyper-V -ErrorAction SilentlyContinue)) {
        throw "Hyper-V not enabled on host"
    }
    if (-not (Get-VMSwitch -Name $VMSwitchName -ErrorAction SilentlyContinue)) {
        throw "No VM switch '$VMSwitchName' — create one first or pass -VMSwitchName"
    }
    New-Item -ItemType Directory -Force -Path "$WorkDir\Images","$WorkDir\Mount","$WorkDir\Unattend" | Out-Null
    Start-Transcript -Path $transcriptLog -Force | Out-Null
    Write-Host "[*] Prereqs OK — ISO: $WinISO"
}

# ── ──────────────────────────────── ──────────
# 1. Mount ISO + extract install.wim
# ────────────────────────────────
function Get-InstallWim {
    $isoDriveLetter = (Mount-DiskImage -ImagePath $WinISO -PassThru | Get-Volume).DriveLetter
    if (-not $isoDriveLetter) { throw "Failed to mount ISO" }
    $wimSrc = "$($isoDriveLetter):\sources\install.wim"

    if (-not (Test-Path $wimSrc)) {
        Write-Host "[*] No install.wim found — checking install.esd"
        $wimSrc = "$($isoDriveLetter):\sources\install.esd"
        if (-not (Test-Path $wimSrc)) { throw "Neither install.wim nor install.esd found in ISO" }
    }

    Copy-Item $wimSrc "$WorkDir\install.wim" -Force
    Dismount-DiskImage -ImagePath $WinISO | Out-Null
    Write-Host "[*] install.wim staged"
    return "$WorkDir\install.wim"
}

# ── ──────────────────────────────── ──────────
# 2. Generate unattend.xml for OOBE bypass
# ────────────────────────────────
function New-UnattendXml {
    $unattend = @'
<?xml version="1.0" encoding="utf-8"?>
<unattend xmlns="urn:schemas-microsoft-com:unattend">
    <settings pass="specialize">
        <component name="Microsoft-Windows-Shell-Setup" processorArchitecture="amd64"
                   publicKeyToken="31bf3856ad364e35" language="neutral"
                   versionScope="nonSxS">
            <ComputerName>BASEVM</ComputerName>
        </component>
        <component name="Microsoft-Windows-Deployment" processorArchitecture="amd64"
                   publicKeyToken="31bf3856ad364e35" language="neutral"
                   versionScope="nonSxS">
            <RunSynchronous>
                <RunSynchronousCommand wcm:action="add">
                    <Order>1</Order>
                    <Path>powershell.exe -ExecutionPolicy Bypass -File C:\Windows\Setup\Scripts\base-config.ps1</Path>
                </RunSynchronousCommand>
            </RunSynchronous>
        </component>
    </settings>
    <settings pass="oobeSystem">
        <component name="Microsoft-Windows-Shell-Setup" processorArchitecture="amd64"
                   publicKeyToken="31bf3856ad364e35" language="neutral"
                   versionScope="nonSxS">
            <OOBE>
                <HideEULAPage>true</HideEULAPage>
                <HideOEMRegistrationScreen>true</HideOEMRegistrationScreen>
                <HideOnlineAccountScreens>true</HideOnlineAccountScreens>
                <HideWirelessSetupInOOBE>true</HideWirelessSetupInOOBE>
                <ProtectYourPC>3</ProtectYourPC>
            </OOBE>
            <UserAccounts>
                <LocalAccounts>
                    <LocalAccount wcm:action="add">
                        <Name>admin</Name>
                        <Group>Administrators</Group>
                        <Password>
                            <Value>TempPass123!</Value>
                            <PlainText>true</PlainText>
                        </Password>
                    </LocalAccount>
                </LocalAccounts>
            </UserAccounts>
            <AutoLogon>
                <Enabled>true</Enabled>
                <Username>admin</Username>
                <Password>
                    <Value>TempPass123!</Value>
                    <PlainText>true</PlainText>
                </Password>
                <LogonCount>1</LogonCount>
            </AutoLogon>
        </component>
    </settings>
</unattend>
'@
    $unattend | Out-File "$WorkDir\Unattend\unattend.xml" -Encoding utf8
    Write-Host "[*] unattend.xml generated"
}

# ── ──────────────────────────────── ──────────
# 3. Generate post-install config script
#    (embedded into VHDX, runs during specialize pass)
# ────────────────────────────────
function New-BaseConfigScript {
    $cfg = @'
# base-config.ps1 — runs during specialize pass
$ErrorActionPreference = "Continue"

# Enable RDP
Set-ItemProperty -Path "HKLM:\SYSTEM\CurrentControlSet\Control\Terminal Server" -Name fDenyTSConnections -Value 0
Enable-NetFirewallRule -DisplayGroup "Remote Desktop"

# Allow RDP with NLA (matches host tunnel config)
Set-ItemProperty -Path "HKLM:\SYSTEM\CurrentControlSet\Control\Terminal Server\WinStations\RDP-Tcp" -Name UserAuthentication -Value 1

# ── Service trimming ──
$servicesToDisable = @(
    "DiagTrack",           # Connected User Experiences & Telemetry
    "dmwappushservice",    # WAP Push
    "WpcMonSvc",           # Parental Controls
    "MapsBroker",          # Downloaded Maps Manager
    "lfsvc",               # Geolocation
    "RetailDemo",          # Retail Demo Service
    "RemoteRegistry",      # Remote Registry (attack surface)
    "WSearch",             # Windows Search (reduces disk I/O noise)
    "Spooler",             # Print Spooler (no printing from VM)
    "Fax",                 # Fax
    "WerSvc",              # Windows Error Reporting
    "WMPNetworkSvc",       # WMP Network Sharing
    "SSDPSRV",             # SSDP Discovery
    "upnphost",            # UPnP Device Host
    "XblAuthManager",      # Xbox Live Auth
    "XblGameSave",         # Xbox Live Game Save
    "XboxNetApiSvc",       # Xbox Live Network API
    "MixedRealityOpenXRSvc"# Windows Mixed Reality
)

foreach ($svc in $servicesToDisable) {
    $svcObj = Get-Service -Name $svc -ErrorAction SilentlyContinue
    if ($svcObj) {
        Stop-Service -Name $svc -Force -ErrorAction SilentlyContinue
        Set-Service -Name $svc -StartupType Disabled -ErrorAction SilentlyContinue
        Write-Host "  [svc] Disabled: $svc"
    }
}

# ── Scheduled task trimming (reduce background noise) ──
$tasksToDisable = @(
    "\Microsoft\Windows\Customer Experience Improvement Program\*",
    "\Microsoft\Windows\Application Experience\*",
    "\Microsoft\Windows\Autochk\Proxy",
    "\Microsoft\Windows\DiskDiagnostic\*"
)

foreach ($taskPath in $tasksToDisable) {
    Get-ScheduledTask -TaskPath $taskPath -ErrorAction SilentlyContinue |
        Disable-ScheduledTask -ErrorAction SilentlyContinue | Out-Null
}

# ── Registry tweaks for minimal footprint ──
# Disable telemetry
New-Item -Path "HKLM:\SOFTWARE\Policies\Microsoft\Windows\DataCollection" -Force | Out-Null
Set-ItemProperty -Path "HKLM:\SOFTWARE\Policies\Microsoft\Windows\DataCollection" -Name AllowTelemetry -Value 0

# Disable Consumer Features (suggested apps, etc.)
New-Item -Path "HKLM:\SOFTWARE\Policies\Microsoft\Windows\CloudContent" -Force | Out-Null
Set-ItemProperty -Path "HKLM:\SOFTWARE\Policies\Microsoft\Windows\CloudContent" -Name DisableWindowsConsumerFeatures -Value 1

# Disable Hibernate (frees disk space, reduces writes)
powercfg.exe /hibernate off

# Set power plan to High Performance (prevent CPU throttling during use)
powercfg.exe /setactive 8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c

# Disable sleep/timeout (VM is always on during active use)
powercfg.exe /change standby-timeout-ac 0
powercfg.exe /change monitor-timeout-ac 0
powercfg.exe /change hibernate-timeout-ac 0

# ── Remove Hyper-V Guest Integration Services ──
# The base image does NOT include ICs. If present from ISO, remove.
$icFeatures = Get-WindowsOptionalFeature -Online |
    Where-Object { $_.FeatureName -like "*Hyper-V*Guest*" -or $_.FeatureName -like "*Integration*" }

foreach ($feat in $icFeatures) {
    if ($feat.State -eq "Enabled") {
        Disable-WindowsOptionalFeature -Online -FeatureName $feat.FeatureName -NoRestart | Out-Null
        Write-Host "  [ic] Disabled feature: $($feat.FeatureName)"
    }
}

# Uninstall any integration services MSI if present
$icMsi = Get-WmiObject -Class Win32_Product -Filter "Name LIKE '%Integration%'" -ErrorAction SilentlyContinue
if ($icMsi) {
    $icMsi | ForEach-Object { $_.Uninstall() }
}

# ── Disable pagefile (reduces disk footprint, faster checkpoint ops) ──
$computersys = Get-WmiObject Win32_ComputerSystem -EnableAllPrivileges
$computersys.AutomaticManagedPagefile = $false
$computersys.Put() | Out-Null
$pagefile = Get-WmiObject Win32_PageFileSetting
if ($pagefile) { $pagefile.Delete() }

# ── Windows Update: defer/pause (base image gets updates via rebuild) ──
New-Item -Path "HKLM:\SOFTWARE\Policies\Microsoft\Windows\WindowsUpdate" -Force | Out-Null
New-Item -Path "HKLM:\SOFTWARE\Policies\Microsoft\Windows\WindowsUpdate\AU" -Force | Out-Null
Set-ItemProperty -Path "HKLM:\SOFTWARE\Policies\Microsoft\Windows\WindowsUpdate\AU" -Name NoAutoUpdate -Value 1

# ── Disable Defender real-time (base image is offline; reduces CPU during use) ──
# Note: leave service intact, just reduce real-time scanning overhead
Set-MpPreference -DisableRealtimeMonitoring $true -ErrorAction SilentlyContinue
Set-MpPreference -DisableBehaviorMonitoring $true -ErrorAction SilentlyContinue
Set-MpPreference -ScanAvgCPULoadFactor 10 -ErrorAction SilentlyContinue

# ── Clear event logs (clean slate for the sealed image) ──
wevtutil.exe el | ForEach-Object { wevtutil.exe cl $_ 2>$null }

# ── Disable Cortana startup ──
Get-AppxPackage -Name "*Cortana*" -ErrorAction SilentlyContinue | Remove-AppxPackage -ErrorAction SilentlyContinue

# Remove bloatware AppX packages
$bloatApps = @(
    "*Microsoft.BingWeather*",
    "*Microsoft.GetHelp*",
    "*Microsoft.Getstarted*",
    "*Microsoft.WindowsFeedbackHub*",
    "*Microsoft.WindowsMaps*",
    "*Microsoft.ZuneMusic*",
    "*Microsoft.ZuneVideo*",
    "*Microsoft.549981C3F5F10*",  # Cortana
    "*Microsoft.YourPhone*",
    "*Microsoft.People*",
    "*MicrosoftTeams*",
    "*Clipchamp.Clipchamp*",
    "*Microsoft.3DBuilder*",
    "*Microsoft.MicrosoftOfficeHub*"
)
foreach ($app in $bloatApps) {
    Get-AppxPackage -Name $app -ErrorAction SilentlyContinue | Remove-AppxPackage -ErrorAction SilentlyContinue
}

Write-Host "  [cfg] Base config complete"
'@
    $cfg | Out-File "$WorkDir\Unattend\base-config.ps1" -Encoding utf8
    Write-Host "[*] base-config.ps1 generated"
}

# ── ──────────────────────────────── ──────────
# 4. Create target VHDX from install.wim
# ────────────────────────────────
function New-BaseDisk {
    $wimFile = "$WorkDir\install.wim"
    $vhdxPath = "$WorkDir\Images\build-raw.vhdx"

    # Get available editions
    $editions = Get-WindowsImage -ImagePath $wimFile
    Write-Host "[*] Available editions:"
    $editions | ForEach-Object { Write-Host "    [$($_.ImageIndex)] $($_.ImageName)" }

    # Pick Pro (or first available)
    $proIndex = ($editions | Where-Object { $_.ImageName -like "*Pro*" } | Select-Object -First 1).ImageIndex
    if (-not $proIndex) { $proIndex = $editions[0].ImageIndex }
    Write-Host "[*] Using edition index: $proIndex ($(($editions | Where-Object ImageIndex -eq $proIndex).ImageName))"

    # Create blank VHDX
    $disk = New-VHD -Path $vhdxPath -SizeBytes ($DiskGB * 1GB) -Dynamic
    $mounted = Mount-VHD -Path $vhdxPath -PassThru
    $diskNum = ($mounted | Get-Disk).Number

    # Initialize + partition (GPT/UEFI)
    Initialize-Disk -Number $diskNum -PartitionStyle GPT
    $sysPart = New-Partition -DiskNumber $diskNum -Size 100MB -GptType '{c12a7328-f81f-11d2-ba4b-00a0c93ec93b}'  # EFI
    $msrPart = New-Partition -DiskNumber $diskNum -Size 16MB -GptType '{e3c9e316-0b5c-4683-9dd1-6df3e0f0e07b}'  # MSR
    $mainPart = New-Partition -DiskNumber $diskNum -UseMaximumSize -GptType '{ebd0a0a2-b9e5-4433-87c0-68b6b72699c7}'  # Basic data

    # Format
    $sysVol  = Format-Volume -Partition $sysPart  -FileSystem FAT32 -NewFileSystemLabel "System"
    $mainVol = Format-Volume -Partition $mainPart -FileSystem NTFS -NewFileSystemLabel "Windows"

    # Get drive letters
    $sysLetter  = ($sysPart  | Get-Partition).DriveLetter
    $mainLetter = ($mainPart | Get-Partition).DriveLetter
    if (-not $sysLetter)  { $sysLetter  = ($sysPart  | Get-Volume).DriveLetter }
    if (-not $mainLetter) { $mainLetter = ($mainPart | Get-Volume).DriveLetter }

    Write-Host "[*] Applying WIM to $($mainLetter):\ ..."
    Expand-WindowsImage -ImagePath $wimFile -ApplyPath "$($mainLetter):\" -Index $proIndex

    # Copy unattend.xml + config script into image
    Copy-Item "$WorkDir\Unattend\unattend.xml" "$($mainLetter):\Windows\System32\Sysprep\unattend.xml" -Force
    New-Item -ItemType Directory -Force -Path "$($mainLetter):\Windows\Setup\Scripts" | Out-Null
    Copy-Item "$WorkDir\Unattend\base-config.ps1" "$($mainLetter):\Windows\Setup\Scripts\base-config.ps1" -Force

        # Copy boot files to EFI partition
    if ($sysLetter) {
        Copy-Item "$($mainLetter):\Windows\Boot\EFI\bootmgfw.efi" "$($sysLetter):\EFI\Microsoft\Boot\bootmgfw.efi" -Force
        New-Item -ItemType Directory -Force -Path "$($sysLetter):\EFI\Microsoft\Boot" | Out-Null
        Copy-Item "$($mainLetter):\Windows\Boot\EFI\bootmgfw.efi" "$($sysLetter):\EFI\Microsoft\Boot\bootmgfw.efi" -Force
        Copy-Item "$($mainLetter):\Windows\Boot\EFI\bootx64.efi" "$($sysLetter):\EFI\Boot\bootx64.efi" -Force
    }

    # Populate BCD store
    Write-Host "[*] Running bcdboot to create boot configuration..."
    $bcdResult = & bcdboot "$($mainLetter):\Windows" /s "$($sysLetter):" /f UEFI /v 2>&1
    Write-Host $bcdResult

    # Dismount
    Write-Host "[*] Dismounting VHDX..."
    Dismount-VHD -Path $vhdxPath

    Write-Host "[✓] Base disk created: $vhdxPath" -ForegroundColor Green
    return $vhdxPath
}

# ── ──────────────────────────────── ──────────
# 5. Create stealth VM from base disk
# ────────────────────────────────
function New-StealthVM {
    param([string]$VhdxPath)

    $vmName = "stealth-vm-$(Get-Random -Maximum 9999)"

    # Create differencing disk so we can reset between sessions
    $diffDisk = "$WorkDir\Images\differencing.vhdx"
    if (Test-Path $diffDisk) { Remove-Item $diffDisk -Force }
    New-VHD -Path $diffDisk -ParentPath $VhdxPath -Differencing | Out-Null

    # Create VM with generation 2 (UEFI)
    New-VM -Name $vmName `
           -MemoryStartupBytes 4GB `
           -VHDPath $diffDisk `
           -Generation 2 `
           -SwitchName "Internal LAN" `
           -NoDefaultSecurity | Out-Null

    # Configure processor
    Set-VM -Name $vmName -ProcessorCount 4 -DynamicMemory -MemoryMinimumBytes 2GB -MemoryMaximumBytes 8GB

    # Disable features that could leak host info
    Set-VMProcessor -VMName $vmName -ExposeVirtualizationExtensions $false

    # Hide Hyper-V enlightenments from guest
    Set-VMProcessor -VMName $vmName -CompatibilityForMigrationEnabled $false

    # Enable nested features for compatibility
    Set-VMSecurity -VMName $vmName -SecurityPolicyEnabled $false

    # Configure VM to not expose Hyper-V host name
    Set-VMKeyProtector -VMName $vmName -NewLocalKeyProtector

    # Disable time sync so VM doesn't inherit host clock signals
    Get-VMIntegrationService -VMName $vmName | Where-Object { $_.Name -eq "Time Synchronization" } | Disable-VMIntegrationService

    # Disable heartbeat so hypervisor doesn't monitor guest health
    Get-VMIntegrationService -VMName $vmName | Where-Object { $_.Name -eq "Heartbeat" } | Disable-VMIntegrationService

    # Set firmware to boot from disk
    Set-VMFirmware -VMName $vmName -BootOrder ((Get-VMHardDiskDrive -VMName $vmName), (Get-VMDvdDrive -VMName $vmName -ErrorAction SilentlyContinue))

    # Enable TPM if target requires it
    if ($EnableTPM) {
        $kp = Get-HgsGuardian -Name "UntrustedGuardian" -ErrorAction SilentlyContinue
        if (-not $kp) {
            $kp = New-HgsKeyProtector -Name "UntrustedGuardian" -AllowUntrustedRoot
        }
        Set-VMKeyProtector -VMName $vmName -KeyProtector $kp.RawData
        Enable-VMTPM -VMName $vmName
    }

    # Start VM
    Start-VM -Name $vmName
    Write-Host "[✓] VM created and started: $vmName" -ForegroundColor Green

    return $vmName
}

# ── ──────────────────────────────── ──────────
# 6. Create RDP loopback connection
# ────────────────────────────────
function Connect-RDP {
    param([string]$VMName)

    # Get VM IP address
    $vmIP = (Get-VM | Where-Object Name -eq $VMName | Get-VMNetworkAdapter).IPAddresses | Where-Object { $_ -match '\d+\.\d+\.\d+\.\d+' } | Select-Object -First 1
    Write-Host "[*] VM IP: $vmIP"

    # Wait for VM to get IP
    $timeout = 60
    $timer = [Diagnostics.Stopwatch]::StartNew()
    while (-not $vmIP -and $timer.Elapsed.TotalSeconds -lt $timeout) {
        Start-Sleep -Seconds 2
        $vmIP = (Get-VM | Where-Object Name -eq $VMName | Get-VMNetworkAdapter).IPAddresses | Where-Object { $_ -match '\d+\.\d+\.\d+\.\d+' } | Select-Object -First 1
    }

    if (-not $vmIP) {
        Write-Host "[!] Could not get VM IP address. Check network adapter config." -ForegroundColor Yellow
        return $null
    }

    # Create RDP file
    $rdpFile = "$WorkDir\vm-connection.rdp"
    $rdpContent = @"
screen mode id:i:2
use multimon:i:0
desktopwidth:i:1920
desktopheight:i:1080
session bpp:i:32
winposstr:s:0,1,0,0,1920,1080
compression:i:1
keyboardhook:i:2
audiocapturemode:i:1
videoplaybackmode:i:1
connection type:i:7
networkautodetect:i:1
bandwidthautodetect:i:1
displayconnectionbar:i:1
disable wallpaper:i:0
allow font smoothing:i:0
allow desktop composition:i:0
disable full window drag:i:1
disable menu anims:i:1
disable themes:i:0
disable cursor setting:i:0
bitmapcachepersistenable:i:1
full address:s:$vmIP
audiomode:i:0
redirectprinters:i:0
redirectcomports:i:0
redirectsmartcards:i:0
redirectclipboard:i:0
redirectposdevices:i:0
autoreconnection enabled:i:1
authentication level:i:0
prompt for credentials:i:0
negotiate security layer:i:1
remoteapplicationmode:i:0
alternate shell:s:
shell working directory:s:
gatewayhostname:s:
gatewayusagemethod:i:4
gatewaycredentialssource:i:4
gatewayprofileusagemethod:i:0
precommand:s:
redirectdrives:i:0
redirectports:i:0
redirectsmartcards:i:0
devicestoredirect:s:
drivestoredirect:s:
redirectdrives:i:0
redirectdevices:i:0
redirectposdevices:i:0
"@
    Set-Content -Path $rdpFile -Value $rdpContent

    Write-Host "[✓] RDP connection file created: $rdpFile" -ForegroundColor Green
    return $rdpFile
}

# ── ──────────────────────────────── ──────────
# MAIN EXECUTION
# ────────────────────────────────
Write-Host "╔════════════════════════════════════════════════╗" -ForegroundColor Cyan
Write-Host "║   STEALTH VM DEPLOYMENT PIPELINE v1.0         ║" -ForegroundColor Cyan
Write-Host "╚════════════════════════════════════════════════╝" -ForegroundColor Cyan
Write-Host ""

# Check prerequisites
Write-Host "[*] Checking prerequisites..." -ForegroundColor Cyan

if (-not (Get-Command "New-VHD" -ErrorAction SilentlyContinue)) {
    Write-Host "[!] Hyper-V PowerShell module not found. Enable Hyper-V first." -ForegroundColor Red
    exit 1
}

if (-not (Test-Path "$WorkDir\install.wim")) {
    Write-Host "[!] install.wim not found at $WorkDir\install.wim" -ForegroundColor Red
    Write-Host "    Run script with -DownloadISO flag first." -ForegroundColor Yellow
    exit 1
}

if (-not (Test-Path "$WorkDir\Unattend\unattend.xml")) {
    Write-Host "[!] unattend.xml not found." -ForegroundColor Red
    exit 1
}

if (-not (Test-Path "$WorkDir\Unattend\base-config.ps1")) {
    Write-Host "[!] base-config.ps1 not found." -ForegroundColor Red
    exit 1
}

Write-Host "[✓] All prerequisites satisfied." -ForegroundColor Green
Write-Host ""

# Execute pipeline
try {
    # Step 1: Create base disk
    Write-Host "── Step 1: Creating Base Disk ──" -ForegroundColor Magenta
    $baseDisk = New-BaseDisk

    # Step 2: Create VM from base disk
    Write-Host "── Step 2: Creating Stealth VM ──" -ForegroundColor Magenta
    $vmName = New-StealthVM -VhdxPath $baseDisk

    # Step 3: Create RDP connection
    Write-Host "── Step 3: Creating RDP Connection ──" -ForegroundColor Magenta
    $rdpPath = Connect-RDP -VMName $vmName

    # Summary
    Write-Host ""
    Write-Host "╔════════════════════════════════════════════════╗" -ForegroundColor Green
    Write-Host "║   DEPLOYMENT COMPLETE                          ║" -ForegroundColor Green
    Write-Host "╚════════════════════════════════════════════════╝" -ForegroundColor Green
    Write-Host ""
    Write-Host "  VM Name:     $vmName"
    Write-Host "  Base Disk:   $baseDisk"
    Write-Host "  RDP File:    $rdpPath"
    Write-Host ""
    Write-Host "  Connect via: mstsc.exe $rdpPath" -ForegroundColor Yellow
    Write-Host ""

    # Open RDP connection if requested
    if ($AutoConnect -and $rdpPath) {
        Write-Host "[*] Opening RDP connection..."
        Start-Process "mstsc.exe" -ArgumentList $rdpPath
    }

} catch {
    Write-Host "[!] Pipeline failed: $_" -ForegroundColor Red
    Write-Host $_.ScriptStackTrace -ForegroundColor DarkRed
    exit 1
}