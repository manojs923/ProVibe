<#
.SYNOPSIS
    One-time setup  creates a sysprep-ready Windows 11 base VHDX with RDP enabled,
    minimal services, no Hyper-V guest tools.
.DESCRIPTION
    Builds a clean Windows 11 VHDX by applying the ISO image, staging post-install
    configuration (RDP, service trimming, firewall rules, telemetry removal) and a
    UEFI boot configuration.
    The image is NOT auto-sysprepped - seal it once by hand (see the notes printed
    at the end) before using it as a differencing parent.
.NOTES
    Run on a Hyper-V host as Administrator.
    Requires: Windows 11 ISO, Hyper-V module, ~80GB free disk space.
#>

param(
    [string]$WinISO       = "C:\Users\rippl\Downloads\Windows11_Client_x64_en-us_26300_9457.iso",
    [string]$WorkDir       = "C:\VMBase",
    [string]$VMSwitchName = "Default Switch",
    [string]$BaseVHDX     = "C:\VMBase\Images\Win11-Base.vhdx",
    [int]$DiskGB       = 64,
    [int]$MemGB        = 8,
    [int]$VCpuCount    = 4,
    [string]$BuildVmName = "Win11-BaseBuild",
    [switch]$EnableTPM,
    [switch]$AutoConnect
)

$ErrorActionPreference = "Stop"
$transcriptLog = Join-Path $WorkDir "build-base.log"

# ---------------------------------------------------------------------------
# 0. Bootstrap / validation
# ---------------------------------------------------------------------------
function Assert-Prereqs {
    if (-not (Test-Path $WinISO)) {
        throw "ISO not found: $WinISO  update -WinISO param"
    }
    if (-not (Get-WindowsOptionalFeature -Online -FeatureName Microsoft-Hyper-V -ErrorAction SilentlyContinue)) {
        throw "Hyper-V not enabled on host"
    }
    if (-not (Get-VMSwitch -Name $VMSwitchName -ErrorAction SilentlyContinue)) {
        throw "No VM switch '$VMSwitchName'  create one first or pass -VMSwitchName"
    }
    New-Item -ItemType Directory -Force -Path "$WorkDir\Images","$WorkDir\Mount","$WorkDir\Unattend" | Out-Null
    Start-Transcript -Path $transcriptLog -Force | Out-Null
    Write-Host "[*] Prereqs OK  ISO: $WinISO"
}

# ---------------------------------------------------------------------------
# Drive-letter helpers
#
# An EFI partition never auto-mounts and a freshly formatted data partition is
# not guaranteed to either. Both cases need an explicit, retried assignment.
# ---------------------------------------------------------------------------
function Get-FreeLetter {
    $used = @(Get-Volume | Where-Object { $_.DriveLetter } | ForEach-Object { $_.DriveLetter })
    foreach ($c in [char[]](68..90)) {
        if ($used -notcontains $c) { return $c }
    }
    throw "No free drive letter available"
}

function Mount-PartitionWithLetter {
    param($Partition)

    $letter = ($Partition | Get-Partition).DriveLetter
    if ($letter) { return $letter }

    $free = Get-FreeLetter
    Set-Partition -DiskNumber $Partition.DiskNumber `
                  -PartitionNumber $Partition.PartitionNumber `
                  -NewDriveLetter $free

    for ($i = 0; $i -lt 15; $i++) {
        Start-Sleep -Milliseconds 300
        $letter = ($Partition | Get-Partition).DriveLetter
        if ($letter) { return $letter }
    }
    throw "Could not assign a drive letter to partition $($Partition.PartitionNumber)"
}

# ---------------------------------------------------------------------------
# 1. Mount ISO + stage install image
# ---------------------------------------------------------------------------
function Get-InstallWim {
    $isoDriveLetter = (Mount-DiskImage -ImagePath $WinISO -PassThru | Get-Volume).DriveLetter
    if (-not $isoDriveLetter) { throw "Failed to mount ISO" }

    $wimSrc = "$($isoDriveLetter):\sources\install.wim"
    if (-not (Test-Path $wimSrc)) {
        Write-Host "[*] No install.wim found  checking install.esd"
        $wimSrc = "$($isoDriveLetter):\sources\install.esd"
        if (-not (Test-Path $wimSrc)) {
            Dismount-DiskImage -ImagePath $WinISO | Out-Null
            throw "Neither install.wim nor install.esd found in ISO"
        }
    }

    Copy-Item $wimSrc "$WorkDir\install.wim" -Force
    Dismount-DiskImage -ImagePath $WinISO | Out-Null
    Write-Host "[*] install image staged"
    return "$WorkDir\install.wim"
}

# ---------------------------------------------------------------------------
# 2. Generate unattend.xml for OOBE bypass
# ---------------------------------------------------------------------------
function New-UnattendXml {
    $targetDir = "$WorkDir\Unattend"
    if (-not (Test-Path -LiteralPath $targetDir)) {
        New-Item -Path $targetDir -ItemType Directory -Force | Out-Null
    }

    $unattend = @'
<?xml version="1.0" encoding="utf-8"?>
<unattend xmlns="urn:schemas-microsoft-com:unattend" xmlns:wcm="http://schemas.microsoft.com/WMIConfig/2002/State">
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
                <LogonCount>50</LogonCount>
            </AutoLogon>
        </component>
    </settings>
</unattend>
'@
    $unattend | Out-File "$WorkDir\Unattend\unattend.xml" -Encoding utf8
    Write-Host "[*] unattend.xml generated"
}

# ---------------------------------------------------------------------------
# 3. Generate post-install config script
# ---------------------------------------------------------------------------
function New-BaseConfigScript {
    $cfg = @'
# base-config.ps1  runs during specialize pass
$ErrorActionPreference = "Continue"

# Enable RDP
Set-ItemProperty -Path "HKLM:\SYSTEM\CurrentControlSet\Control\Terminal Server" -Name fDenyTSConnections -Value 0
Enable-NetFirewallRule -DisplayGroup "Remote Desktop"

# Allow RDP with NLA (matches host tunnel config)
Set-ItemProperty -Path "HKLM:\SYSTEM\CurrentControlSet\Control\Terminal Server\WinStations\RDP-Tcp" -Name UserAuthentication -Value 1

# -- Service trimming --
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

# -- Scheduled task trimming (reduce background noise) --
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

# -- Registry tweaks for minimal footprint --
New-Item -Path "HKLM:\SOFTWARE\Policies\Microsoft\Windows\DataCollection" -Force | Out-Null
Set-ItemProperty -Path "HKLM:\SOFTWARE\Policies\Microsoft\Windows\DataCollection" -Name AllowTelemetry -Value 0

New-Item -Path "HKLM:\SOFTWARE\Policies\Microsoft\Windows\CloudContent" -Force | Out-Null
Set-ItemProperty -Path "HKLM:\SOFTWARE\Policies\Microsoft\Windows\CloudContent" -Name DisableWindowsConsumerFeatures -Value 1

powercfg.exe /hibernate off
powercfg.exe /setactive 8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c
powercfg.exe /change standby-timeout-ac 0
powercfg.exe /change monitor-timeout-ac 0
powercfg.exe /change hibernate-timeout-ac 0

# -- Remove Hyper-V Guest Integration Services --
$icFeatures = Get-WindowsOptionalFeature -Online |
    Where-Object { $_.FeatureName -like "*Hyper-V*Guest*" -or $_.FeatureName -like "*Integration*" }

foreach ($feat in $icFeatures) {
    if ($feat.State -eq "Enabled") {
        Disable-WindowsOptionalFeature -Online -FeatureName $feat.FeatureName -NoRestart | Out-Null
        Write-Host "  [ic] Disabled feature: $($feat.FeatureName)"
    }
}

$icMsi = Get-WmiObject -Class Win32_Product -Filter "Name LIKE '%Integration%'" -ErrorAction SilentlyContinue
if ($icMsi) {
    $icMsi | ForEach-Object { $_.Uninstall() }
}

# -- Disable pagefile (reduces disk footprint, faster checkpoint ops) --
$computersys = Get-WmiObject Win32_ComputerSystem -EnableAllPrivileges
$computersys.AutomaticManagedPagefile = $false
$computersys.Put() | Out-Null
$pagefile = Get-WmiObject Win32_PageFileSetting
if ($pagefile) { $pagefile.Delete() }

# -- Windows Update: pause (base image gets updates via rebuild) --
New-Item -Path "HKLM:\SOFTWARE\Policies\Microsoft\Windows\WindowsUpdate" -Force | Out-Null
New-Item -Path "HKLM:\SOFTWARE\Policies\Microsoft\Windows\WindowsUpdate\AU" -Force | Out-Null
Set-ItemProperty -Path "HKLM:\SOFTWARE\Policies\Microsoft\Windows\WindowsUpdate\AU" -Name NoAutoUpdate -Value 1

# -- Reduce Defender overhead --
Set-MpPreference -DisableRealtimeMonitoring $true -ErrorAction SilentlyContinue
Set-MpPreference -DisableBehaviorMonitoring $true -ErrorAction SilentlyContinue
Set-MpPreference -ScanAvgCPULoadFactor 10 -ErrorAction SilentlyContinue

# -- Clear event logs (clean slate for the sealed image) --
wevtutil.exe el | ForEach-Object { wevtutil.exe cl $_ 2>$null }

# -- Remove bloatware AppX packages --
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

# ---------------------------------------------------------------------------
# 4. Create target VHDX from install image
# ---------------------------------------------------------------------------
function New-BaseDisk {
    $wimFile = "$WorkDir\install.wim"
    $vhdxPath = "$WorkDir\Images\build-raw.vhdx"

    # Self-clean so a previous failed run can never collide.
    if (Test-Path $vhdxPath) {
        Write-Host "[*] Removing stale build disk..."
        Dismount-VHD -Path $vhdxPath -ErrorAction SilentlyContinue
        Remove-Item $vhdxPath -Force
    }

    $editions = Get-WindowsImage -ImagePath $wimFile
    Write-Host "[*] Available editions:"
    $editions | ForEach-Object { Write-Host "    [$($_.ImageIndex)] $($_.ImageName)" }

    $proIndex = ($editions | Where-Object { $_.ImageName -like "*Pro*" } | Select-Object -First 1).ImageIndex
    if (-not $proIndex) { $proIndex = $editions[0].ImageIndex }
    Write-Host "[*] Using edition index: $proIndex ($(($editions | Where-Object ImageIndex -eq $proIndex).ImageName))"

    New-VHD -Path $vhdxPath -SizeBytes ($DiskGB * 1GB) -Dynamic | Out-Null
    $mounted = Mount-VHD -Path $vhdxPath -PassThru
    $diskNum = ($mounted | Get-Disk).Number

    Initialize-Disk -Number $diskNum -PartitionStyle GPT
    $sysPart  = New-Partition -DiskNumber $diskNum -Size 100MB -GptType '{c12a7328-f81f-11d2-ba4b-00a0c93ec93b}'  # EFI
    $msrPart  = New-Partition -DiskNumber $diskNum -Size 16MB  -GptType '{e3c9e316-0b5c-4683-9dd1-6df3e0f0e07b}'  # MSR
    $mainPart = New-Partition -DiskNumber $diskNum -UseMaximumSize -GptType '{ebd0a0a2-b9e5-4433-87c0-68b6b72699c7}'  # Basic data

    Format-Volume -Partition $sysPart  -FileSystem FAT32 -NewFileSystemLabel "System"  | Out-Null
    Format-Volume -Partition $mainPart -FileSystem NTFS  -NewFileSystemLabel "Windows" | Out-Null

    Start-Sleep -Seconds 2

    $sysLetter  = Mount-PartitionWithLetter $sysPart
    $mainLetter = Mount-PartitionWithLetter $mainPart

    Write-Host "[*] Applying image to $($mainLetter):\ ..."
    Expand-WindowsImage -ImagePath $wimFile -ApplyPath "$($mainLetter):\" -Index $proIndex

    Copy-Item "$WorkDir\Unattend\unattend.xml" "$($mainLetter):\Windows\System32\Sysprep\unattend.xml" -Force
    New-Item -ItemType Directory -Force -Path "$($mainLetter):\Windows\Setup\Scripts" | Out-Null
    Copy-Item "$WorkDir\Unattend\base-config.ps1" "$($mainLetter):\Windows\Setup\Scripts\base-config.ps1" -Force

    # Stage boot files. bootmgfw.efi is the real filename in the applied image;
    # BOOTX64.EFI is the removable-media fallback the firmware probes on a Gen2
    # VM that has no NVRAM entry yet.
    $bootMgr = "$($mainLetter):\Windows\Boot\EFI\bootmgfw.efi"
    if (-not (Test-Path $bootMgr)) {
        throw "bootmgfw.efi missing from the applied image - apply was incomplete"
    }

    New-Item -ItemType Directory -Force -Path "$($sysLetter):\EFI\Microsoft\Boot" | Out-Null
    New-Item -ItemType Directory -Force -Path "$($sysLetter):\EFI\Boot" | Out-Null
    Copy-Item $bootMgr "$($sysLetter):\EFI\Microsoft\Boot\bootmgfw.efi" -Force
    Copy-Item $bootMgr "$($sysLetter):\EFI\Boot\BOOTX64.EFI" -Force

    Write-Host "[*] Running bcdboot to create boot configuration..."
    $bcdResult = & bcdboot "$($mainLetter):\Windows" /s "$($sysLetter):" /f UEFI /v 2>&1
    Write-Host $bcdResult
    if ($LASTEXITCODE -ne 0) { throw "bcdboot failed with exit code $LASTEXITCODE" }

    # Release the letters so nothing dangles between runs.
    Remove-PartitionAccessPath -DiskNumber $diskNum -PartitionNumber $sysPart.PartitionNumber  -AccessPath "$($sysLetter):\"  -ErrorAction SilentlyContinue
    Remove-PartitionAccessPath -DiskNumber $diskNum -PartitionNumber $mainPart.PartitionNumber -AccessPath "$($mainLetter):\" -ErrorAction SilentlyContinue

    Write-Host "[*] Dismounting VHDX..."
    Dismount-VHD -Path $vhdxPath

    Write-Host "[+] Base disk created: $vhdxPath" -ForegroundColor Green
    return $vhdxPath
}

# ---------------------------------------------------------------------------
# 5. Create throwaway VM from base disk
# ---------------------------------------------------------------------------
function New-StealthVM {
    param([string]$VhdxPath)

    $vmName = "stealth-vm-$(Get-Random -Maximum 9999)"

    $diffDisk = "$WorkDir\Images\differencing.vhdx"
    if (Test-Path $diffDisk) {
        Dismount-VHD -Path $diffDisk -ErrorAction SilentlyContinue
        Remove-Item $diffDisk -Force
    }
    New-VHD -Path $diffDisk -ParentPath $VhdxPath -Differencing | Out-Null

    New-VM -Name $vmName `
           -MemoryStartupBytes 4GB `
           -VHDPath $diffDisk `
           -Generation 2 `
           -SwitchName $VMSwitchName `
           -NoDefaultSecurity | Out-Null

    Set-VM -Name $vmName -ProcessorCount 4 -DynamicMemory -MemoryMinimumBytes 2GB -MemoryMaximumBytes 8GB
    Set-VMProcessor -VMName $vmName -ExposeVirtualizationExtensions $false

    # $true masks host CPU features from the guest. (A later $false here
    # previously cancelled this out.)
    Set-VMProcessor -VMName $vmName -CompatibilityForMigrationEnabled $true

    Set-VMSecurity -VMName $vmName -SecurityPolicyEnabled $false
    Set-VMKeyProtector -VMName $vmName -NewLocalKeyProtector

    Get-VMIntegrationService -VMName $vmName |
        Where-Object { $_.Name -eq "Time Synchronization" } |
        Disable-VMIntegrationService

    Get-VMIntegrationService -VMName $vmName |
        Where-Object { $_.Name -eq "Heartbeat" } |
        Disable-VMIntegrationService

    $bootOrder = @(Get-VMHardDiskDrive -VMName $vmName)
    $bootDvd = Get-VMDvdDrive -VMName $vmName -ErrorAction SilentlyContinue
    if ($bootDvd) { $bootOrder += $bootDvd }
    Set-VMFirmware -VMName $vmName -BootOrder $bootOrder

    if ($EnableTPM) {
        $kp = Get-HgsGuardian -Name "UntrustedGuardian" -ErrorAction SilentlyContinue
        if (-not $kp) {
            $kp = New-HgsKeyProtector -Name "UntrustedGuardian" -AllowUntrustedRoot
        }
        Set-VMKeyProtector -VMName $vmName -KeyProtector $kp.RawData
        Enable-VMTPM -VMName $vmName
    }

    Start-VM -Name $vmName
    Write-Host "[+] VM created and started: $vmName" -ForegroundColor Green
    return $vmName
}

# ---------------------------------------------------------------------------
# 6. Create RDP connection file
# ---------------------------------------------------------------------------
function Connect-RDP {
    param([string]$VMName)

    $vmIP = $null
    $timer = [Diagnostics.Stopwatch]::StartNew()
    while (-not $vmIP -and $timer.Elapsed.TotalSeconds -lt 60) {
        Start-Sleep -Seconds 2
        $vmIP = (Get-VMNetworkAdapter -VMName $VMName).IPAddresses |
            Where-Object { $_ -match '^\d+\.\d+\.\d+\.\d+$' } |
            Select-Object -First 1
    }

    if (-not $vmIP) {
        Write-Host "[!] Could not get VM IP. Check network adapter config." -ForegroundColor Yellow
        return $null
    }
    Write-Host "[*] VM IP: $vmIP"

    $rdpFile = "$WorkDir\vm-connection.rdp"
    $rdpContent = @"
screen mode id:i:2
use multimon:i:0
desktopwidth:i:1920
desktopheight:i:1080
session bpp:i:32
compression:i:1
keyboardhook:i:2
audiocapturemode:i:1
videoplaybackmode:i:1
connection type:i:7
networkautodetect:i:1
bandwidthautodetect:i:1
displayconnectionbar:i:1
bitmapcachepersistenable:i:1
full address:s:$vmIP
audiomode:i:0
redirectclipboard:i:0
autoreconnection enabled:i:1
authentication level:i:0
prompt for credentials:i:0
negotiate security layer:i:1
"@
    Set-Content -Path $rdpFile -Value $rdpContent

    Write-Host "[+] RDP connection file created: $rdpFile" -ForegroundColor Green
    return $rdpFile
}

# ---------------------------------------------------------------------------
# MAIN EXECUTION
# ---------------------------------------------------------------------------
Write-Host "+------------------------------------------------+" -ForegroundColor Cyan
Write-Host "   STEALTH VM DEPLOYMENT PIPELINE v1.1         " -ForegroundColor Cyan
Write-Host "+------------------------------------------------+" -ForegroundColor Cyan
Write-Host ""

Write-Host "[*] Checking prerequisites..." -ForegroundColor Cyan

if (-not (Get-Command "New-VHD" -ErrorAction SilentlyContinue)) {
    Write-Host "[!] Hyper-V PowerShell module not found. Enable Hyper-V first." -ForegroundColor Red
    exit 1
}

Assert-Prereqs

if (-not (Test-Path "$WorkDir\install.wim")) {
    Write-Host "[*] Staging install image from ISO..." -ForegroundColor Yellow
    Get-InstallWim | Out-Null
}

if (-not (Test-Path "$WorkDir\Unattend\unattend.xml")) {
    Write-Host "[*] Generating unattend.xml..." -ForegroundColor Yellow
    New-UnattendXml | Out-Null
}

if (-not (Test-Path "$WorkDir\Unattend\base-config.ps1")) {
    Write-Host "[*] Generating base-config.ps1..." -ForegroundColor Yellow
    New-BaseConfigScript | Out-Null
}

if (-not (Test-Path "$WorkDir\Unattend\unattend.xml")) {
    Write-Host "[!] unattend.xml not found or failed to generate." -ForegroundColor Red
    exit 1
}

if (-not (Test-Path "$WorkDir\Unattend\base-config.ps1")) {
    Write-Host "[!] base-config.ps1 not found or failed to generate." -ForegroundColor Red
    exit 1
}

Write-Host "[+] All prerequisites satisfied." -ForegroundColor Green
Write-Host ""

try {
    Write-Host "-- Step 1: Creating Base Disk --" -ForegroundColor Magenta
    $baseDisk = New-BaseDisk

    Write-Host "-- Step 2: Creating VM --" -ForegroundColor Magenta
    $vmName = New-StealthVM -VhdxPath $baseDisk

    Write-Host "-- Step 3: Creating RDP Connection --" -ForegroundColor Magenta
    $rdpPath = Connect-RDP -VMName $vmName

    Write-Host ""
    Write-Host "+------------------------------------------------+" -ForegroundColor Green
    Write-Host "   DEPLOYMENT COMPLETE                          " -ForegroundColor Green
    Write-Host "+------------------------------------------------+" -ForegroundColor Green
    Write-Host ""
    Write-Host "  VM Name:     $vmName"
    Write-Host "  Base Disk:   $baseDisk"
    Write-Host "  RDP File:    $rdpPath"
    Write-Host ""
    Write-Host "  NEXT: the base image is NOT sealed yet." -ForegroundColor Yellow
    Write-Host "        Boot it directly, finish OOBE, then run sysprep." -ForegroundColor Yellow
    Write-Host "        See the seal instructions before using it as a parent." -ForegroundColor Yellow
    Write-Host ""

    if ($AutoConnect -and $rdpPath) {
        Write-Host "[*] Opening RDP connection..."
        Start-Process "mstsc.exe" -ArgumentList $rdpPath
    }

} catch {
    Write-Host "[!] Pipeline failed: $_" -ForegroundColor Red
    Write-Host $_.ScriptStackTrace -ForegroundColor DarkRed
    Stop-Transcript -ErrorAction SilentlyContinue | Out-Null
    exit 1
}

Stop-Transcript -ErrorAction SilentlyContinue | Out-Null


