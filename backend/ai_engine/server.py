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
from google import genai
from pypdf import PdfReader
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
REQUIRE_AUTH = os.getenv("REQUIRE_AUTH", "true").lower() == "true"
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
# MODEL FALLBACK CASCADES (TRIPLE-PROVIDER TITANIUM SHIELD)
# =====================================================================
ULTIMATE_FALLBACK = [
    "gemini-2.5-flash",
    "gemini-2.0-flash",
    "gemini-2.0-flash-lite-preview-02-05",
    "nvidia/nemotron-nano-12b-v2-vl:free",      # OpenRouter
    "cohere/north-mini-code:free",              # OpenRouter
    "google/gemma-4-31b-it:free",               # OpenRouter
    "pollinations/openai",                      # Pollinations AI (GPT-4o proxy)
    "pollinations/gpt-oss",                     # Pollinations AI (Open Source fallback)
    "openrouter/free"                           # Dynamic auto-routing free tier
]

MODEL_TIERS = {
    "CODING": ULTIMATE_FALLBACK,
    "MCQ": ULTIMATE_FALLBACK,
    "SQL": ULTIMATE_FALLBACK,
    "BEHAVIORAL": ULTIMATE_FALLBACK,
    "VISUAL": ULTIMATE_FALLBACK,
    "AUDIO": ULTIMATE_FALLBACK
}

DEFAULT_FALLBACK = ULTIMATE_FALLBACK

# =====================================================================
# GLOBAL STATE & MULTI-TENANT SESSION MANAGEMENT
# =====================================================================
env_keys = [k.strip() for k in os.getenv("GEMINI_API_KEY", "").split(",") if k.strip()]

sessions_ws = {}          # session_code -> set of connected websocket clients
session_states = {}       # session_code -> isolated state data for that session
local_active_session = "" # Tracks the primary local desktop session for hardware hooks

hooks_registered = False

# Track indices for all three providers independently (Globally load-balanced)
current_key_index = 0
current_or_key_index = 0
current_poll_key_index = 0

def get_session_state(session_code):
    """Retrieves or initializes the isolated state for a specific room/session."""
    if session_code not in session_states:
        session_states[session_code] = {
            "buffer": [],
            "mode": "AUTO",
            "is_recording": False,
            "stop_recording_event": threading.Event(),
            "config": {
                "api_keys": env_keys.copy(),
                "resume_context": "Candidate has experience in Software Engineering, Data Structures, Algorithms, and System Design."
            }
        }
    return session_states[session_code]

def get_genai_client(keys, custom_key=None):
    """Instantiates Gemini client using the current active key in the rotation pool."""
    global current_key_index
    if custom_key:
        return genai.Client(api_key=custom_key)
        
    if not keys:
        raise ValueError("No Gemini API key provided. Paste one (or more, comma-separated) in the Vault.")
    
    active_key = keys[current_key_index % len(keys)]
    return genai.Client(api_key=active_key)

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
                
                print(f"ENTER DETECTED! Sending {len(state['buffer'])} batched screenshot(s) to CLOUD & AI...")
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
    """Handles Native Tauri screenshot batch: Syncs to cloud, then analyzes with existing AI pipeline."""
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

    print(f"\n[☁️ UPLOAD & AI PROCESSING INITIATED: {len(state['buffer'])} Images for Session {session_code}]")
    await broadcast_to_ui({
        "status": "loading", 
        "transcript": f"Transmitting & Analyzing {len(state['buffer'])} screenshot(s)..."
    }, session_code)
    
    try:
        images_to_process = state["buffer"].copy()
        state["buffer"].clear()
        
        batch_id = f"batch_{int(time.time())}"
        
        # 1. Offload synchronous network calls to background thread (Cloud Sync)
        await asyncio.to_thread(sync_upload_batch, images_to_process, session_code, batch_id)
        
        # 2. Existing AI Pipeline Integration (Processes visual data directly)
        answer, resolved_model = await execute_cascade_query(
            contents=images_to_process,
            task_type=requested_mode,
            session_code=session_code
        )
        target_line = get_first_inner_code_line(answer)
        
        await broadcast_to_ui({
            "status": "success", 
            "data": answer,
            "transcript": f"✅ Processed by: {resolved_model} (Cloud Synced)",
            "active_line": target_line
        }, session_code)
    except Exception as e:
        state["buffer"].clear()
        await broadcast_to_ui({"status": "error", "data": f"Upload/Processing Failed: {str(e)}"}, session_code)


# =====================================================================
# SYSTEM PROMPT BUILDER (STRICT MODE)
# =====================================================================
def build_prompt(task_type="GENERAL", resume_context=""):
    base_rules = f"""
You are a stealth live-assessment copilot acting directly for the candidate taking the Amazon SDE Online Assessment.
CANDIDATE RESUME / BACKGROUND:
\"\"\"
{resume_context}
\"\"\"

CRITICAL FRAMEWORK: 
Amazon grades situational and style sections strictly against their 16 Leadership Principles. 
Always prioritize: Customer Obsession, Ownership, Deliver Results, and Insist on Highest Standards.

CRITICAL AUDIO CORRECTION RULE:
The input is transcribed from live audio and may contain severe speech-to-text errors. Infer what standard interview question or scenario was actually asked.

ABSOLUTE OUTPUT CONSTRAINTS (VIOLATION IS A SYSTEM FAILURE):
1. NO EXPLANATIONS. NO FILLER. NO PLEASANTRIES.
2. DO NOT say "Here is the answer" or "The right choice is".
3. Provide ONLY the exact words to speak, the code to type, or the option to click.
"""

    if task_type == "WORK_SIMULATION":
        return base_rules + """
TASK: WORK SIMULATION (SITUATIONAL JUDGMENT)
INSTRUCTIONS: 
Evaluate the scenario using Amazon Leadership Principles. 
- If balancing speed vs. quality, lean towards quality/standards but escalate blockers immediately.
- If a customer issue arises, drop everything to prioritize the customer.
- Never choose options that involve passing the blame or ignoring a broken process.
OUTPUT FORMAT:
Provide ONLY the text of the best action to take, or rank the options EXACTLY as requested (e.g., 1. Option C, 2. Option A).
"""

    elif task_type == "WORK_STYLE":
        return base_rules + """
TASK: WORK STYLE SURVEY
INSTRUCTIONS: 
The user is presented with two statements on a slider. Pick the statement that best aligns with Amazon Leadership Principles (Action-oriented, data-driven, customer-focused, takes extreme ownership).
OUTPUT FORMAT: 
Output EXACTLY one of these phrases based on how strongly it maps to an LP:
- "Strongly choose [Statement A text]"
- "Mostly choose [Statement A text]"
- "Strongly choose [Statement B text]"
- "Mostly choose [Statement B text]"
"""

    elif task_type in ["CODING", "VISUAL"]:
        return base_rules + """
TASK: CODING CHALLENGE & VISUAL DEBUGGING
INSTRUCTIONS:
First, analyze the screenshots or input to determine the current state. 

STATE 1: FAILING TEST CASES DETECTED
If the screenshot shows execution errors, compiler errors, or failing test cases from a previous attempt, DO NOT output the full code again. 
OUTPUT FORMAT (Use EXACTLY this format):
// this was the coding mistake because of which test cases are not passing here is the updated, these things you have to update, not writing the full code

// in [filename.ext] update line [number] to:
[only the corrected lines of code]


STATE 2: NEW PROBLEM OR NO TEST CASES VISIBLE
If it is a new problem or test cases are not visible, solve it entirely.
OUTPUT FORMAT:
1. TRADITIONAL ALGORITHM: Provide ONLY the raw, optimal C++ code inside a markdown block. ABSOLUTELY NO COMMENTS IN THE CODE.
2. REPOSITORY / MULTI-FILE TASK: Use EXACTLY this format using comments for file routing:

// [Brief 1-sentence note of what to do]

// in [filename.ext] add this code:
[raw code here]

// in [filename.ext] this is the code:
[raw code here]
"""

    elif task_type == "SQL":
        return base_rules + """
TASK: DATABASE / SQL QUERY
OUTPUT FORMAT:
Provide ONLY the raw SQL query inside a markdown block. NO explanations. NO comments.
"""

    elif task_type == "BEHAVIORAL":
        return base_rules + """
TASK: BEHAVIORAL / CONCEPTUAL INTERVIEW
OUTPUT FORMAT:
Write a 3-4 sentence script in the FIRST PERSON ("I", "my") using the STAR method based ONLY on the resume provided. DO NOT explain the logic.
"""

    else:
        return base_rules + """
TASK: GENERAL TECHNICAL / INTERVIEW QUESTION
OUTPUT FORMAT:
Give the direct, raw answer. No filler.
"""

# =====================================================================
# AI ROUTER WITH TRIPLE CROSS-PROVIDER FAILOVER
# =====================================================================
async def execute_cascade_query(contents, task_type="GENERAL", custom_key=None, session_code=None):
    global current_key_index, current_or_key_index, current_poll_key_index
    cascade_models = MODEL_TIERS.get(task_type, DEFAULT_FALLBACK)
    
    # Retrieve session-specific resume config
    state = get_session_state(session_code)
    prompt = build_prompt(task_type, state["config"].get("resume_context", ""))
    
    # 1. Parse Gemini keys array (Session Specific falling back to Env)
    google_keys = state["config"].get("api_keys", [])
    if not google_keys:
        env_gkeys = os.getenv("GEMINI_API_KEY", "")
        google_keys = [k.strip() for k in env_gkeys.split(",") if k.strip()]
    total_g_keys = max(1, len(google_keys))

    # 2. Parse OpenRouter keys array
    env_orkeys = os.getenv("OPENROUTER_API_KEY", "")
    or_keys = [k.strip() for k in env_orkeys.split(",") if k.strip()]
    total_or_keys = max(1, len(or_keys))

    # 3. Parse Pollinations keys array
    env_pollkeys = os.getenv("POLLINATIONS_API_KEY", "")
    poll_keys = [k.strip() for k in env_pollkeys.split(",") if k.strip()]
    total_poll_keys = max(1, len(poll_keys))
    
    # Extract images and text strings from contents
    images = [c for c in contents if hasattr(c, 'convert')] if isinstance(contents, list) else []
    text_parts = [c for c in contents if isinstance(c, str)] if isinstance(contents, list) else [contents]
    full_text_prompt = prompt + "\n" + "\n".join(text_parts)
    
    for model_name in cascade_models:
        is_openrouter = "/" in model_name and not model_name.startswith("pollinations/")
        is_pollinations = model_name.startswith("pollinations/")
        
        # ---------------------------------------------------------
        # TIER 1: NATIVE GOOGLE MODEL ROTATION
        # ---------------------------------------------------------
        if not is_openrouter and not is_pollinations:
            for _ in range(total_g_keys):
                actual_key_number = (current_key_index % total_g_keys) + 1
                try:
                    client = get_genai_client(google_keys, custom_key)
                    print(f"[🤖 GOOGLE ROUTER] Task '{task_type}' on model: {model_name} (Key #{actual_key_number}) for session {session_code}")
                    
                    full_contents = [prompt] + (contents if isinstance(contents, list) else [contents])
                    response = client.models.generate_content(
                        model=model_name,
                        contents=full_contents
                    )
                    
                    if response and response.text:
                        print(f"[✅ SUCCESS] Handled by Google: {model_name}")
                        return response.text.strip(), model_name
                        
                except Exception as e:
                    error_msg = str(e)
                    if "429" in error_msg or "RESOURCE_EXHAUSTED" in error_msg:
                        print(f"[⚠️ RATE LIMIT] Gemini Key #{actual_key_number} exhausted. Swapping...")
                        if total_g_keys > 1:
                            current_key_index += 1
                            await asyncio.sleep(0.1)
                            continue
                        
                    print(f"[⚠️ GOOGLE FAILOVER] {model_name} failed: {error_msg}. Cascading...")
                    await asyncio.sleep(0.2)
                    break 
                    
        # ---------------------------------------------------------
        # TIER 2 & 3: EXTERNAL ROUTING (OPENROUTER & POLLINATIONS)
        # ---------------------------------------------------------
        else:
            if is_pollinations:
                active_keys = poll_keys
                total_ext_keys = total_poll_keys
                provider_name = "Pollinations AI"
                base_url = "https://gen.pollinations.ai/v1/chat/completions"
                api_model_name = model_name.replace("pollinations/", "")
            else:
                active_keys = or_keys
                total_ext_keys = total_or_keys
                provider_name = "OpenRouter"
                base_url = "https://openrouter.ai/api/v1/chat/completions"
                api_model_name = model_name

            if not active_keys:
                print(f"[⚠️ SKIP] Skipping {model_name}: No API key found for {provider_name} in .env")
                continue
                
            for _ in range(total_ext_keys):
                if is_pollinations:
                    actual_ext_number = (current_poll_key_index % total_ext_keys) + 1
                    active_key = active_keys[current_poll_key_index % total_ext_keys]
                else:
                    actual_ext_number = (current_or_key_index % total_ext_keys) + 1
                    active_key = active_keys[current_or_key_index % total_ext_keys]
                
                try:
                    print(f"[🌐 EXTERNAL ROUTER] Task '{task_type}' on {provider_name}: {model_name} (Key #{actual_ext_number}) for session {session_code}")
                    
                    messages_content = [{"type": "text", "text": full_text_prompt}]
                    
                    for img in images:
                        buffered = io.BytesIO()
                        img.convert("RGB").save(buffered, format="JPEG")
                        img_str = base64.b64encode(buffered.getvalue()).decode("utf-8")
                        messages_content.append({
                            "type": "image_url",
                            "image_url": {"url": f"data:image/jpeg;base64,{img_str}"}
                        })

                    headers = {
                        "Authorization": f"Bearer {active_key}",
                        "Content-Type": "application/json"
                    }
                    payload = {
                        "model": api_model_name,
                        "messages": [{"role": "user", "content": messages_content}]
                    }
                    
                    response = await asyncio.to_thread(
                        requests.post, 
                        base_url, 
                        headers=headers, 
                        json=payload,
                        timeout=15
                    )
                    response.raise_for_status()
                    data = response.json()
                    
                    answer_text = data["choices"][0]["message"]["content"]
                    if answer_text:
                        print(f"[✅ SUCCESS] Handled externally by: {model_name}")
                        return answer_text.strip(), model_name
                        
                except Exception as e:
                    error_msg = str(e)
                    if "429" in error_msg:
                        print(f"[⚠️ RATE LIMIT] {provider_name} Key #{actual_ext_number} exhausted. Swapping keys...")
                        if total_ext_keys > 1:
                            if is_pollinations:
                                current_poll_key_index += 1
                            else:
                                current_or_key_index += 1
                            await asyncio.sleep(0.1)
                            continue
                            
                    print(f"[⚠️ EXTERNAL FAILOVER] {provider_name} model {model_name} failed: {e}. Cascading...")
                    await asyncio.sleep(0.2)
                    break

    raise RuntimeError(f"All models across all three providers (Google, OpenRouter, Pollinations) failed for task '{task_type}'.")

def get_first_inner_code_line(code_text: str) -> int:
    lines = code_text.split('\n')
    for idx, line in enumerate(lines, start=1):
        stripped = line.strip()
        if not stripped or stripped.startswith("```") or stripped.startswith("//") or stripped.startswith("#") or stripped == "{":
            continue
        if line.startswith(" ") or line.startswith("\t"):
            return idx
    return 1

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
# DYNAMIC INTENT CLASSIFIER
# =====================================================================
async def classify_intent(transcript, keys, custom_key=None):
    try:
        client = get_genai_client(keys, custom_key)
        classifier_model = "gemini-1.5-flash-8b" 
        
        prompt = f"""
Analyze the following user input and categorize it into EXACTLY ONE of the following tags:
[BEHAVIORAL] - For questions about experience, past situations, or soft skills.
[CODING] - For algorithmic problems, data structures, or code repository tasks.
[WORK_SIMULATION] - For scenario-based questions about trade-offs, project timelines, or colleague interactions.
[WORK_STYLE] - For personality sliders or "how do you approach work" statements.
[SQL] - For database queries.
[GENERAL] - For technical trivia or anything that doesn't fit above.

USER INPUT: "{transcript}"

OUTPUT ONLY THE TAG INSIDE BRACKETS. DO NOT OUTPUT ANY OTHER TEXT.
"""
        
        response = client.models.generate_content(
            model=classifier_model,
            contents=[prompt]
        )
        tag = response.text.strip().upper()
        
        valid_tags = ["[BEHAVIORAL]", "[CODING]", "[SQL]", "[MCQ]", "[GENERAL]"]
        if tag in valid_tags:
            return tag.strip("[]")
        else:
            print(f"[⚠️ CLASSIFIER] Unexpected output: {tag}. Defaulting to GENERAL.")
            return "GENERAL"
            
    except Exception as e:
        print(f"[⚠️ CLASSIFIER FAILED]: {e}. Defaulting to GENERAL.")
        return "GENERAL"

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
        "status": "loading", 
        "transcript": f"Heard: '{transcript}'\nClassifying intent..."
    }, session_code)

    keys = state["config"].get("api_keys", [])
    task_type = await classify_intent(transcript, keys)
    
    await broadcast_to_ui({
        "status": "loading", 
        "transcript": f"Heard: '{transcript}'\nRouting through [{task_type}] cascade..."
    }, session_code)

    try:
        answer, resolved_model = await execute_cascade_query(
            contents=f"Interviewer asked: {transcript}",
            task_type=task_type,
            session_code=session_code
        )
        
        target_line = get_first_inner_code_line(answer)

        await broadcast_to_ui({
            "status": "success", 
            "data": answer,
            "transcript": f"Heard: '{transcript}' | Solved by: {resolved_model}",
            "active_line": target_line
        }, session_code)
    except Exception as e:
        await broadcast_to_ui({"status": "error", "data": str(e)}, session_code)

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
                mode = get_session_state(session_code)["mode"]
                if query_text and isinstance(query_text, str):
                    async def process_text(text, m, s_code):
                        await broadcast_to_ui({"status": "loading", "transcript": f"🤖 AI Processing: {text}"}, s_code)
                        try:
                            answer, resolved_model = await execute_cascade_query(contents=text, task_type=m, session_code=s_code)
                            target_line = get_first_inner_code_line(answer)
                            await broadcast_to_ui({
                                "status": "success", 
                                "data": answer, 
                                "transcript": f"🤖 Solved by: {resolved_model}", 
                                "active_line": target_line
                            }, s_code)
                        except Exception as e:
                            await broadcast_to_ui({"status": "error", "data": str(e)}, s_code)
                    asyncio.create_task(process_text(query_text, mode, session_code))
                    
            elif action == "browser_image_upload":
                image_data = payload.get("image_data")
                mode = payload.get("mode", get_session_state(session_code)["mode"])

                # Strict Validation
                if not image_data or not isinstance(image_data, str) or not image_data.startswith("data:image/"):
                    await websocket.send(json.dumps({"status": "error", "data": "Invalid or missing image data format."}))
                    continue

                async def process_browser_image(img_str, m, s_code):
                    await broadcast_to_ui({"status": "loading", "transcript": "📸 Analyzing browser screen capture..."}, s_code)
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

                        # 5. Connect to existing AI visual processing pipeline
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

                        answer, resolved_model = await execute_cascade_query(contents=[img], task_type=m, session_code=s_code)
                        target_line = get_first_inner_code_line(answer)

                        await broadcast_to_ui({
                            "status": "success",
                            "data": answer,
                            "transcript": f"🤖 Solved by: {resolved_model} (Browser Capture)",
                            "active_line": target_line
                        }, s_code)
                        
                    except Exception as e:
                        # Log error securely without exposing base64 payload
                        print(f"[⚠️ BROWSER IMAGE ERROR] Processing failed: {str(e)}")
                        await broadcast_to_ui({"status": "error", "data": f"Failed to process browser image: {str(e)}"}, s_code)
                    finally:
                        # Memory Safety: Explicitly release heavy PIL Image resources
                        if img is not None:
                            img.close()

                asyncio.create_task(process_browser_image(image_data, mode, session_code))

            elif action == "update_config":
                if REQUIRE_AUTH and client_type != "desktop":
                    await websocket.send(json.dumps({
                         "status": "error",
                         "data": "Only the authorized desktop client can update session configuration."
                    }))
                    continue
                state = get_session_state(session_code)
                if "api_key" in payload and isinstance(payload["api_key"], str):
                    state["config"]["api_keys"] = [k.strip() for k in payload["api_key"].split(",") if k.strip()]
                if "resume_text" in payload and isinstance(payload["resume_text"], str):
                    state["config"]["resume_context"] = payload["resume_text"]
                    
                await broadcast_to_ui({
                    "status": "config_updated",
                    "data": "✅ Settings saved successfully! Vault & Resume updated."
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
    port = int(os.getenv("PORT", os.getenv("WS_PORT", "8765")))
    
    print(f"✅ PROTOCOL V2 MULTI-MODEL ROUTER ACTIVE ON ws://{host}:{port}")
    if REQUIRE_AUTH:
         print("🔒 Authentication layer ENABLED for secure deployment.")
    else:
         print("🔓 Authentication layer DISABLED (Local Development Mode).")
         
    async with websockets.serve(
        ws_handler, 
        host, 
        port, 
        max_size=25 * 1024 * 1024,  # 25 MB max size for large browser images
        origins=ALLOWED_ORIGINS
    ):
        await asyncio.Future()

if __name__ == "__main__":
    asyncio.run(start_server())