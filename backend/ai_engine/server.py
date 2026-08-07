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

load_dotenv()

# =====================================================================
# MODEL FALLBACK CASCADES (Sanitized for Multimodal Free-Tier Stability)
# =====================================================================
MODEL_TIERS = {
    "CODING": [
        "gemini-2.5-flash",
        "gemini-2.0-flash",
        "gemini-1.5-pro-latest",
        "gemini-1.5-flash-latest"
    ],
    "MCQ": [
        "gemini-2.5-flash",
        "gemini-2.0-flash-lite-preview-02-05",
        "gemini-2.0-flash",
        "gemini-1.5-flash-8b-latest",
        "gemini-1.5-flash-latest"
    ],
    "SQL": [
        "gemini-2.5-flash",
        "gemini-2.0-flash",
        "gemini-1.5-pro-latest",
        "gemini-1.5-flash-latest"
    ],
    "BEHAVIORAL": [
        "gemini-2.5-flash",
        "gemini-2.0-flash",
        "gemini-1.5-pro-latest",
        "gemini-1.5-flash-latest"
    ],
    "VISUAL": [
        "gemini-2.5-flash",
        "gemini-2.0-flash",
        "gemini-1.5-pro-latest",
        "gemini-1.5-flash-latest"
    ],
    "AUDIO": [
        "gemini-2.5-flash",
        "gemini-2.0-flash",
        "gemini-1.5-pro-latest",
        "gemini-1.5-flash-latest"
    ]
}

DEFAULT_FALLBACK = [
    "gemini-2.5-flash",
    "gemini-2.0-flash",
    "gemini-1.5-pro-latest",
    "gemini-1.5-flash-latest",
    "gemini-1.5-flash-8b-latest"
]

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
You are a stealth live-interview copilot acting directly for the candidate.
CANDIDATE RESUME / BACKGROUND:
\"\"\"
{user_config['resume_context']}
\"\"\"

CRITICAL AUDIO CORRECTION RULE:
The input is transcribed from live audio and may contain severe speech-to-text errors (e.g., hearing "kill yourself" instead of "tell me about yourself"). 
If the prompt sounds nonsensical, hostile, or grammatically broken, USE YOUR CONTEXTUAL LOGIC to infer what standard software engineering interview question was actually asked, and answer THAT question.

ABSOLUTE OUTPUT CONSTRAINTS (VIOLATION IS A SYSTEM FAILURE):
1. NO EXPLANATIONS. NO FILLER. NO PLEASANTRIES.
2. DO NOT say "Here is the answer" or "The right choice is".
3. Provide ONLY the exact words I need to speak out loud, or the exact code I need to type.
"""

    if task_type == "MCQ":
        return base_rules + """
TASK: MULTIPLE CHOICE QUESTION (MCQ)
OUTPUT FORMAT:
Provide ONLY the correct option letter and its exact text. 
Example Output: B - Hash Table
"""
    elif task_type == "CODING":
        return base_rules + """
TASK: CODING INTERVIEW QUESTION OR DEBUGGING
OUTPUT FORMAT:
1. IF WRITING NEW CODE FROM SCRATCH: Provide ONLY the raw, optimal code inside a markdown block. DO NOT explain the code. ABSOLUTELY NO COMMENTS IN THE CODE (No //, no #, no /* */). I need the direct, clean code block only.
2. IF FIXING AN ERROR / DEBUGGING: Point out the exact line number, briefly explain what is wrong, and provide ONLY the fixed line(s) of code inside a markdown block.

Example Debug Output:
Line 23 has no semicolon. It should be:
```cpp
return;
```
"""

    elif task_type == "SQL":
        return base_rules + """
TASK: DATABASE / SQL QUERY
OUTPUT FORMAT:
Provide ONLY the raw SQL query inside a markdown block.
NO explanations. NO comments in the SQL.
"""
    elif task_type == "BEHAVIORAL":
        return base_rules + """
TASK: BEHAVIORAL / CONCEPTUAL INTERVIEW
OUTPUT FORMAT:
Write a 3-4 sentence script in the FIRST PERSON ("I", "my") that I can read out loud directly to the interviewer.
Use the STAR method based ONLY on the resume provided.
DO NOT explain the logic. ONLY give me the script to read.
"""
    else:
        return base_rules + """
TASK: GENERAL TECHNICAL / INTERVIEW QUESTION
OUTPUT FORMAT:
Give the direct, raw answer. If it's a concept, give me a 2-sentence script to read out loud. No filler.
"""

# =====================================================================
# AI ROUTER WITH API KEY ROTATION & CASCADE FAILOVER
# =====================================================================
async def execute_cascade_query(contents, task_type="GENERAL", custom_key=None):
    global current_key_index
    cascade_models = MODEL_TIERS.get(task_type, DEFAULT_FALLBACK)
    prompt = build_prompt(task_type)
    
    full_contents = [prompt] + (contents if isinstance(contents, list) else [contents])
    
    total_keys = len(user_config.get("api_keys", []))
    if total_keys == 0: total_keys = 1
    
    for model_name in cascade_models:
        for _ in range(max(1, total_keys)):
            try:
                client = get_genai_client(custom_key)
                print(f"[🤖 ROUTER] Attempting task '{task_type}' on model: {model_name} (Key #{current_key_index + 1})")
                
                response = client.models.generate_content(
                    model=model_name,
                    contents=full_contents
                )
                
                if response and response.text:
                    print(f"[✅ SUCCESS] Handled by model: {model_name}")
                    return response.text.strip(), model_name
                    
            except Exception as e:
                error_msg = str(e)
                if "429" in error_msg or "RESOURCE_EXHAUSTED" in error_msg:
                    print(f"[⚠️ RATE LIMIT HIT] Key #{current_key_index + 1} exhausted on {model_name}. Swapping keys...")
                    if total_keys > 1:
                        current_key_index += 1
                        await asyncio.sleep(0.1)
                        continue
                    
                print(f"[⚠️ FAILOVER] Model {model_name} failed: {error_msg}. Cascading to next model...")
                await asyncio.sleep(0.2)
                break 

    raise RuntimeError(f"All models in cascade for task '{task_type}' failed.")

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
    [BEHAVIORAL] - For questions about experience, past situations, soft skills, or "tell me about yourself".
    [CODING] - For algorithmic problems, data structures, debugging, or writing functions.
    [SQL] - For database queries, joins, or table schema questions.
    [MCQ] - For multiple choice questions or identifying the correct option from a list.
    [GENERAL] - For general technical trivia or anything that doesn't fit above.

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