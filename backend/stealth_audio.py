import pyaudiowpatch as pyaudio
import speech_recognition as sr
from google import genai
import wave
import io
import threading

# 🛑 INSERT YOUR GEMINI API KEY HERE
API_KEY = "AQ.Ab8RN6LnyY-shVi-yjTIt6TaFJcer8KkgWrWcm2ZZoK-D0bf4A"

client = genai.Client(api_key=API_KEY)
recognizer = sr.Recognizer()

# The AI Router: Ranked list of models to try automatically
AVAILABLE_MODELS = [
    "gemini-3.5-flash",        # Top tier, fastest
    "gemini-2.5-flash",        # Solid fallback
    "gemini-flash-latest",     # Dynamic pointer to whichever is free
    "gemini-3.5-flash-lite",   # Lighter, less likely to be rate-limited
    "gemini-2.5-flash-lite"    # Absolute last resort
]

def process_audio_chunk(frames, channels, sample_rate, sample_size):
    """Runs in the background: Transcribes audio and asks the AI"""
    try:
        # 1. Build the WAV file entirely in RAM
        wav_io = io.BytesIO()
        wf = wave.open(wav_io, 'wb')
        wf.setnchannels(channels)
        wf.setsampwidth(sample_size)
        wf.setframerate(sample_rate)
        wf.writeframes(b''.join(frames))
        wf.close()
        wav_io.seek(0)

        # 2. Transcribe
        with sr.AudioFile(wav_io) as source:
            audio_data = recognizer.record(source)
            
        text = recognizer.recognize_google(audio_data, language="en-IN")
        
        print(f"\n[🗣️ INTERCEPTED]: {text}")
        
        # 3. Trigger the AI 
        if len(text.split()) > 3:
            prompt = f"You are a coding copilot. Solve this problem quickly and concisely with code: {text}"
            
            # 4. THE AUTO-FAILOVER ROUTER
            success = False
            for model_name in AVAILABLE_MODELS:
                try:
                    print(f"[🤖 Routing to {model_name} ...]")
                    response = client.models.generate_content(
                        model=model_name,
                        contents=prompt
                    )
                    # If we reach here, it succeeded! Print and break the loop.
                    print("\n==================================")
                    print(f"🔥 AI RESPONSE ({model_name}):\n{response.text.strip()}")
                    print("==================================\n")
                    success = True
                    break # Stop trying other models
                    
                except Exception as api_error:
                    # If this model is busy/unavailable, silently catch it and loop to the next one
                    print(f"   [⚠️ {model_name} busy or failed, switching...]")
                    continue
            
            if not success:
                print("\n[❌ ALL MODELS FAILED. Google servers are completely slammed right now.]")
                
            print("🔴 Recording background audio...") 

    except sr.UnknownValueError:
        pass
    except Exception as e:
        print(f"\n[❌ BACKGROUND ERROR]: {e}")


if __name__ == "__main__":
    CHUNK = 4096
    RECORD_SECONDS = 7 

    with pyaudio.PyAudio() as p:
        try:
            wasapi_info = p.get_default_wasapi_loopback()
            device_index = wasapi_info["index"]
            sample_rate = int(wasapi_info["defaultSampleRate"])
            channels = wasapi_info["maxInputChannels"]
            sample_size = p.get_sample_size(pyaudio.paInt16)
            
            print(f"✅ STEALTH PIPELINE ACTIVE ON: {wasapi_info['name']}")
            print("⏳ Listening continuously...")
            print("🔴 Recording background audio...")

            stream = p.open(format=pyaudio.paInt16,
                            channels=channels,
                            rate=sample_rate,
                            input=True,
                            frames_per_buffer=CHUNK,
                            input_device_index=device_index)

            while True:
                frames = []
                for _ in range(0, int(sample_rate / CHUNK * RECORD_SECONDS)):
                    data = stream.read(CHUNK)
                    frames.append(data)
                
                threading.Thread(target=process_audio_chunk, args=(frames, channels, sample_rate, sample_size)).start()

        except KeyboardInterrupt:
            print("\n🛑 Shutting down stealth pipeline.")
        except Exception as e:
            print(f"❌ ERROR: {e}")