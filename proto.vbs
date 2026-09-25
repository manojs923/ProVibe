Set WshShell = CreateObject("WScript.Shell")

' 1. Force the script to run in the exact folder where this .vbs file is located
WshShell.CurrentDirectory = CreateObject("Scripting.FileSystemObject").GetParentFolderName(WScript.ScriptFullName)

' 2. Execute the Tauri dev environment silently
WshShell.Run "cmd /c npm run tauri dev", 0, False