import { useState, useEffect } from "react";

interface WebSocketPayload {
  status: 'loading' | 'success' | 'error' | 'command';
  data?: string;
  action?: 'toggle_hide' | 'opacity_up' | 'opacity_down';
}

export default function App() {
  const [status, setStatus] = useState("STANDBY");
  const [transcript, setTranscript] = useState("Awaiting voice input or screen sniper...");
  const [response, setResponse] = useState(`// High-Contrast Output Stream
function solve(nums, target) {
    const map = new Map();
    for (let i = 0; i < nums.length; i++) {
        const diff = target - nums[i];
        if (map.has(diff)) return [map.get(diff), i];
        map.set(nums[i], i);
    }
    return [];
}`);

  // NEW: Stealth States
  const [isVisible, setIsVisible] = useState(true);
  const [uiOpacity, setUiOpacity] = useState(0.95);

  useEffect(() => {
    const ws = new WebSocket("ws://localhost:8765");

    ws.onopen = () => {
      console.log("Connected to Python backend!");
      setStatus("CONNECTED");
    };
    
    ws.onclose = () => setStatus("DISCONNECTED");

    ws.onmessage = (event: MessageEvent) => {
      try {
        const payload: WebSocketPayload = JSON.parse(event.data);
        
        // NEW: Handle Stealth Commands
        if (payload.status === "command") {
          if (payload.action === "toggle_hide") setIsVisible(prev => !prev);
          if (payload.action === "opacity_up") setUiOpacity(prev => Math.min(1.0, prev + 0.1));
          if (payload.action === "opacity_down") setUiOpacity(prev => Math.max(0.1, prev - 0.1));
          return;
        }

        // Standard AI handling
        if (payload.status === "loading") {
          setStatus("THINKING...");
          setTranscript("Analyzing screen capture (ALT + S)...");
          setIsVisible(true); // Auto-show if a capture is triggered
        } else if (payload.status === "success" && payload.data) {
          setStatus("READY");
          setTranscript("Sniper capture successful. Outputting code:");
          setResponse(payload.data);
        } else if (payload.status === "error" && payload.data) {
          setStatus("ERROR");
          setResponse("⚠️ Error: " + payload.data);
        }
      } catch (err) {
        console.error("Failed to parse message", err);
      }
    };

    return () => ws.close();
  }, []);

  // NEW: The wrapper style applies our dynamic display and opacity
  return (
    <main 
      className="h-screen w-screen p-3 bg-transparent select-none font-mono transition-opacity duration-200"
      style={{ 
        display: isVisible ? 'block' : 'none',
        opacity: uiOpacity 
      }}
    >
      <div className="h-full w-full bg-slate-950 border-2 border-slate-700/80 rounded-xl p-4 flex flex-col justify-between shadow-[0_0_20px_rgba(0,0,0,0.8)] text-white">
        
        <div className="flex items-center justify-between border-b border-slate-700 pb-2">
          <div className="flex items-center gap-2">
            <span className="h-2.5 w-2.5 rounded-full bg-emerald-400 animate-pulse shadow-[0_0_8px_#34d399]" />
            <span className="text-xs font-bold tracking-widest text-white uppercase">PROTOCOL v1.0</span>
          </div>
          <span className="text-[10px] font-bold px-2 py-0.5 rounded bg-slate-800 text-emerald-400 border border-emerald-500/50">
            {status} | OPACITY: {Math.round(uiOpacity * 100)}%
          </span>
        </div>

        <div className="my-3 p-3 bg-slate-900 rounded-lg border border-slate-700 text-xs">
          <span className="text-[10px] font-bold text-slate-300 block mb-1 uppercase tracking-wider">
            Input Stream
          </span>
          <p className="text-white font-medium italic">{transcript}</p>
        </div>

        <div className="flex-1 p-3 bg-black rounded-lg border border-slate-700 text-xs font-mono overflow-y-auto">
          <span className="text-[10px] font-bold text-emerald-400 block mb-1 uppercase tracking-wider">
            Intelligence Feed
          </span>
          <pre className="whitespace-pre-wrap leading-relaxed text-slate-100 font-semibold drop-shadow">
            {response}
          </pre>
        </div>

        <div className="mt-3 flex items-center justify-between text-[10px] text-slate-300 font-bold pt-2 border-t border-slate-800">
          <span className="text-slate-200">MODE: GHOST</span>
          <span className="text-emerald-400">HIDE: ALT+H | OPACITY: ALT+UP/DN</span>
        </div>
      </div>
    </main>
  );
}