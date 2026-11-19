import os
import io
import json
import wave
import asyncio
import threading
import time
import websockets
import base64
import requests
import binascii
from PIL import Image
import speech_recognition as sr
from dotenv import load_dotenv
from supabase import create_client, Client

# =====================================================================
# DIAGNOSTIC NATIVE HOOKS
# =====================================================================
try:
    import keyboard
    from PIL import ImageGrab
    import pyaudiowpatch as pyaudio
    import mss
    import cv2
    import pyvirtualcam
    import pygetwindow as gw
except ImportError:
    print("[CLOUD MODE] Native OS libraries skipped. Waiting for Tauri payloads...")
    keyboard = None
    ImageGrab = None
    pyaudio = None
    mss = None
    cv2 = None
    pyvirtualcam = None
    gw = None

load_dotenv()

# =====================================================================
# SYSTEM & SECURITY CONFIGURATION
# =====================================================================
REQUIRE_AUTH = os.getenv("REQUIRE_AUTH", "false").lower() == "true"
AUTH_TOKEN = os.getenv("AUTH_TOKEN", "")

if REQUIRE_AUTH and not AUTH_TOKEN:
    raise ValueError("CRITICAL ERROR: REQUIRE_AUTH is true but AUTH_TOKEN is missing. Refusing to start unauthenticated production server.")

ALLOWED_ORIGINS_ENV = os.getenv("ALLOWED_ORIGINS", "")
ALLOWED_ORIGINS = [orig.strip() for orig in ALLOWED_ORIGINS_ENV.split(",") if orig.strip()] if ALLOWED_ORIGINS_ENV else None

# =====================================================================
# CLOUD DATABASE PIPELINE & FALLBACK ROUTING
# =====================================================================
SUPABASE_URLS_ENV = os.getenv("SUPABASE_URLS", os.getenv("SUPABASE_URL", ""))
SUPABASE_KEYS_ENV = os.getenv("SUPABASE_KEYS", os.getenv("SUPABASE_ANON_KEY", ""))

supabase_urls = [url.strip() for url in SUPABASE_URLS_ENV.split(",") if url.strip()]
supabase_keys = [key.strip() for key in SUPABASE_KEYS_ENV.split(",") if key.strip()]

supabase_clients = []
for url, key in zip(supabase_urls, supabase_keys):
    try:
        client: Client = create_client(url, key)
        supabase_clients.append(client)
    except Exception as e:
        print(f"[WARN] Failed to initialize DB client for {url}: {e}")

if not supabase_clients:
    print("[WARN] No valid database credentials found. Cloud uploads will fail.")

# =====================================================================
# GLOBAL STATE & MULTI-TENANT SESSION MANAGEMENT
# =====================================================================
sessions_ws = {}          # session_code -> set of connected websocket clients
session_states = {}       # session_code -> isolated state data for that session
local_active_session = "" # Tracks the primary local desktop session for hardware hooks
cam_spoof_active = threading.Event()

hooks_registered = False

def get_session_state(session_code):
    """Retrieves or initializes the isolated state for a specific room/session."""
    if session_code not in session_states:
        session_states[session_code] = {
            "buffer": [],
            "mode": "AUTO",
            "route": "ai",
            "is_recording": False,
            "stop_recording_event": threading.Event(),
            "config": {}
        }
    return session_states[session_code]

async def broadcast_to_ui(message_dict, session_code=None):
    """Safely broadcasts a message only to clients within the same session/room."""
    if not session_code:
        session_code = local_active_session
    if not session_code or session_code not in sessions_ws:
        return

    msg = json.dumps(message_dict)
    for ws in list(sessions_ws[session_code]):
        try:
            await ws.send(msg)
        except Exception:
            pass

# =====================================================================
# MULTI-SHOT SCREEN BATCHING (Double Tab OR Double Minus -> Enter)
# =====================================================================
tab_presses = []
minus_presses = []
cooldown_until = 0

def setup_keyboard_hooks(main_loop):
    """Registers keyboard hooks once globally for native local clients."""
    global hooks_registered
    if hooks_registered:
        return
    hooks_registered = True

    def handle_double_tap(press_history, key_name):
        global cooldown_until, local_active_session
        now = time.time()
        if now < cooldown_until:
            return

        press_history.append(now)
        if len(press_history) > 2:
            press_history.pop(0)

        if len(press_history) == 2 and (press_history[1] - press_history[0] <= 0.5):
            print(f"DOUBLE {key_name.upper()} DETECTED! Buffering screenshot...")
            press_history.clear()
            cooldown_until = now + 1.0

            asyncio.run_coroutine_threadsafe(
                buffer_screenshot(local_active_session),
                main_loop
            )

    def on_tab(event):
        if event.event_type == 'down':
            handle_double_tap(tab_presses, "tab")

    def on_minus(event):
        if event.event_type == 'down':
            handle_double_tap(minus_presses, "minus (-)")

    def on_enter(event):
        global cooldown_until, local_active_session
        if event.event_type == 'down':
            session_code = local_active_session
            if not session_code: return
            state = get_session_state(session_code)

            if len(state["buffer"]) > 0:
                now = time.time()
                if now < cooldown_until:
                    return
                cooldown_until = now + 3.0

                print(f"ENTER DETECTED! Sending {len(state['buffer'])} batched screenshot(s) to CLOUD...")
                asyncio.run_coroutine_threadsafe(
                    execute_screenshot_batch(
                        requested_mode=state["mode"],
                        session_code=session_code,
                        route=state["route"],
                    ),
                    main_loop
                )

    def on_up(event):
        global local_active_session
        if event.event_type == 'down':
            print("GLOBAL UP DETECTED (Stealth scroll)")
            asyncio.run_coroutine_threadsafe(
                broadcast_to_ui({"status": "nav_up"}, local_active_session),
                main_loop
            )

    def on_down(event):
        global local_active_session
        if event.event_type == 'down':
            print("GLOBAL DOWN DETECTED (Stealth scroll)")
            asyncio.run_coroutine_threadsafe(
                broadcast_to_ui({"status": "nav_down"}, local_active_session),
                main_loop
            )

    try:
        keyboard.hook_key('tab', on_tab)
        keyboard.hook_key('-', on_minus)
        keyboard.hook_key('enter', on_enter)
        keyboard.hook_key('up', on_up)
        keyboard.hook_key('down', on_down)
    except ImportError:
        print("[WARN] Keyboard module requires root/admin or is not supported in this environment.")
    except Exception as e:
        print(f"[WARN] Could not bind global hotkeys (Cloud environment?): {e}")

# =====================================================================
# MSB VIRTUAL CAMERA SPOOF
# =====================================================================
def virtual_camera_loop():
    if not cv2 or not pyvirtualcam: return
    video = cv2.VideoCapture("assets/loop.mp4")
    width, height = int(video.get(3)), int(video.get(4))

    with pyvirtualcam.Camera(width=width, height=height, fps=30, fmt=pyvirtualcam.PixelFormat.BGR) as cam:
        while cam_spoof_active.is_set():
            ret, frame = video.read()
            if not ret:
                video.set(cv2.CAP_PROP_POS_FRAMES, 0)
                continue
            cam.send(frame)
            cam.sleep_until_next_frame()

def get_vm_window_rect():
    windows = gw.getWindowsWithTitle("exam_stealth_vm") or gw.getWindowsWithTitle("Virtual Machine Connection")
    if windows:
        win = windows[0]
        return {"top": win.top, "left": win.left, "width": win.width, "height": win.height}
    return None

async def buffer_screenshot(session_code=None):
    session_code = session_code or local_active_session
    state = get_session_state(session_code)

    rect = get_vm_window_rect()
    if not rect:
        await broadcast_to_ui({"status": "error", "data": "vm window not found."}, session_code)
        return

    with mss.mss() as sct:
        monitor = {"top": rect["top"], "left": rect["left"], "width": rect["width"], "height": rect["height"]}
        sct_img = sct.grab(monitor)
        img = Image.frombytes("RGB", sct_img.size, sct_img.bgra, "raw", "BGRX")
        img.thumbnail((1280, 1280))
        state["buffer"].append(img)

        await broadcast_to_ui({
            "status": "loading",
            "transcript": f"optical air-gap buffered {len(state['buffer'])} frame(s) from vm."
        }, session_code)


# THE MULTI-DB CLOUD UPLOAD THREAD
def sync_upload_batch(images, session_code, batch_id):
    """Executes blocking cloud uploads on a background thread with multi-DB fallback routing."""
    if not supabase_clients:
        return

    for idx, img in enumerate(images):
        img_byte_arr = io.BytesIO()
        img.save(img_byte_arr, format='JPEG', quality=80)
        img_bytes = img_byte_arr.getvalue()

        file_name = f"{session_code}/{batch_id}_{idx}.jpg"
        upload_success = False

        for db_index, client in enumerate(supabase_clients):
            try:
                client.storage.from_("captures").upload(
                    file=img_bytes,
                    path=file_name,
                    file_options={"content-type": "image/jpeg"}
                )

                public_url = client.storage.from_("captures").get_public_url(file_name)

                client.table("copilot_sessions").insert({
                    "session_code": session_code,
                    "batch_id": batch_id,
                    "image_url": public_url
                }).execute()

                upload_success = True
                break

            except Exception as e:
                print(f"[DB FALLBACK] Node {db_index + 1} failed: {e}. Routing to next available database...")

        if not upload_success:
            print(f"[CRITICAL] All database nodes failed to process frame {file_name}.")

async def execute_screenshot_batch(requested_mode="AUTO", session_code=None, route="ai"):
    """Handles Native Tauri screenshot batch.

    `route` decides who answers:
      - "ai"   -> upload, then the model reply arrives on the socket
      - "frf"/"friend" -> upload only, and the session waits on an operator
                          override written to `copilot_sessions.answer_text`
    """
    if not session_code:
        session_code = local_active_session
    if not session_code:
        return

    state = get_session_state(session_code)

    if not state["buffer"]:
        return

    if not supabase_clients:
        await broadcast_to_ui({
            "status": "error",
            "data": "// CRITICAL ERROR: Database keys missing. Cloud uploads will fail.\n// Check .env configuration."
        }, session_code)
        state["buffer"].clear()
        return

    print(f"\n[UPLOAD INITIATED: {len(state['buffer'])} Images for Session {session_code} route={route}]")
    await broadcast_to_ui({
        "status": "loading",
        "transcript": f"Transmitting {len(state['buffer'])} screenshot(s)..."
    }, session_code)

    try:
        images_to_process = state["buffer"].copy()
        state["buffer"].clear()

        batch_id = f"batch_{int(time.time())}"

        await asyncio.to_thread(sync_upload_batch, images_to_process, session_code, batch_id)

        if route == "friend":
            await broadcast_to_ui({
                "status": "awaiting_friend",
                "data": "Frames with the operator.",
                "transcript": "awaiting operator reply",
                "active_line": None
            }, session_code)
        else:
            await broadcast_to_ui({
                "status": "success",
                "data": "Image(s) successfully transmitted.",
                "transcript": "multi-db sync complete",
                "active_line": None
            }, session_code)
    except Exception as e:
        state["buffer"].clear()
        await broadcast_to_ui({"status": "error", "data": f"Upload Failed: {str(e)}"}, session_code)

# =====================================================================
# AUDIO RECORDING ENGINE
# =====================================================================
def record_audio_sync(stop_event):
    CHUNK = 4096
    MAX_SECONDS = 30

    try:
        with pyaudio.PyAudio() as p:
            wasapi_info = None
            try:
                wasapi_info = p.get_default_wasapi_loopback()
            except OSError:
                print("[WARN] Default WASAPI failed. Hunting for active earphone loopback...")

            if not wasapi_info:
                for i in range(p.get_device_count()):
                    dev = p.get_device_info_by_index(i)
                    if dev.get("isLoopbackDevice", False):
                        print(f"[FOUND EARPHONE LOOPBACK]: {dev['name']}")
                        wasapi_info = dev
                        break

            if not wasapi_info:
                return "ERROR_SYSTEM"

            device_index = wasapi_info["index"]
            sample_rate = int(wasapi_info["defaultSampleRate"])
            channels = wasapi_info["maxInputChannels"]
            sample_size = p.get_sample_size(pyaudio.paInt16)

            stream = p.open(format=pyaudio.paInt16,
                            channels=channels,
                            rate=sample_rate,
                            input=True,
                            frames_per_buffer=CHUNK,
                            input_device_index=device_index)

            frames = []
            max_chunks = int(sample_rate / CHUNK * MAX_SECONDS)

            for _ in range(max_chunks):
                if stop_event.is_set():
                    break
                frames.append(stream.read(CHUNK))

            stream.stop_stream()
            stream.close()

            if not frames:
                return "ERROR_NO_SPEECH"

            wav_io = io.BytesIO()
            wf = wave.open(wav_io, 'wb')
            wf.setnchannels(channels)
            wf.setsampwidth(sample_size)
            wf.setframerate(sample_rate)
            wf.writeframes(b''.join(frames))
            wf.close()
            wav_io.seek(0)

            recognizer = sr.Recognizer()
            with sr.AudioFile(wav_io) as source:
                audio_data = recognizer.record(source)

            return recognizer.recognize_google(audio_data, language="en-US")

    except Exception as e:
        print(f"Audio Error (Check if headless/cloud environment): {e}")
        return "ERROR_SYSTEM"

# =====================================================================
# WORKFLOW HANDLERS
# =====================================================================
async def handle_audio_workflow(websocket, session_code):
    state = get_session_state(session_code)
    state["is_recording"] = True
    state["stop_recording_event"].clear()

    await broadcast_to_ui({
        "status": "loading",
        "transcript": "intercepting system audio... press the audio key to finish."
    }, session_code)

    transcript = await asyncio.to_thread(record_audio_sync, state["stop_recording_event"])
    state["is_recording"] = False

    if transcript.startswith("ERROR"):
        await broadcast_to_ui({
            "status": "error",
            "data": "Could not process audio. Ensure system sound is playing and environment supports loopback."
        }, session_code)
        return

    print(f"[TRANSCRIBED (Session {session_code})]: {transcript}")

    await broadcast_to_ui({
        "status": "success",
        "data": "Audio intercepted successfully.",
        "transcript": f"Heard: '{transcript}'",
    }, session_code)


# =====================================================================
# WEBSOCKET SERVER & CONFIG PROTOCOL
# =====================================================================
async def ws_handler(websocket):
    global local_active_session

    session_code = None
    client_type = None

    try:
        client_ip = websocket.remote_address[0] if websocket.remote_address else "unknown"
    except Exception:
        client_ip = "unknown"

    print(f"\n[NEW CONNECTION] From IP: {client_ip}")

    try:
        async for message in websocket:
            try:
                payload = json.loads(message)
                if not isinstance(payload, dict):
                    raise ValueError("Payload must be a JSON object")
            except (json.JSONDecodeError, ValueError):
                print("[WARN] Received malformed JSON from client.")
                await websocket.send(json.dumps({"status": "error", "data": "Malformed JSON payload."}))
                continue

            action = payload.get("action")

            if action == "set_session":
                req_session_code = payload.get("session_code", "")
                if not req_session_code or not isinstance(req_session_code, str):
                    await websocket.send(json.dumps({"status": "error", "data": "Invalid session_code."}))
                    continue

                if session_code is not None and session_code != req_session_code:
                    await websocket.send(json.dumps({"status": "error", "data": "Cannot switch sessions after authentication."}))
                    continue

                if REQUIRE_AUTH:
                    req_token = payload.get("auth_token", "")
                    if not AUTH_TOKEN or req_token != AUTH_TOKEN:
                        print(f"[UNAUTHORIZED] Failed auth attempt for session {req_session_code}")
                        await websocket.send(json.dumps({"status": "error", "data": "Unauthorized connection."}))
                        continue

                session_code = req_session_code
                requested_client_type = payload.get("client_type", "browser")

                if requested_client_type == "desktop":
                    if not REQUIRE_AUTH:
                        client_type = "desktop"
                    else:
                        client_type = "desktop" if payload.get("auth_token") == AUTH_TOKEN else "browser"
                else:
                    client_type = "browser"

                if session_code not in sessions_ws:
                    sessions_ws[session_code] = set()
                sessions_ws[session_code].add(websocket)

                if client_type == "desktop":
                    local_active_session = session_code
                    setup_keyboard_hooks(asyncio.get_running_loop())

                print(f"[SESSION LINKED] Assigned Code: {session_code} | Type: {client_type.upper()} | Total sessions: {len(sessions_ws)}")
                continue

            if not session_code:
                await websocket.send(json.dumps({"status": "error", "data": "Not authenticated. Send set_session first."}))
                continue

            msg_session = payload.get("session_code", session_code)
            if msg_session != session_code:
                await websocket.send(json.dumps({"status": "error", "data": "Session mismatch boundary violation."}))
                continue

            if action == "set_mode":
                get_session_state(session_code)["mode"] = payload.get("mode", "AUTO")

            elif action == "set_route":
                route = payload.get("route", "ai")
                get_session_state(session_code)["route"] = "friend" if route == "FRD" else "ai"

            elif action == "set_target_browser":
                target = payload.get("target", "SEB")
                state = get_session_state(session_code)
                state["mode"] = target
                if target == "MSB" and not cam_spoof_active.is_set():
                    cam_spoof_active.set()
                    threading.Thread(target=virtual_camera_loop, daemon=True).start()
                elif target == "SEB":
                    cam_spoof_active.clear()
                await broadcast_to_ui({"status": "idle", "transcript": f"backend target locked to {target}"}, session_code)

            elif action == "capture_screen":
                if client_type != "desktop":
                    await websocket.send(json.dumps({"status": "error", "data": "Action requires desktop client architecture."}))
                    continue

                state = get_session_state(session_code)
                mode = payload.get("mode", state["mode"])
                route = state["route"]

                async def force_single_capture(s_code, m, r):
                    await buffer_screenshot(s_code)
                    await execute_screenshot_batch(m, s_code, r)

                asyncio.create_task(force_single_capture(session_code, mode, route))

            elif action == "toggle_audio":
                if client_type != "desktop":
                    await websocket.send(json.dumps({"status": "error", "data": "Action requires desktop client architecture."}))
                    continue

                state = get_session_state(session_code)
                if not state["is_recording"]:
                    asyncio.create_task(handle_audio_workflow(websocket, session_code))
                else:
                    state["stop_recording_event"].set()

            elif action == "text_query":
                query_text = payload.get("text", "")
                if query_text and isinstance(query_text, str):
                    await broadcast_to_ui({
                        "status": "success",
                        "data": "Text query received.",
                        "transcript": f"Query: {query_text}",
                    }, session_code)

            elif action == "browser_image_batch_upload":
                images_data = payload.get("images", [])
                route = "friend" if payload.get("route") == "FRD" else "ai"

                if not images_data or not isinstance(images_data, list):
                    continue

                async def process_batch(imgs_strs, s_code, r):
                    await broadcast_to_ui({
                        "status": "loading",
                        "transcript": f"syncing {len(imgs_strs)} image(s) to {r}..."
                    }, s_code)
                    processed_images = []
                    try:
                        for img_str in imgs_strs:
                            parts = img_str.split(",", 1)
                            if len(parts) == 2:
                                img_bytes = base64.b64decode(parts[1], validate=True)
                                img_io = io.BytesIO(img_bytes)
                                img = Image.open(img_io)
                                img.load()
                                if img.mode != 'RGB':
                                    img = img.convert('RGB')
                                processed_images.append(img)

                        if processed_images:
                            batch_id = f"batch_{int(time.time())}"
                            await asyncio.to_thread(sync_upload_batch, processed_images, s_code, batch_id)

                            if r == "friend":
                                await broadcast_to_ui({
                                    "status": "awaiting_friend",
                                    "data": "Frames with the operator.",
                                    "transcript": "awaiting operator reply",
                                }, s_code)
                            else:
                                await broadcast_to_ui({
                                    "status": "success",
                                    "data": f"{len(processed_images)} Image(s) successfully transmitted.",
                                    "transcript": "multi-db synced (batch capture)",
                                }, s_code)
                    except Exception as e:
                        print(f"[BATCH UPLOAD ERROR] {str(e)}")
                        await broadcast_to_ui({"status": "error", "data": f"Batch upload failed: {str(e)}"}, s_code)
                    finally:
                        for img in processed_images:
                            img.close()

                asyncio.create_task(process_batch(images_data, session_code, route))

            elif action == "update_config":
                if REQUIRE_AUTH and client_type != "desktop":
                    await websocket.send(json.dumps({
                        "status": "error",
                        "data": "Only the authorized desktop client can update session configuration."
                    }))
                    continue

                await broadcast_to_ui({
                    "status": "config_updated",
                    "data": "Settings saved successfully!"
                }, session_code)

    except websockets.exceptions.ConnectionClosed:
        print(f"\n[UI DISCONNECTED] Session: {session_code}")
    except Exception as e:
        print(f"\n[UNEXPECTED WEBSOCKET ERROR] {e}")
    finally:
        if session_code and session_code in sessions_ws:
            if websocket in sessions_ws[session_code]:
                sessions_ws[session_code].remove(websocket)

            if not sessions_ws[session_code]:
                del sessions_ws[session_code]
                if session_code in session_states:
                    del session_states[session_code]
                if local_active_session == session_code and client_type == "desktop":
                    local_active_session = ""

async def start_server():
    host = os.getenv("WS_HOST", "0.0.0.0")
    PORT = int(os.environ.get("PORT", 8765))

    print(f"PROTOCOL V2 MULTI-DB ROUTER ACTIVE ON ws://{host}:{PORT}")

    if REQUIRE_AUTH:
        print("Authentication layer ENABLED for secure deployment.")
    else:
        print("Authentication layer DISABLED (Local Development Mode).")

    async with websockets.serve(
        ws_handler,
        host,
        PORT,
        max_size=50 * 1024 * 1024,
        origins=ALLOWED_ORIGINS
    ):
        await asyncio.Future()

if __name__ == "__main__":
    asyncio.run(start_server())