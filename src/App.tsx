import { useState, useEffect, useRef } from "react";
import { getCurrentWindow } from '@tauri-apps/api/window';
import { listen } from '@tauri-apps/api/event';

interface WebSocketPayload {
  status: 'loading' | 'success' | 'error' | 'config_updated';
  data?: string;
  transcript?: string;
}

export default function App() {
  const [status, setStatus] = useState("STANDBY");
  const [transcript, setTranscript] = useState("Awaiting voice input or screen sniper...");
  const [response, setResponse] = useState(`// Protocol AI Multi-Model Suite\n// Press ALT+S to capture screen | ALT+A for audio`);
  const [isVisible, setIsVisible] = useState(true);
  const [uiOpacity, setUiOpacity] = useState(0.95);
  
  // Settings & Customization
  const [showSettings, setShowSettings] = useState(false);
  const [apiKey, setApiKey] = useState("");
  const [resumeText, setResumeText] = useState("");
  const [taskMode, setTaskMode] = useState<"AUTO" | "MCQ" | "CODING" | "SQL" | "BEHAVIORAL">("AUTO");

  const wsRef = useRef<WebSocket | null>(null);

  useEffect(() => {
    const socket = new WebSocket("ws://localhost:8765");
    wsRef.current = socket;
    
    socket.onopen = () => setStatus("CONNECTED");
    socket.onclose = () => setStatus("DISCONNECTED");

    socket.onmessage = (event: MessageEvent) => {
      try {
        const payload: WebSocketPayload = JSON.parse(event.data);
        
        if (payload.status === "loading") {
          setStatus("THINKING...");
          setTranscript(payload.transcript || "Routing query through fallback cascades...");
          setIsVisible(true);
        } else if (payload.status === "success" && payload.data) {
          setStatus("READY");
          setTranscript(payload.transcript || "Capture processed successfully.");
          setResponse(payload.data);
        } else if (payload.status === "error" && payload.data) {
          setStatus("ERROR");
          setResponse("⚠️ Error: " + payload.data);
        } else if (payload.status === "config_updated") {
          setStatus("CONFIG SAVED");
          setTimeout(() => setStatus("READY"), 2000);
        }
      } catch (err) {
        console.error("Failed to parse socket payload", err);
      }
    };

    return () => socket.close();
  }, []);

  useEffect(() => {
    const unlisten = listen<string>('app-command', (event) => {
      const command = event.payload; 

      if (command === "TRIGGER_SNIPER") {
        if (wsRef.current?.readyState === WebSocket.OPEN) {
          wsRef.current.send(JSON.stringify({ 
            action: "capture_screen",
            mode: taskMode
          })); 
        }
      }
      
      if (command === "TRIGGER_AUDIO") {
        if (wsRef.current?.readyState === WebSocket.OPEN) {
          wsRef.current.send(JSON.stringify({ action: "toggle_audio" })); 
        }
      }
      
      if (command === "TOGGLE_VISIBILITY") setIsVisible(prev => !prev);
      if (command === "OPACITY_UP") setUiOpacity(prev => Math.min(1.0, prev + 0.1));
      if (command === "OPACITY_DOWN") setUiOpacity(prev => Math.max(0.1, prev - 0.1));
    });

    return () => {
      unlisten.then(f => f());
    };
  }, [taskMode]);

  const saveSettings = () => {
    if (wsRef.current?.readyState === WebSocket.OPEN) {
      wsRef.current.send(JSON.stringify({
        action: "update_config",
        api_key: apiKey,
        resume_text: resumeText
      }));
      setShowSettings(false);
    }
  };

  return (
    <main 
      className="h-screen w-screen p-3 select-none font-mono relative overflow-hidden"
      style={{ opacity: isVisible ? uiOpacity : 0, visibility: isVisible ? 'visible' : 'hidden', backgroundColor: 'transparent' }}
    >
      <div className="h-full w-full bg-slate-950 border-2 border-slate-700/80 rounded-xl p-4 flex flex-col justify-between text-white relative">
        
        {/* HEADER BAR */}
        <div onPointerDown={() => getCurrentWindow().startDragging()} className="flex items-center justify-between border-b border-slate-700 pb-2 cursor-move">
          <div className="flex items-center gap-2 pointer-events-none">
            <span className="h-2.5 w-2.5 rounded-full bg-emerald-400 animate-pulse" />
            <span className="text-xs font-bold tracking-widest text-white uppercase">PROTOCOL v2.0 ENGINE</span>
          </div>
          
          <div className="flex items-center gap-2">
            <select 
              value={taskMode} 
              onChange={(e) => setTaskMode(e.target.value as any)}
              className="bg-slate-900 border border-slate-700 text-[10px] text-emerald-400 font-bold rounded px-1 py-0.5 outline-none cursor-pointer"
            >
              <option value="AUTO">MODE: AUTO</option>
              <option value="MCQ">MODE: MCQ</option>
              <option value="CODING">MODE: CODE</option>
              <option value="SQL">MODE: SQL</option>
              <option value="BEHAVIORAL">MODE: RESUME</option>
            </select>

            <button 
              onClick={() => setShowSettings(!showSettings)}
              className="text-[10px] bg-slate-800 hover:bg-slate-700 text-white font-bold px-2 py-0.5 rounded border border-slate-600"
            >
              ⚙️ VAULT
            </button>

            <span className="text-[10px] font-bold px-2 py-0.5 rounded bg-slate-800 text-emerald-400 border border-emerald-500/50 pointer-events-none">
              {status}
            </span>
          </div>
        </div>

        {/* SETTINGS SLIDE-OVER DRAWER */}
        {showSettings && (
          <div className="absolute inset-x-4 top-14 bottom-12 bg-slate-900/95 backdrop-blur-md border border-slate-700 rounded-lg p-4 z-50 flex flex-col justify-between">
            <div>
              <h3 className="text-xs font-bold text-emerald-400 mb-2 uppercase tracking-wider">User Settings & Resume Vault</h3>
              
              <label className="text-[10px] text-slate-400 block mb-1">Custom Gemini API Key (BYOK / Free Tier):</label>
              <input 
                type="password"
                placeholder="AIzaSy..."
                value={apiKey}
                onChange={(e) => setApiKey(e.target.value)}
                className="w-full bg-slate-950 border border-slate-700 text-white text-xs p-2 rounded mb-3 outline-none focus:border-emerald-500"
              />

              <label className="text-[10px] text-slate-400 block mb-1">Candidate Resume Context (For Behavioral Alignment):</label>
              <textarea 
                rows={5}
                placeholder="Paste your resume, project highlights, or background info here..."
                value={resumeText}
                onChange={(e) => setResumeText(e.target.value)}
                className="w-full bg-slate-950 border border-slate-700 text-white text-xs p-2 rounded outline-none focus:border-emerald-500 font-sans resize-none"
              />
            </div>

            <div className="flex gap-2">
              <button 
                onClick={saveSettings}
                className="flex-1 bg-emerald-600 hover:bg-emerald-500 text-white text-xs font-bold py-2 rounded uppercase tracking-wider"
              >
                Save & Inject Vault
              </button>
              <button 
                onClick={() => setShowSettings(false)}
                className="bg-slate-800 text-slate-300 text-xs font-bold px-4 py-2 rounded"
              >
                Cancel
              </button>
            </div>
          </div>
        )}

        {/* INPUT STREAM / TELEMETRY */}
        <div className="my-3 p-3 bg-slate-900 rounded-lg border border-slate-700 text-xs">
          <span className="text-[10px] font-bold text-slate-300 block mb-1 uppercase tracking-wider">Input Stream & Telemetry</span>
          <p className="text-white font-medium italic">{transcript}</p>
        </div>

        {/* INTELLIGENCE FEED */}
        <div className="flex-1 p-3 bg-black rounded-lg border border-slate-700 text-xs font-mono overflow-y-auto">
          <span className="text-[10px] font-bold text-emerald-400 block mb-1 uppercase tracking-wider">Intelligence Feed</span>
          <pre className="whitespace-pre-wrap leading-relaxed text-slate-100 font-semibold">{response}</pre>
        </div>

        {/* FOOTER */}
        <div onPointerDown={() => getCurrentWindow().startDragging()} className="mt-3 flex items-center justify-between text-[10px] text-slate-300 font-bold pt-2 border-t border-slate-800 cursor-move">
          <span className="text-slate-200 pointer-events-none">CASCADE ROUTER: ACTIVE</span>
          <span className="text-emerald-400 pointer-events-none">SNIPE: ALT+S | AUDIO: ALT+A | HIDE: ALT+H</span>
        </div>
      </div>
    </main>
  );
}