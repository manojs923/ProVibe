import requests

# 🛑 INSERT YOUR API KEY HERE
API_KEY = "AQ.Ab8RN6LnyY-shVi-yjTIt6TaFJcer8KkgWrWcm2ZZoK-D0bf4A"

url = f"https://generativelanguage.googleapis.com/v1beta/models?key={API_KEY}"
response = requests.get(url)

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