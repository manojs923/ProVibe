import os
import io
import json
import wave
import asyncio
import threading
import time
import websockets
from PIL import ImageGrab
import pyaudiowpatch as pyaudio
import speech_recognition as sr
from google import genai
from pypdf import PdfReader
from dotenv import load_dotenv
import keyboard
import requests
import base64

load_dotenv()

# =====================================================================
# MODEL FALLBACK CASCADES (Sanitized for Multimodal Free-Tier Stability)
# =====================================================================
ULTIMATE_FALLBACK = [
    "gemini-3.6-flash",
    "gemini-3.5-flash",
    "gemini-omni-flash-preview",
    "gemini-3.5-flash-lite",
    "gemini-3.1-flash-lite",
    "gemini-3.1-flash-lite-preview",
    "gemini-3-flash-preview",
    "gemini-2.5-flash",
    "gemini-2.5-flash-lite",
    "gemini-2.0-flash",
    "gemini-2.0-flash-001",
    "gemini-2.0-flash-lite",
    "gemini-2.0-flash-lite-001",
    "gemini-flash-latest",
    "gemini-flash-lite-latest"
    "nvidia/nemotron-nano-12b-v2-vl:free",      # Nvidia's free Vision-Language model
    "cohere/north-mini-code:free",              # Cohere's dedicated coding model
    "google/gemma-4-31b-it:free",               # Gemma 31B instruction-tuned
    "openai/gpt-oss-20b:free",                  # Free GPT endpoint
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
# GLOBAL STATE & USER VAULT (WITH KEY ROTATION)
# =====================================================================
env_keys = [k.strip() for k in os.getenv("GEMINI_API_KEY", "").split(",") if k.strip()]

user_config = {
    "api_keys": env_keys,
    "resume_context": "Candidate has experience in Software Engineering, Data Structures, Algorithms, and System Design."
}

is_recording = False
stop_recording_event = threading.Event()
current_mode = "AUTO"
hooks_registered = False
connected_clients = set()
current_key_index = 0

def get_genai_client(custom_key=None):
    """Instantiates Gemini client using the current active key in the rotation pool."""
    global current_key_index
    if custom_key:
        return genai.Client(api_key=custom_key)
        
    keys = user_config.get("api_keys", [])
    if not keys:
        raise ValueError("No Gemini API key provided. Paste one (or more, comma-separated) in the Vault.")
    
    active_key = keys[current_key_index % len(keys)]
    return genai.Client(api_key=active_key)

async def broadcast_to_ui(message_dict):
    """Safely broadcasts a message to all open React windows to beat Strict Mode."""
    if not connected_clients:
        return
    msg = json.dumps(message_dict)
    for ws in list(connected_clients):
        try:
            await ws.send(msg)
        except Exception:
            pass

# =====================================================================
# MULTI-SHOT SCREEN BATCHING (Double Tab -> Enter)
# =====================================================================
tab_presses = []
cooldown_until = 0
screenshot_buffer = []

def setup_keyboard_hooks(main_loop):
    """Registers keyboard hooks once globally to prevent memory leaks/crashes."""
    global hooks_registered
    if hooks_registered:
        return
    hooks_registered = True
    
    def on_tab(event):
        global tab_presses, cooldown_until
        if event.event_type == 'down':
            now = time.time()
            if now < cooldown_until:
                return
                
            tab_presses.append(now)
            if len(tab_presses) > 2:
                tab_presses.pop(0)
                
            if len(tab_presses) == 2 and (tab_presses[1] - tab_presses[0] <= 0.5):
                print("DOUBLE TAB DETECTED! Buffering screenshot...")
                tab_presses.clear()
                cooldown_until = now + 1.0 
                
                asyncio.run_coroutine_threadsafe(
                    buffer_screenshot(), 
                    main_loop
                )

    def on_enter(event):
        global screenshot_buffer, cooldown_until, current_mode
        if event.event_type == 'down':
            if len(screenshot_buffer) > 0:
                now = time.time()
                if now < cooldown_until:
                    return
                cooldown_until = now + 3.0
                
                print(f"ENTER DETECTED! Sending {len(screenshot_buffer)} batched screenshot(s) to AI...")
                asyncio.run_coroutine_threadsafe(
                    execute_screenshot_batch(requested_mode=current_mode),
                    main_loop
                )

    def on_up(event):
        if event.event_type == 'down':
            print("⬆️ GLOBAL UP DETECTED (Stealth scroll)")
            asyncio.run_coroutine_threadsafe(
                broadcast_to_ui({"status": "nav_up"}),
                main_loop
            )

    def on_down(event):
        if event.event_type == 'down':
            print("⬇️ GLOBAL DOWN DETECTED (Stealth scroll)")
            asyncio.run_coroutine_threadsafe(
                broadcast_to_ui({"status": "nav_down"}),
                main_loop
            )

    keyboard.hook_key('tab', on_tab)
    keyboard.hook_key('enter', on_enter)
    keyboard.hook_key('up', on_up)
    keyboard.hook_key('down', on_down)

async def buffer_screenshot():
    global screenshot_buffer
    try:
        img = ImageGrab.grab()
        img.thumbnail((1280, 1280))
        
        save_dir = "screenshots"
        os.makedirs(save_dir, exist_ok=True)
        timestamp = int(time.time())
        file_path = os.path.join(save_dir, f"snipe_{timestamp}.jpg")
        
        img.save(file_path, format="JPEG", quality=75)
        
        screenshot_buffer.append(img)
        
        await broadcast_to_ui({
            "status": "loading",
            "transcript": f"📸 Buffered {len(screenshot_buffer)} screenshot(s). Scroll & Double-Tab again, or press [ENTER] to solve..."
        })
    except Exception as e:
        print(f"Failed to buffer screenshot: {e}")

async def execute_screenshot_batch(requested_mode="AUTO"):
    global screenshot_buffer
    
    if not screenshot_buffer:
        return
        
    print(f"\n[📸 SCREEN CAPTURE BATCH TRIGGERED: {len(screenshot_buffer)} Images]")
    await broadcast_to_ui({
        "status": "loading", 
        "transcript": f"Analyzing {len(screenshot_buffer)} buffered screenshot(s)..."
    })
    
    try:
        task_type = "VISUAL" if requested_mode == "AUTO" else requested_mode
        
        images_to_process = screenshot_buffer.copy()
        screenshot_buffer.clear()
        
        contents = images_to_process + ["Analyze these images together as one continuous problem. Provide the exact solution according to the system prompt instructions."]
        
        answer, resolved_model = await execute_cascade_query(
            contents=contents,
            task_type=task_type
        )
        
        target_line = get_first_inner_code_line(answer)
        
        await broadcast_to_ui({
            "status": "success", 
            "data": answer,
            "transcript": f"Batch Solved ({len(images_to_process)} images) | Model: {resolved_model} | Mode: {task_type}",
            "active_line": target_line
        })
    except Exception as e:
        screenshot_buffer.clear()
        await broadcast_to_ui({"status": "error", "data": str(e)})

# =====================================================================
# SYSTEM PROMPT BUILDER (STRICT MODE)
# =====================================================================
def build_prompt(task_type="GENERAL"):
    base_rules = f"""
You are a stealth live-assessment copilot acting directly for the candidate taking the Amazon SDE Online Assessment.
CANDIDATE RESUME / BACKGROUND:
\"\"\"
{user_config['resume_context']}
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
# AI ROUTER WITH API KEY ROTATION & CASCADE FAILOVER
# =====================================================================
# Track indices for both providers independently
current_key_index = 0
current_or_key_index = 0

import os
import io
import base64
import asyncio
import requests

# Track indices for all three providers independently
current_key_index = 0
current_or_key_index = 0
current_poll_key_index = 0

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
    "openrouter/free"                           # OpenRouter auto-routing
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
# AI ROUTER WITH TRIPLE CROSS-PROVIDER FAILOVER
# =====================================================================
async def execute_cascade_query(contents, task_type="GENERAL", custom_key=None):
    global current_key_index, current_or_key_index, current_poll_key_index
    cascade_models = MODEL_TIERS.get(task_type, DEFAULT_FALLBACK)
    prompt = build_prompt(task_type)
    
    # 1. Parse Gemini keys array
    google_keys = user_config.get("api_keys", [])
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
                    client = get_genai_client(custom_key)
                    print(f"[🤖 GOOGLE ROUTER] Task '{task_type}' on model: {model_name} (Key #{actual_key_number})")
                    
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
            # Determine which external provider we are targeting
            if is_pollinations:
                active_keys = poll_keys
                total_ext_keys = total_poll_keys
                provider_name = "Pollinations AI"
                base_url = "https://gen.pollinations.ai/v1/chat/completions"
                # Strip the routing prefix to pass the actual model name (e.g., "openai")
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
                # Handle correct index rotation based on the provider
                if is_pollinations:
                    actual_ext_number = (current_poll_key_index % total_ext_keys) + 1
                    active_key = active_keys[current_poll_key_index % total_ext_keys]
                else:
                    actual_ext_number = (current_or_key_index % total_ext_keys) + 1
                    active_key = active_keys[current_or_key_index % total_ext_keys]
                
                try:
                    print(f"[🌐 EXTERNAL ROUTER] Task '{task_type}' on {provider_name}: {model_name} (Key #{actual_ext_number})")
                    
                    # Both providers use exact same OpenAI-compatible payload structure
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
def record_audio_sync():
    global stop_recording_event
    CHUNK = 4096
    MAX_SECONDS = 30
    
    with pyaudio.PyAudio() as p:
        try:
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
                if stop_recording_event.is_set():
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
            
        except sr.UnknownValueError:
            return "ERROR_NO_SPEECH"
        except Exception as e:
            print(f"Audio Error: {e}")
            return "ERROR_SYSTEM"

# =====================================================================
# DYNAMIC INTENT CLASSIFIER
# =====================================================================
async def classify_intent(transcript, custom_key=None):
    client = get_genai_client(custom_key)
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
    
    try:
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
async def handle_audio_workflow(websocket):
    global is_recording, stop_recording_event
    is_recording = True
    stop_recording_event.clear()
    
    await broadcast_to_ui({
        "status": "loading", 
        "transcript": "🔴 Intercepting system audio... Press [ALT+A] to finish."
    })
    
    transcript = await asyncio.to_thread(record_audio_sync)
    is_recording = False
    
    if transcript.startswith("ERROR"):
        await broadcast_to_ui({
            "status": "error", 
            "data": "Could not process audio. Ensure system sound is playing."
        })
        return

    print(f"[🗣️ TRANSCRIBED]: {transcript}")
    
    await broadcast_to_ui({
        "status": "loading", 
        "transcript": f"Heard: '{transcript}'\nClassifying intent..."
    })

    task_type = await classify_intent(transcript)
    
    await broadcast_to_ui({
        "status": "loading", 
        "transcript": f"Heard: '{transcript}'\nRouting through [{task_type}] cascade..."
    })

    try:
        answer, resolved_model = await execute_cascade_query(
            contents=f"Interviewer asked: {transcript}",
            task_type=task_type
        )
        
        target_line = get_first_inner_code_line(answer)

        await broadcast_to_ui({
            "status": "success", 
            "data": answer,
            "transcript": f"Heard: '{transcript}' | Solved by: {resolved_model}",
            "active_line": target_line
        })
    except Exception as e:
        await broadcast_to_ui({"status": "error", "data": str(e)})

# =====================================================================
# WEBSOCKET SERVER & CONFIG PROTOCOL
# =====================================================================
async def ws_handler(websocket):
    global is_recording, stop_recording_event, current_mode
    
    connected_clients.add(websocket)
    print(f"\n[🌐 REACT UI CONNECTED] Total Active Clients: {len(connected_clients)}")
    
    setup_keyboard_hooks(asyncio.get_running_loop())
    
    try:
        async for message in websocket:
            payload = json.loads(message)
            action = payload.get("action")
            
            if action == "set_mode":
                current_mode = payload.get("mode", "AUTO")
                
            elif action == "capture_screen":
                async def force_single_capture():
                    await buffer_screenshot()
                    await execute_screenshot_batch(payload.get("mode", "AUTO"))
                asyncio.create_task(force_single_capture())
                
            elif action == "toggle_audio":
                if not is_recording:
                    asyncio.create_task(handle_audio_workflow(websocket))
                else:
                    stop_recording_event.set()
                    
            elif action == "update_config":
                if "api_key" in payload and payload["api_key"]:
                    user_config["api_keys"] = [k.strip() for k in payload["api_key"].split(",") if k.strip()]
                if "resume_text" in payload and payload["resume_text"]:
                    user_config["resume_context"] = payload["resume_text"]
                    
                await broadcast_to_ui({
                    "status": "config_updated",
                    "data": "✅ Settings saved successfully! Vault & Resume updated."
                })
                
    except websockets.exceptions.ConnectionClosed:
        print("\n[⚠️ UI DISCONNECTED]")
    finally:
        connected_clients.remove(websocket)

async def start_server():
    print("✅ PROTOCOL V2 MULTI-MODEL ROUTER ACTIVE ON PORT 8765")
    async with websockets.serve(ws_handler, "localhost", 8765):
        await asyncio.Future()

if __name__ == "__main__":
    asyncio.run(start_server())