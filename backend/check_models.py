import os

import requests

API_KEY = os.getenv("GEMINI_API_KEY")
if not API_KEY:
    raise SystemExit("Set GEMINI_API_KEY before running check_models.py")

url = f"https://generativelanguage.googleapis.com/v1beta/models?key={API_KEY}"
response = requests.get(url, timeout=20)
response.raise_for_status()

data = response.json()

print("\n✅ GOOGLE SAYS YOU CAN USE THESE FLASH MODELS:\n")
if "models" in data:
    for model in data["models"]:
        name = model.get("name", "")
        # Filter for only the fast "flash" models
        if "flash" in name.lower():
            print(f"- {name.replace('models/', '')}")
else:
    print("❌ ERROR:", data)
print("\n==================================")