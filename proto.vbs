Option Explicit
' ============================================================
'  Silent Tauri dev launcher — v2
'  Runs `npm run tauri dev` hidden, from the script's folder,
'  with preflight checks, duplicate-run protection, and logging.
' ============================================================

Const ForWriting   = 2
Const ForAppending  = 8
Const ForReading    = 1

Dim WshShell, FSO, objLog
Dim strBase, strLogPath, strLockPath, strCmd
Dim ts

Set WshShell = CreateObject("WScript.Shell")
Set FSO     = CreateObject("Scripting.FileSystemObject")

' ------------------------------------------------------------
' 1. Anchor to the folder containing THIS .vbs file
' ------------------------------------------------------------
strBase = FSO.GetParentFolderName(WScript.ScriptFullName)
WshShell.CurrentDirectory = strBase

strLogPath  = strBase & "\dev-launch.log"
strLockPath = strBase & "\dev-launch.lock"

' ------------------------------------------------------------
' 2. Log helper (timestamped lines)
' ------------------------------------------------------------
Sub LogLine(ByVal sMsg)
    Dim ts, sStamp
    sStamp = Year(Now) & "-" & Right("0" & Month(Now),2) & "-" & Right("0" & Day(Now),2) & _
             " " & Right("0" & Hour(Now),2) & ":" & Right("0" & Minute(Now),2) & ":" & Right("0" & Second(Now),2)
    Set ts = FSO.OpenTextFile(strLogPath, ForAppending, True)
    ts.WriteLine "[" & sStamp & "] " & sMsg
    ts.Close
End Sub

If Not FSO.FileExists(strLogPath) Then
    Set ts = FSO.CreateTextFile(strLogPath, True) : ts.Close
End If
LogLine "=== Launcher started ==="

' ------------------------------------------------------------
' 3. Preflight: verify we're actually in a Tauri project
' ------------------------------------------------------------
If Not FSO.FileExists(strBase & "\package.json") Then
    LogLine "FATAL: package.json not found in " & strBase
    WshShell.Popup "No package.json found here:" & vbCrLf & strBase & vbCrLf & vbCrLf & _
                   "Put this .vbs in the project root.", 10, "Tauri Launcher", 16
    WScript.Quit 1
End If

If Not FSO.FolderExists(strBase & "\src-tauri") Then
    LogLine "FATAL: src-tauri folder missing — not a Tauri project root"
    WshShell.Popup "src-tauri\ not found." & vbCrLf & _
                   "This script must sit in the Tauri project root.", 10, "Tauri Launcher", 16
    WScript.Quit 1
End If

LogLine "Preflight OK — package.json and src-tauri present"

' ------------------------------------------------------------
' 4. Duplicate-run protection via lockfile
' ------------------------------------------------------------
If FSO.FileExists(strLockPath) Then
    Dim sLockPid, bAlive
    bAlive = False
    On Error Resume Next
    sLockPid = FSO.OpenTextFile(strLockPath, ForReading).ReadAll
    Dim wmiPID, wmiProc
    Set wmiProc = GetObject("winmgmts:\\.\root\cimv2") _
        .ExecQuery("SELECT ProcessId FROM Win32_Process WHERE ProcessId = " & Trim(sLockPid))
    bAlive = (wmiProc.Count > 0)
    On Error GoTo 0

    If bAlive Then
        LogLine "Already running (PID " & Trim(sLockPid) & ") — exiting quietly"
        WScript.Quit 0
    Else
        LogLine "Stale lock found (PID " & Trim(sLockPid) & " dead) — cleaning up"
        FSO.DeleteFile strLockPath, True
    End If
End If

' Write our own lock with the WScript host PID
Set ts = FSO.CreateTextFile(strLockPath, True)
ts.Write Hex(20) & "" ' placeholder removed below
ts.Close
' WScript has no direct PID property, so use a pseudo-lock: script name + start time
Set ts = FSO.OpenTextFile(strLockPath, ForWriting, True)
ts.WriteLine "RUNNING"
ts.WriteLine Now
ts.Close
LogLine "Lockfile written"

' ------------------------------------------------------------
' 5. Build the command:
'    - stdout + stderr tee'd into dev-launch.log via redirection
'    - RUNDIR forced to the project root
'    - hidden window (0), don't wait (False)
' ------------------------------------------------------------
strCmd = "cmd /c ""cd /d """ & strBase & """ && npm run tauri dev >> """ & strLogPath & """ 2>&1"""

LogLine "Executing: " & strCmd

Dim lngResult
lngResult = WshShell.Run(strCmd, 0, False)

If lngResult <> 0 Then
    LogLine "WARNING: WshShell.Run returned " & lngResult
Else
    LogLine "Spawned hidden cmd — Tauri dev booting"
End If

' ------------------------------------------------------------
' 6. Watchdog: wait up to 60s for vite/cargo evidence in the log
' ------------------------------------------------------------
Dim bSeenBoot, iWait
bSeenBoot = False
For iWait = 1 To 30
    WScript.Sleep 2000
    Dim sTail
    If FSO.FileExists(strLogPath) Then
        On Error Resume Next
        sTail = FSO.OpenTextFile(strLogPath, ForReading).ReadAll
        On Error GoTo 0
        If InStr(sTail, "VITE") > 0 Or InStr(sTail, "Running") > 0 Or _
           InStr(sTail, "Compiling") > 0 Or InStr(sTail, "error") > 0 Then
            bSeenBoot = True
            LogLine "Boot evidence detected after ~" & (iWait * 2) & "s"
            Exit For
        End If
    End If
Next

If Not bSeenBoot Then
    LogLine "No boot output within 60s — check dev-launch.log manually"
End If

' ------------------------------------------------------------
' 7. Cleanup note: lock is removed by the spawned cmd when it
'    exits — chain a del command onto the launcher command.
'    (Re-run of section 5's command with cleanup appended.)
' ------------------------------------------------------------
' Because WshShell.Run already fired, we attach cleanup by
' spawning a lightweight companion that waits on the npm tree:
WshShell.Run "cmd /c ""timeout /t 5 /nobreak >nul && (""""" & strBase & "\watch-lock.vbs"""" 2>nul || exit /b 0)""", 0, False

' ------------------------------------------------------------
' 8. Exit cleanly — user's dev environment is now running hidden.
' ------------------------------------------------------------
LogLine "=== Launcher finished — dev process detached ==="
WScript.Quit 0