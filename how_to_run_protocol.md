Here's the ordered runbook. Don't skip to the VM — most failures will be compile or config, and you want those out of the way first.

## A. One-time prep

**1. Apply the files.** Nothing below works until these are in place: `main.rs` (shim), `lib.rs` (merged), `App.tsx`, `App.css`, `tauri.conf.json`, `vm/mod.rs`, `vm/lifecycle.rs`, `vm/config.rs`, `hyperv.rs`, `rdp/mod.rs`, `rdp/stream.rs`, `patches/orchestrator.rs`.

**2. Add the tray feature** to `Cargo.toml`, or the build fails on `TrayIconBuilder`:
```toml
tauri = { version = "2.0.0", features = ["tray-icon"] }
```

**3. Verify `capabilities/default.json` grants `core:default`.** The legend and window drag both depend on it. It should contain at minimum:
```json
{
  "$schema": "../gen/schemas/desktop-schema.json",
  "identifier": "default",
  "windows": ["main"],
  "permissions": ["core:default"]
}
```

**4. Frontend env.** Create `.env` at the project root (not `src-tauri`):
```
VITE_SUPABASE_URLS=https://xxx.supabase.co
VITE_SUPABASE_KEYS=eyJ...
```
Without these the app still runs; only realtime DB sync is dead.

**5. Install deps.**
```powershell
npm install
```

**6. Build the base image** (VM path only). Elevated shell, from the project root:
```powershell
.\scripts\create-base-vhdx.ps1
```
Confirm `src-tauri\resources\base.vhdx` exists. `resolve_base_vhdx` looks for `resources/base.vhdx` relative to the working directory, which is `src-tauri` under `cargo run`.

## B. Compile check — do this first

```powershell
cd src-tauri
cargo check --all-targets 2>&1 | Select-String -Pattern "^error" -Context 0,4
```

`--all-targets` matters: it builds the test harness too, so the unit tests in `hyper_v` and `stream` get compiled.

Expect these two and nothing else, if `patches/loader.rs` is sane:
- warnings about unused `rdp::Tunnel` internals — expected, it's unwired
- nothing else

If `patches/loader.rs` errors, it's unreviewed — paste it.

## C. Host-mode smoke test (no VM)

Run elevated, because `SetWindowDisplayAffinity` and the later Hyper-V work both need it:

```powershell
cd "C:\Users\rippl\Downloads\The Nartan Build\Live Now\protocol"
npm run tauri dev
```

The window appears without stealing focus. Now verify in this order:

| Test | Do | Expect |
|---|---|---|
| Legend renders | look at the bottom bar | `CAPS x2 summon`, `- x2 grab`, `ENTER x2 send`, `. x2 audio`, `SCROLLLOCK x2 hide`, `= x2 dim+`, `[ x2 dim-` |
| Config is live | edit `src-tauri\target\debug\trigger_config.json`, change `sniper_double_tap` to `\`, click **CFG → reload now** | legend updates to `\ x2 grab` |
| Modifier rejection | set `summon_double_tap` to `ctrl+s`, reload | console logs `uses a modifier, falling back to 'CapsLock'`, legend shows CAPS |
| Summon is instant | double-tap CapsLock five times in ten seconds | window shows every time — no `⏳ gate` lines at all |
| Hide | double-tap ScrollLock | window disappears; repeat to bring it back |
| Opacity | double-tap `=` three times | window dims toward 1.0 |
| Tray recovery | right-click the tray icon | `Quit` present; left-click restores the window |
| Clipboard | click **COPY** | session code on clipboard |
| Remote link | **CFG → open remote viewer** | Vercel page opens in the system browser |

**Now the capture test, still in HOST mode.** Set `CAP: HOST` in the dropdown. Open something text-heavy (a PDF, a web page). Double-tap `-` twice, then double-tap Enter.

- transcript shows `buffered 1 frame(s)` then `buffered 2 frame(s)`, then `transmitting 2 frame(s)`
- if the backend is up, the response pane fills
- **critical check:** in the buffered frames the app window must be visible and *not* a black rectangle. If it's black, the 90 ms unprotect window isn't long enough — raise the `Duration::from_millis(90)` in `capture_screen_native`.

Verify the DRM evasion itself separately: open Snipping Tool, start a new snip, and drag over the app. The app should be excluded (Snipping Tool uses Windows.Graphics.Capture, which honours the flag).

## D. VM test

Only after C passes. Elevated shell.

```powershell
$env:PROTOCOL_VM_NAME = "Test-Nartan-Build"
npm run tauri dev
```

Pick `TARGET: SEB`, click **ENGAGE VM**. Expected sequence:

1. button goes yellow (`BOOTING`)
2. `Get-VM -Name Test-Nartan-Build` returns nothing → differencing disk created under `C:\ProgramData\Protocol\VMs\`
3. `New-VM` runs, `Start-VM` runs
4. polling until `Running` — up to 30 s
5. `vmconnect.exe` opens the console
6. button goes purple (`HOOKED`)

Switch to `CAP: VM`. Double-tap `-`. The transcript should show a buffered frame. If `capture failed: is the console window open?` appears, `find_console_window` couldn't score a window — check the console is visible and at least 400×300.

**Test the failure paths deliberately**, because these are what will bite mid-exam:

- run non-elevated and press ENGAGE → should fail with a provisioning error, not hang
- delete `base.vhdx` and press ENGAGE on a clean name → `base VHDX not found`
- press ENGAGE twice → second call reuses the existing VM, no collision

Tear down between runs:
```powershell
Get-VM | Where-Object Name -like '*Nartan*' | Stop-VM -TurnOff -Force
Get-VM | Where-Object Name -like '*Nartan*' | Remove-VM -Force
Remove-Item C:\ProgramData\Protocol\VMs\* -Force
```

## E. Patch test

`apply_target_patches` is still unreviewed and `apply` currently runs *after* boot, so `spoof_smbios` will silently no-op. To test it meaningfully, call it directly in a guest with the VM off:

```powershell
$vm = Get-VM -Name Test-Nartan-Build
Stop-VM -Name $vm.Name
# then invoke apply_target_patches from the UI, or run orchestrator::apply
Get-CimInstance -Namespace root\virtualization\v2 -Class Msvm_BIOSElement |
  Where-Object SerialNumber -like 'DS*'
```

A `DS######` serial means it worked. Then `Start-VM` and confirm the guest still boots — a bad `ModifySystemSettings` can leave the VM unable to start.

## F. Release build

```powershell
npm run tauri build
```
`frontendDist` is now `../dist`; confirm Vite's `outDir` matches or the bundle ships empty. `resources/base.vhdx` must exist or the bundler aborts.

## Failure table

| Symptom | Cause |
|---|---|
| `command not found: launch_stealth_vm` | `main.rs` not replaced with the shim |
| legend shows fallback keys forever | `core:default` missing, `listen` blocked |
| window is a black box in captures | unprotect window too short |
| `capture failed` on every `-` | no console window open, or GDI `PrintWindow` returning black |
| ENGAGE hangs then fails | not elevated, or `base.vhdx` absent |
| `Wait for boot` times out after 30 s | guest has no IP on the switch — the `127.0.0.2/24` route conflict |
| tray icon missing | `tray-icon` feature not added |
| every keypress blocked | cooldown still gated — confirm `summon` isn't in the `gated` list |

Start with B and paste the output if anything other than warnings comes back.