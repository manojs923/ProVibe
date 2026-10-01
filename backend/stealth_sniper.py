import keyboard
import threading
import asyncio
import websockets
import json
import queue
import os
import time
import cv2
import pyvirtualcam
import pygetwindow as gw
import mss
from PIL import Image
from google import genai
from dotenv import load_dotenv

load_dotenv()

API_KEY = os.getenv("GEMINI_API_KEY")
if not API_KEY:
    raise ValueError("🚨 api key missing. glowies took it.")

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

message_queue = queue.Queue()
camera_active = False

# 🔥 NEW: optical air-gap screenshot buffering
def get_vm_screenshot():
    try:
        # target the specific rdp/hyper-v window
        windows = gw.getWindowsWithTitle("Virtual Machine Connection")
        if not windows:
            windows = gw.getWindowsWithTitle("exam_stealth_vm")
            
        if not windows:
            print("[⚠️] target vm window not found. proctoring bypass compromised.")
            return None
            
        win = windows[0]
        monitor = {"top": win.top, "left": win.left, "width": win.width, "height": win.height}
        
        with mss.mss() as sct:
            sct_img = sct.grab(monitor)
            # convert raw mss pixels to PIL for the llm
            return Image.frombytes("RGB", sct_img.size, sct_img.bgra, "raw", "BGRX")
    except Exception as e:
        print(f"[❌] scraping error: {e}")
        return None

# 🔥 NEW: virtual camera spoofing loop for msb bypass
def virtual_camera_loop():
    global camera_active
    print("[🎥] injecting pre-recorded feed into virtual webcam...")
    try:
        cap = cv2.VideoCapture('assets/loop.mp4')
        if not cap.isOpened():
            print("[❌] assets/loop.mp4 missing. no rizz on the camera.")
            return
            
        width = int(cap.get(cv2.CAP_PROP_FRAME_WIDTH))
        height = int(cap.get(cv2.CAP_PROP_FRAME_HEIGHT))
        fps = cap.get(cv2.CAP_PROP_FPS) or 30.0
        
        with pyvirtualcam.Camera(width=width, height=height, fps=fps) as cam:
            while camera_active:
                ret, frame = cap.read()
                if not ret:
                    cap.set(cv2.CAP_PROP_POS_FRAMES, 0) # loop it
                    continue
                
                # cv2 is BGR, virtualcam expects RGB
                frame_rgb = cv2.cvtColor(frame, cv2.COLOR_BGR2RGB)
                cam.send(frame_rgb)
                cam.sleep_until_next_frame()
    except Exception as e:
        print(f"[❌] virtual camera error: {e}")

def capture_and_solve():
    print("\n[📸 BUFFERING VM SCREENSHOT...]")
    message_queue.put(json.dumps({"status": "loading"}))
    
    screenshot = get_vm_screenshot()
    if not screenshot:
        message_queue.put(json.dumps({"status": "error", "data": "VM window occluded or missing."}))
        return
        
    try:
        success = False
        for model_name in AVAILABLE_MODELS:
            try:
                print(f"[🤖 routing payload to {model_name}]")
                response = client.models.generate_content(
                    model=model_name,
                    contents=[SYSTEM_PROMPT, screenshot]
                )
                
                answer = response.text.strip()
                print(f"\n🔥 BEAMING ANSWER TO C2 PANEL ({model_name})")
                message_queue.put(json.dumps({"status": "success", "data": answer}))
                success = True
                break
                
            except Exception as api_error:
                print(f"   [⚠️ {model_name} choked]: {api_error}")
                continue
        
        if not success:
            message_queue.put(json.dumps({"status": "error", "data": "all models failed. ngmi."}))
            
    except Exception as e:
        print(f"\n[❌ SYSTEM ERROR]: {e}")
        message_queue.put(json.dumps({"status": "error", "data": str(e)}))

# --- COOLDOWN TRACKERS ---
last_capture = 0
last_toggle = 0

def trigger_sniper():
    global last_capture
    if time.time() - last_capture > 2.0:
        last_capture = time.time()
        threading.Thread(target=capture_and_solve).start()

def toggle_visibility():
    global last_toggle
    if time.time() - last_toggle > 0.5:
        last_toggle = time.time()
        message_queue.put(json.dumps({"status": "command", "action": "toggle_hide"}))
    
def increase_opacity():
    message_queue.put(json.dumps({"status": "command", "action": "opacity_up"}))
    
def decrease_opacity():
    message_queue.put(json.dumps({"status": "command", "action": "opacity_down"}))

# --- BI-DIRECTIONAL WEBSOCKET SERVER ---
async def ws_handler(websocket):
    print("\n[🌐 REACT C2 PANEL CONNECTED]")
    global camera_active
    
    async def send_messages():
        while True:
            try:
                msg = message_queue.get_nowait()
                await websocket.send(msg)
            except queue.Empty:
                await asyncio.sleep(0.1)
            except websockets.exceptions.ConnectionClosed:
                break
                
    async def receive_messages():
        global camera_active
        try:
            async for message in websocket:
                data = json.loads(message)
                if data.get("action") == "set_target_browser":
                    target = data.get("target")
                    print(f"[⚙️] c2 panel set target mode: {target}")
                    
                    if target == "MSB" and not camera_active:
                        camera_active = True
                        threading.Thread(target=virtual_camera_loop, daemon=True).start()
                    elif target != "MSB":
                        camera_active = False
        except websockets.exceptions.ConnectionClosed:
            print("\n[⚠️ REACT C2 PANEL DISCONNECTED]")

    await asyncio.gather(send_messages(), receive_messages())

async def start_server():
    async with websockets.serve(ws_handler, "localhost", 8765):
        await asyncio.Future()

def start_websocket_server():
    asyncio.run(start_server())

if __name__ == "__main__":
    print("✅ HOST-SIDE C2 PIPELINE IGNITED...")
    threading.Thread(target=start_websocket_server, daemon=True).start()
    
    print("🎯 [ALT + S]       : scrape vm & solve")
    print("👻 [ALT + H]       : toggle hide/show c2 ui")
    print("🔆 [ALT + UP/DOWN] : adjust c2 opacity")
    print("🛑 [ESC]           : exit\n")
    
    keyboard.add_hotkey('alt+s', trigger_sniper)
    keyboard.add_hotkey('alt+h', toggle_visibility)
    keyboard.add_hotkey('alt+up', increase_opacity)
    keyboard.add_hotkey('alt+down', decrease_opacity)
    
    keyboard.wait('esc')
    camera_active = False
    print("\n🛑 tearing down pipeline.")