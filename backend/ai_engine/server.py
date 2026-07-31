import os
import io
import json
import wave
import asyncio
import threading
import websockets
from PIL import ImageGrab
import pyaudiowpatch as pyaudio
import speech_recognition as sr
from google import genai
from pypdf import PdfReader
from dotenv import load_dotenv

load_dotenv()

# =====================================================================
# MODEL FALLBACK CASCADES (Grouped by Task Capability)
# =====================================================================
MODEL_TIERS = {
    "CODING": [
        "gemini-3.6-flash",
        "gemini-3.5-flash",
        "gemini-3-flash-preview",
        "gemini-2.5-flash",
        "gemini-flash-latest"
    ],
    "MCQ": [
        "gemini-3.5-flash-lite",
        "gemini-3.1-flash-lite-preview",
        "gemini-3.1-flash-lite",
        "gemini-2.5-flash-lite",
        "gemini-2.0-flash-lite",
        "gemini-2.0-flash-lite-001",
        "gemini-flash-lite-latest"
    ],
    "SQL": [
        "gemini-3.5-flash",
        "gemini-3.1-flash-lite",
        "gemini-2.5-flash",
        "gemini-2.0-flash",
        "gemini-2.0-flash-001"
    ],
    "BEHAVIORAL": [
        "gemini-omni-flash-preview",
        "gemini-3.5-flash",
        "gemini-3.6-flash",
        "gemini-2.5-flash"
    ],
    "VISUAL": [
        "gemini-3.1-flash-image",
        "gemini-3.1-flash-image-preview",
        "gemini-2.5-flash-image",
        "gemini-3.1-flash-lite-image",
        "gemini-3.5-flash"
    ],
    "AUDIO": [
        "gemini-3.1-flash-tts-preview",
        "gemini-2.5-flash-preview-tts",
        "gemini-3.5-flash",
        "gemini-2.5-flash"
    ]
}

# Default Global Fallback list if classification is ambiguous
DEFAULT_FALLBACK = [
    "gemini-3.5-flash",
    "gemini-3.6-flash",
    "gemini-3.5-flash-lite",
    "gemini-2.5-flash",
    "gemini-2.0-flash"
]

# =====================================================================
# GLOBAL STATE & USER VAULT
# =====================================================================
user_config = {
    "api_key": os.getenv("GEMINI_API_KEY", ""),
    "resume_context": "Candidate has experience in Software Engineering, Data Structures, Algorithms, and System Design."
}

is_recording = False
stop_recording_event = threading.Event()


def get_genai_client(custom_key=None):
    """Instantiates Gemini client using custom user key or environment key."""
    key = custom_key if custom_key else user_config["api_key"]
    if not key:
        raise ValueError("No Gemini API key provided. Set it in Settings.")
    return genai.Client(api_key=key)


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
TASK: CODING INTERVIEW QUESTION
OUTPUT FORMAT:
Provide ONLY the raw, optimal code inside a markdown block.
DO NOT explain the code. DO NOT tell me how it works. I just need to copy/paste it.
Put the Time and Space complexity as a comment at the top of the code.
"""
    elif task_type == "SQL":
        return base_rules + """
TASK: DATABASE / SQL QUERY
OUTPUT FORMAT:
Provide ONLY the raw SQL query inside a markdown block.
NO explanations.
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
# AI ROUTER WITH CASCADE FAILOVER
# =====================================================================
async def execute_cascade_query(contents, task_type="GENERAL", custom_key=None):
    """Iterates through task-specific model cascade until success."""
    client = get_genai_client(custom_key)
    cascade_models = MODEL_TIERS.get(task_type, DEFAULT_FALLBACK)
    prompt = build_prompt(task_type)
    
    full_contents = [prompt] + (contents if isinstance(contents, list) else [contents])
    
    for model_name in cascade_models:
        try:
            print(f"[🤖 ROUTER] Attempting task '{task_type}' on model: {model_name}")
            response = client.models.generate_content(
                model=model_name,
                contents=full_contents
            )
            if response and response.text:
                print(f"[✅ SUCCESS] Handled by model: {model_name}")
                return response.text.strip(), model_name
        except Exception as e:
            print(f"[⚠️ FAILOVER] Model {model_name} failed: {e}. Cascading to next model...")
            await asyncio.sleep(0.2) # Small backoff before failover

    raise RuntimeError(f"All models in cascade for task '{task_type}' failed.")


# =====================================================================
# AUDIO RECORDING ENGINE
# =====================================================================
def record_audio_sync():
    global stop_recording_event
    CHUNK = 4096
    MAX_SECONDS = 30
    
    with pyaudio.PyAudio() as p:
        try:
            # 👈 THE CTO FIX: Aggressive WASAPI Loopback Hunter
            wasapi_info = None
            
            try:
                wasapi_info = p.get_default_wasapi_loopback()
            except OSError:
                print("[⚠️ WARNING] Default WASAPI failed. Hunting for active earphone loopback...")
                
            # If default fails or we are using a headset, hunt through all devices
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
            
            # Record loop with interrupt check
            for _ in range(max_chunks):
                if stop_recording_event.is_set():
                    break
                frames.append(stream.read(CHUNK))
                
            stream.stop_stream()
            stream.close()

            if not frames:
                return "ERROR_NO_SPEECH"

            # Build WAV in RAM
            wav_io = io.BytesIO()
            wf = wave.open(wav_io, 'wb')
            wf.setnchannels(channels)
            wf.setsampwidth(sample_size)
            wf.setframerate(sample_rate)
            wf.writeframes(b''.join(frames))
            wf.close()
            wav_io.seek(0)

            # Transcribe
            recognizer = sr.Recognizer()
            with sr.AudioFile(wav_io) as source:
                audio_data = recognizer.record(source)
            
            return recognizer.recognize_google(audio_data, language="en-US")
            
        except sr.UnknownValueError:
            # This specifically triggers when it records pure silence
            return "ERROR_NO_SPEECH"
        except Exception as e:
            print(f"Audio Error: {e}")
            return "ERROR_SYSTEM"
        
# =====================================================================
# DYNAMIC INTENT CLASSIFIER
# =====================================================================
async def classify_intent(transcript, custom_key=None):
    """Uses a lightweight, ultra-fast model to classify the user's intent."""
    client = get_genai_client(custom_key)
    
    # We use 1.5-flash-8b because it is the fastest model for simple routing
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
        
        # Validate the output to ensure it matched our schema
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
    
    await websocket.send(json.dumps({
        "status": "loading", 
        "transcript": "🔴 Intercepting system audio... Press [ALT+A] to finish."
    }))
    
    transcript = await asyncio.to_thread(record_audio_sync)
    is_recording = False
    
    if transcript.startswith("ERROR"):
        await websocket.send(json.dumps({
            "status": "error", 
            "data": "Could not process audio. Ensure system sound is playing."
        }))
        return

    print(f"[🗣️ TRANSCRIBED]: {transcript}")
    
    await websocket.send(json.dumps({
        "status": "loading", 
        "transcript": f"Heard: '{transcript}'\nClassifying intent..."
    }))

    # 👈 THE CTO FIX: Dynamic AI Classification instead of hardcoded strings
    task_type = await classify_intent(transcript)
    
    await websocket.send(json.dumps({
        "status": "loading", 
        "transcript": f"Heard: '{transcript}'\nRouting through [{task_type}] cascade..."
    }))

    try:
        answer, resolved_model = await execute_cascade_query(
            contents=f"Interviewer asked: {transcript}",
            task_type=task_type
        )
        await websocket.send(json.dumps({
            "status": "success", 
            "data": answer,
            "transcript": f"Heard: '{transcript}' | Solved by: {resolved_model}"
        }))
    except Exception as e:
        await websocket.send(json.dumps({"status": "error", "data": str(e)}))


async def handle_screen_workflow(websocket, requested_mode="AUTO"):
    print("\n[📸 SCREEN CAPTURE TRIGGERED]")
    await websocket.send(json.dumps({
        "status": "loading", 
        "transcript": "Capturing screen buffer and analyzing visual layout..."
    }))
    
    try:
        screenshot = ImageGrab.grab()
        
        # Mode override or default visual cascade
        task_type = "VISUAL" if requested_mode == "AUTO" else requested_mode
        
        answer, resolved_model = await execute_cascade_query(
            contents=[screenshot, "Analyze this image and provide the exact solution according to the prompt instructions."],
            task_type=task_type
        )
        
        await websocket.send(json.dumps({
            "status": "success", 
            "data": answer,
            "transcript": f"Screen Solved | Model: {resolved_model} | Mode: {task_type}"
        }))
    except Exception as e:
        await websocket.send(json.dumps({"status": "error", "data": str(e)}))


# =====================================================================
# WEBSOCKET SERVER & CONFIG PROTOCOL
# =====================================================================
async def ws_handler(websocket):
    global is_recording, stop_recording_event
    print("\n[🌐 REACT UI CONNECTED TO MULTI-MODEL ENGINE]")
    
    try:
        async for message in websocket:
            payload = json.loads(message)
            action = payload.get("action")
            
            if action == "capture_screen":
                mode = payload.get("mode", "AUTO")
                asyncio.create_task(handle_screen_workflow(websocket, mode))
                
            elif action == "toggle_audio":
                if not is_recording:
                    asyncio.create_task(handle_audio_workflow(websocket))
                else:
                    stop_recording_event.set()
                    
            elif action == "update_config":
                # Handle API key and Resume updates dynamically from UI
                if "api_key" in payload and payload["api_key"]:
                    user_config["api_key"] = payload["api_key"]
                if "resume_text" in payload and payload["resume_text"]:
                    user_config["resume_context"] = payload["resume_text"]
                    
                await websocket.send(json.dumps({
                    "status": "config_updated",
                    "data": "✅ Settings saved successfully! Vault & Resume updated."
                }))
                
    except websockets.exceptions.ConnectionClosed:
        print("\n[⚠️ UI DISCONNECTED]")

async def start_server():
    print("✅ 4CPA MULTI-MODEL ROUTER ACTIVE ON PORT 8765")
    async with websockets.serve(ws_handler, "localhost", 8765):
        await asyncio.Future()

if __name__ == "__main__":
    asyncio.run(start_server())