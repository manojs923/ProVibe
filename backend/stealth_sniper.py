import keyboard
from PIL import ImageGrab
from google import genai
import threading
import asyncio
import websockets
import json
import queue
import os
from dotenv import load_dotenv

# Load the environment variables from the .env file
load_dotenv()

# Securely fetch the API key
API_KEY = os.getenv("GEMINI_API_KEY")

if not API_KEY:
    raise ValueError("🚨 API Key not found! Please make sure your .env file exists and contains GEMINI_API_KEY.")

client = genai.Client(api_key=API_KEY)
AVAILABLE_MODELS = ["gemini-3.5-flash", "gemini-2.5-flash", "gemini-flash-latest"]

SYSTEM_PROMPT = """
You are a stealth interview copilot. I am taking a software engineering interview. 
Look at the provided screenshot of my screen, find the interview question, and give me the exact answer.
RULES:
1. NEVER say "As an AI..." or "I don't have feelings." 
2. Write the answer from MY perspective (first-person "I").
3. If it's a coding question, just give me the raw, highly optimized code. No explanations unless requested.
4. Keep behavioral answers under 4 sentences.
"""

# Thread-safe pipeline to pass AI answers & commands to the React UI
message_queue = queue.Queue()

def capture_and_solve():
    print("\n[📸 SNAPSHOT TAKEN! Sending to AI...]")
    # Tell React we are loading!
    message_queue.put(json.dumps({"status": "loading"}))
    
    try:
        screenshot = ImageGrab.grab()
        success = False
        
        for model_name in AVAILABLE_MODELS:
            try:
                print(f"[🤖 Routing to {model_name} ...]")
                response = client.models.generate_content(
                    model=model_name,
                    contents=[SYSTEM_PROMPT, screenshot]
                )
                
                answer = response.text.strip()
                print(f"\n🔥 BEAMING ANSWER TO UI ({model_name})")
                
                # Beam the actual code to React!
                message_queue.put(json.dumps({"status": "success", "data": answer}))
                
                success = True
                break
                
            except Exception as api_error:
                print(f"   [⚠️ {model_name} FAILED]: {api_error}")
                continue
        
        if not success:
            message_queue.put(json.dumps({"status": "error", "data": "All models failed. Try again."}))
            
    except Exception as e:
        print(f"\n[❌ SYSTEM ERROR]: {e}")
        message_queue.put(json.dumps({"status": "error", "data": str(e)}))
        
    print("🎯 Sniper mode active. Waiting for hotkeys...")

# --- HOTKEY ACTION HANDLERS ---
def trigger_sniper():
    threading.Thread(target=capture_and_solve).start()

def toggle_visibility():
    message_queue.put(json.dumps({"status": "command", "action": "toggle_hide"}))
    
def increase_opacity():
    message_queue.put(json.dumps({"status": "command", "action": "opacity_up"}))
    
def decrease_opacity():
    message_queue.put(json.dumps({"status": "command", "action": "opacity_down"}))

# --- WEBSOCKET SERVER ---
async def ws_handler(websocket):
    print("\n[🌐 REACT FRONTEND CONNECTED!]")
    while True:
        try:
            # Check for new AI answers or commands every 0.1 seconds
            msg = message_queue.get_nowait()
            await websocket.send(msg)
        except queue.Empty:
            await asyncio.sleep(0.1)
        except websockets.exceptions.ConnectionClosed:
            print("\n[⚠️ REACT FRONTEND DISCONNECTED]")
            break

async def start_server():
    async with websockets.serve(ws_handler, "localhost", 8765):
        await asyncio.Future()  # Keeps the server running forever

def start_websocket_server():
    asyncio.run(start_server())

if __name__ == "__main__":
    print("✅ STEALTH BACKEND STARTING...")
    
    # Start the WebSocket server silently in the background
    threading.Thread(target=start_websocket_server, daemon=True).start()
    
    print("🎯 [ALT + S]       : Capture Screen & Solve")
    print("👻 [ALT + H]       : Toggle Hide/Show UI")
    print("🔆 [ALT + UP/DOWN] : Increase/Decrease UI Opacity")
    print("🛑 [ESC]           : Exit\n")
    
    # Register global system hotkeys
    keyboard.add_hotkey('alt+s', trigger_sniper)
    keyboard.add_hotkey('alt+h', toggle_visibility)
    keyboard.add_hotkey('alt+up', increase_opacity)
    keyboard.add_hotkey('alt+down', decrease_opacity)
    
    keyboard.wait('esc')
    print("\n🛑 Shutting down backend.")