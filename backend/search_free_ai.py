import requests
import json

def fetch_free_openrouter_models():
    print("🔍 Scanning OpenRouter for 100% FREE AI Models...\n")
    
    try:
        response = requests.get("https://openrouter.ai/api/v1/models")
        response.raise_for_status()
        data = response.json()
        
        free_models = []
        
        for model in data.get("data", []):
            pricing = model.get("pricing", {})
            prompt_cost = pricing.get("prompt", "-1")
            completion_cost = pricing.get("completion", "-1")
            
            # Identify models where cost is exactly zero
            if prompt_cost == "0" and completion_cost == "0":
                free_models.append({
                    "id": model["id"],
                    "name": model["name"],
                    "context_length": model["context_length"]
                })
                
        # Sort by context length to find the most capable ones
        free_models.sort(key=lambda x: x["context_length"], reverse=True)
        
        print(f"✅ FOUND {len(free_models)} COMPLETELY FREE MODELS:\n")
        print("-" * 60)
        
        for m in free_models:
            print(f"Model ID: {m['id']}")
            print(f"Context Window: {m['context_length']} tokens")
            print("-" * 60)
            
    except Exception as e:
        print(f"❌ Failed to fetch models: {e}")

if __name__ == "__main__":
    fetch_free_openrouter_models()