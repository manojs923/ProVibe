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
# CLOUD-SAFE NATIVE HOOKS
# =====================================================================
try:
    import keyboard
    from PIL import ImageGrab
    import pyaudiowpatch as pyaudio
except ImportError:
    print("[☁️ CLOUD MODE] Native OS libraries skipped. Waiting for Tauri payloads...")
    keyboard = None
    ImageGrab = None
    pyaudio = None

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
# SUPABASE CLOUD CONFIGURATION
# =====================================================================
SUPABASE_URL = os.getenv("SUPABASE_URL", "")
SUPABASE_KEY = os.getenv("SUPABASE_ANON_KEY", "")

if SUPABASE_URL and SUPABASE_KEY:
    supabase: Client = create_client(SUPABASE_URL, SUPABASE_KEY)
else:
    supabase = None
    print("[⚠️ WARNING] Supabase credentials missing from .env. Cloud uploads will fail.")

# =====================================================================
# GLOBAL STATE & MULTI-TENANT SESSION MANAGEMENT
# =====================================================================
sessions_ws = {}          # session_code -> set of connected websocket clients
session_states = {}       # session_code -> isolated state data for that session
local_active_session = "" # Tracks the primary local desktop session for hardware hooks

hooks_registered = False

def get_session_state(session_code):
    """Retrieves or initializes the isolated state for a specific room/session."""
    if session_code not in session_states:
        session_states[session_code] = {
            "buffer": [],
            "mode": "AUTO",
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
                    execute_screenshot_batch(requested_mode=state["mode"], session_code=session_code),
                    main_loop
                )

    def on_up(event):
        global local_active_session
        if event.event_type == 'down':
            print("⬆️ GLOBAL UP DETECTED (Stealth scroll)")
            asyncio.run_coroutine_threadsafe(
                broadcast_to_ui({"status": "nav_up"}, local_active_session),
                main_loop
            )

    def on_down(event):
        global local_active_session
        if event.event_type == 'down':
            print("⬇️ GLOBAL DOWN DETECTED (Stealth scroll)")
            asyncio.run_coroutine_threadsafe(
                broadcast_to_ui({"status": "nav_down"}, local_active_session),
                main_loop
            )

    # Register both Tab and Minus key hooks safely isolated to OS
    try:
        keyboard.hook_key('tab', on_tab)
        keyboard.hook_key('-', on_minus)
        keyboard.hook_key('enter', on_enter)
        keyboard.hook_key('up', on_up)
        keyboard.hook_key('down', on_down)
    except ImportError:
        print("[⚠️ WARNING] Keyboard module requires root/admin or is not supported in this environment.")
    except Exception as e:
        print(f"[⚠️ WARNING] Could not bind global hotkeys (Cloud environment?): {e}")

async def buffer_screenshot(session_code=None):
    if not session_code:
        session_code = local_active_session
    if not session_code: 
        return
        
    state = get_session_state(session_code)
    try:
        img = ImageGrab.grab()
        img.thumbnail((1280, 1280))
        
        save_dir = "screenshots"
        os.makedirs(save_dir, exist_ok=True)
        timestamp = int(time.time())
        file_path = os.path.join(save_dir, f"snipe_{timestamp}.jpg")
        
        img.save(file_path, format="JPEG", quality=75)
        
        state["buffer"].append(img)
        
        await broadcast_to_ui({
            "status": "loading",
            "transcript": f"📸 Buffered {len(state['buffer'])} screenshot(s). Scroll & Double-Tab again, or press [ENTER] to process..."
        }, session_code)
    except Exception as e:
        error_msg = f"Failed to buffer screenshot (Headless environment?): {e}"
        print(error_msg)
        await broadcast_to_ui({"status": "error", "data": error_msg}, session_code)

# 🛡️ THE NEW CLOUD UPLOAD THREAD
def sync_upload_batch(images, session_code, batch_id):
    """Executes blocking Supabase uploads on a background thread to prevent UI freezing."""
    if not supabase: return
    for idx, img in enumerate(images):
        img_byte_arr = io.BytesIO()
        img.save(img_byte_arr, format='JPEG', quality=80)
        img_bytes = img_byte_arr.getvalue()
        
        file_name = f"{session_code}/{batch_id}_{idx}.jpg"
        
        supabase.storage.from_("captures").upload(
            file=img_bytes,
            path=file_name,
            file_options={"content-type": "image/jpeg"}
        )
        
        public_url = supabase.storage.from_("captures").get_public_url(file_name)
        
        supabase.table("copilot_sessions").insert({
            "session_code": session_code,
            "batch_id": batch_id,
            "image_url": public_url
        }).execute()

async def execute_screenshot_batch(requested_mode="AUTO", session_code=None):
    """Handles Native Tauri screenshot batch: Syncs to cloud, AI bypassed."""
    if not session_code:
        session_code = local_active_session
    if not session_code: 
        return
        
    state = get_session_state(session_code)
    
    if not state["buffer"]:
        return
        
    if not supabase:
        await broadcast_to_ui({
            "status": "error", 
            "data": "// CRITICAL ERROR: Supabase keys missing. Cloud uploads will fail.\n// Check .env configuration."
        }, session_code)
        state["buffer"].clear()
        return

    print(f"\n[☁️ UPLOAD INITIATED: {len(state['buffer'])} Images for Session {session_code}]")
    await broadcast_to_ui({
        "status": "loading", 
        "transcript": f"Transmitting {len(state['buffer'])} screenshot(s) to Overseer..."
    }, session_code)
    
    try:
        images_to_process = state["buffer"].copy()
        state["buffer"].clear()
        
        batch_id = f"batch_{int(time.time())}"
        
        # 1. Offload synchronous network calls to background thread (Cloud Sync)
        await asyncio.to_thread(sync_upload_batch, images_to_process, session_code, batch_id)
        
        # AI Processing removed. Returning success immediately.
        await broadcast_to_ui({
            "status": "success", 
            "data": "Image(s) successfully transmitted to Overseer.",
            "transcript": "✅ Cloud Synced",
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
                print("[⚠️ WARNING] Default WASAPI failed. Hunting for active earphone loopback...")
                
            if not wasapi_info:
                for i in range(p.get_device_count()):
                    dev = p.get_device_info_by_index(i)
                    if dev.get("isLoopbackDevice", False):
                        print(f"[🔎 FOUND EARPHONE LOOPBACK]: {dev['name']}")
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
        "transcript": "🔴 Intercepting system audio... Press [ALT+A] to finish."
    }, session_code)
    
    transcript = await asyncio.to_thread(record_audio_sync, state["stop_recording_event"])
    state["is_recording"] = False
    
    if transcript.startswith("ERROR"):
        await broadcast_to_ui({
            "status": "error", 
            "data": "Could not process audio. Ensure system sound is playing and environment supports loopback."
        }, session_code)
        return

    print(f"[🗣️ TRANSCRIBED (Session {session_code})]: {transcript}")
    
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
        
    print(f"\n[🌐 NEW CONNECTION] From IP: {client_ip}")

    try:
        async for message in websocket:
            # 1. MESSAGE VALIDATION
            try:
                payload = json.loads(message)
                if not isinstance(payload, dict):
                    raise ValueError("Payload must be a JSON object")
            except (json.JSONDecodeError, ValueError):
                print("[⚠️ WARNING] Received malformed JSON from client.")
                await websocket.send(json.dumps({"status": "error", "data": "Malformed JSON payload."}))
                continue
                
            action = payload.get("action")
            
            # 2. SESSION & AUTH INITIALIZATION
            if action == "set_session":
                req_session_code = payload.get("session_code", "")
                if not req_session_code or not isinstance(req_session_code, str):
                    await websocket.send(json.dumps({"status": "error", "data": "Invalid session_code."}))
                    continue
                
                # Prevent session switching on an already bound socket
                if session_code is not None and session_code != req_session_code:
                    await websocket.send(json.dumps({"status": "error", "data": "Cannot switch sessions after authentication."}))
                    continue
                
                # Verify Authentication if enabled for production deployment
                if REQUIRE_AUTH:
                    req_token = payload.get("auth_token", "")
                    if not AUTH_TOKEN or req_token != AUTH_TOKEN:
                        print(f"[⚠️ UNAUTHORIZED] Failed auth attempt for session {req_session_code}")
                        await websocket.send(json.dumps({"status": "error", "data": "Unauthorized connection."}))
                        continue
                
                # Set session code and client type ONLY after successful authorization
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
                
                # Only permit native hooks for explicitly authenticated desktop clients
                if client_type == "desktop":
                    local_active_session = session_code
                    setup_keyboard_hooks(asyncio.get_running_loop())
                    
                print(f"[🔗 SESSION LINKED] Assigned Code: {session_code} | Type: {client_type.upper()} | Total sessions: {len(sessions_ws)}")
                continue

            # 3. AUTHORIZATION GUARD FOR ALL OTHER ACTIONS
            if not session_code:
                await websocket.send(json.dumps({"status": "error", "data": "Not authenticated. Send set_session first."}))
                continue
                
            # Verify session scope to prevent cross-session execution
            msg_session = payload.get("session_code", session_code)
            if msg_session != session_code:
                await websocket.send(json.dumps({"status": "error", "data": "Session mismatch boundary violation."}))
                continue

            # 4. ACTION ROUTING
            if action == "set_mode":
                get_session_state(session_code)["mode"] = payload.get("mode", "AUTO")
                
            elif action == "capture_screen":
                # NATIVE FUNCTIONALITY ISOLATION
                if client_type != "desktop":
                    await websocket.send(json.dumps({"status": "error", "data": "Action requires desktop client architecture."}))
                    continue
                    
                state = get_session_state(session_code)
                mode = payload.get("mode", state["mode"])
                async def force_single_capture(s_code, m):
                    await buffer_screenshot(s_code)
                    await execute_screenshot_batch(m, s_code)
                asyncio.create_task(force_single_capture(session_code, mode))
                
            elif action == "toggle_audio":
                # NATIVE FUNCTIONALITY ISOLATION
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
                    
            elif action == "browser_image_upload":
                image_data = payload.get("image_data")

                # Strict Validation
                if not image_data or not isinstance(image_data, str) or not image_data.startswith("data:image/"):
                    await websocket.send(json.dumps({"status": "error", "data": "Invalid or missing image data format."}))
                    continue

                async def process_browser_image(img_str, s_code):
                    await broadcast_to_ui({"status": "loading", "transcript": "📸 Syncing browser screen capture to Overseer..."}, s_code)
                    img = None
                    try:
                        # 1. Safely split data URL metadata from payload
                        parts = img_str.split(",", 1)
                        if len(parts) != 2:
                            raise ValueError("Malformed data URL.")
                        header, encoded = parts
                        
                        if not encoded:
                            raise ValueError("Empty image payload.")

                        # 2. Strict MIME type validation
                        mime_type = header.split(";")[0].replace("data:", "")
                        supported_mimes = ["image/jpeg", "image/png", "image/webp"]
                        if mime_type not in supported_mimes:
                            raise ValueError(f"Unsupported MIME type: {mime_type}. Expected JPEG, PNG, or WEBP.")
                        
                        # 3. Secure Base64 Decode with strict alphabet validation
                        try:
                            img_bytes = base64.b64decode(encoded, validate=True)
                        except binascii.Error:
                            raise ValueError("Invalid base64 encoding formatting.")

                        # 4. Enforce resource limits (e.g., maximum 15MB payload)
                        if len(img_bytes) > 15 * 1024 * 1024:
                            raise ValueError("Image payload exceeds maximum allowed size (15MB).")

                        # 5. Load Image for processing
                        img_io = io.BytesIO(img_bytes)
                        img = Image.open(img_io)
                        
                        img_format = img.format
                        # Force image loading/verification safely to catch truncation/corruption
                        img.load()
                        
                        supported_formats = ["JPEG", "PNG", "WEBP", "MPO"]
                        if img_format not in supported_formats:
                            raise ValueError(f"Unsupported internal image format: {img_format}. Expected JPEG, PNG, or WEBP.")

                        if img.mode != 'RGB':
                            converted_img = img.convert('RGB')
                            img.close()
                            img = converted_img

                        # Sync the image to Supabase so the Overseer Terminal can view it
                        batch_id = f"batch_{int(time.time())}"
                        await asyncio.to_thread(sync_upload_batch, [img], s_code, batch_id)

                        # AI Processing removed. Returning success immediately.
                        await broadcast_to_ui({
                            "status": "success",
                            "data": "Image successfully transmitted to Overseer.",
                            "transcript": "✅ Cloud Synced (Browser Capture)",
                        }, s_code)
                        
                    except Exception as e:
                        # Log error securely without exposing base64 payload
                        print(f"[⚠️ BROWSER IMAGE ERROR] Processing failed: {str(e)}")
                        await broadcast_to_ui({"status": "error", "data": f"Failed to process browser image: {str(e)}"}, s_code)
                    finally:
                        # Memory Safety: Explicitly release heavy PIL Image resources
                        if img is not None:
                            img.close()

                asyncio.create_task(process_browser_image(image_data, session_code))

            elif action == "update_config":
                if REQUIRE_AUTH and client_type != "desktop":
                    await websocket.send(json.dumps({
                         "status": "error",
                         "data": "Only the authorized desktop client can update session configuration."
                    }))
                    continue
                
                await broadcast_to_ui({
                    "status": "config_updated",
                    "data": "✅ Settings saved successfully!"
                }, session_code)
                
    except websockets.exceptions.ConnectionClosed:
        print(f"\n[⚠️ UI DISCONNECTED] Session: {session_code}")
    except Exception as e:
        print(f"\n[⚠️ UNEXPECTED WEBSOCKET ERROR] {e}")
    finally:
        # CONNECTION CLEANUP
        if session_code and session_code in sessions_ws:
            if websocket in sessions_ws[session_code]:
                sessions_ws[session_code].remove(websocket)
            
            # Free unused isolated state spaces
            if not sessions_ws[session_code]:
                del sessions_ws[session_code]
                if session_code in session_states:
                    del session_states[session_code]
                if local_active_session == session_code and client_type == "desktop":
                    local_active_session = ""

async def start_server():
    # Make host and port configurable for Cloud Deployment & Reverse Proxies
    host = os.getenv("WS_HOST", "0.0.0.0")
    PORT = int(os.environ.get("PORT", 8765))
    
    print(f"✅ PROTOCOL V2 SYNC ROUTER ACTIVE ON ws://{host}:{PORT}")
    
    if REQUIRE_AUTH:
         print("🔒 Authentication layer ENABLED for secure deployment.")
    else:
         print("🔓 Authentication layer DISABLED (Local Development Mode).")
         
    async with websockets.serve(
        ws_handler, 
        "0.0.0.0", 
        PORT,
        max_size=25 * 1024 * 1024,  # 25 MB max size for large browser images
        origins=ALLOWED_ORIGINS
    ):
        await asyncio.Future()

if __name__ == "__main__":
    asyncio.run(start_server())