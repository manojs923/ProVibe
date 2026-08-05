import { useState, useEffect, useRef } from "react";
import { getCurrentWindow, PhysicalPosition, PhysicalSize } from '@tauri-apps/api/window';
import { listen } from '@tauri-apps/api/event';

interface WebSocketPayload {
  status: 'loading' | 'success' | 'error' | 'config_updated' | 'sync_cursor';
  data?: string;
  transcript?: string;
  active_line?: number;
}

export default function App() {
  const [status, setStatus] = useState("STANDBY");
  const [transcript, setTranscript] = useState("Awaiting voice input or screen sniper...");
  const [response, setResponse] = useState(`// Protocol AI Multi-Model Suite\n// Press ALT+S to capture screen | ALT+A for audio`);
  const [isVisible, setIsVisible] = useState(true);
  const [uiOpacity, setUiOpacity] = useState(0.95);
  
  // Teleprompter State
  const [activeLine, setActiveLine] = useState<number | null>(null);
  const activeLineRef = useRef<HTMLDivElement>(null);
  
  // Settings & Customization
  const [showSettings, setShowSettings] = useState(false);
  const [apiKey, setApiKey] = useState("");
  const [resumeText, setResumeText] = useState("");
  const [taskMode, setTaskMode] = useState<"AUTO" | "MCQ" | "CODING" | "SQL" | "BEHAVIORAL">("AUTO");

  const wsRef = useRef<WebSocket | null>(null);

  // ---------------------------------------------------------
  // 1. RESTORE SAVED WINDOW POSITION, SIZE & OPACITY ON LOAD
  // ---------------------------------------------------------
  useEffect(() => {
    const appWindow = getCurrentWindow();

    // Restore Opacity
    const savedOpacity = localStorage.getItem('protocol_ui_opacity');
    if (savedOpacity) {
      setUiOpacity(parseFloat(savedOpacity));
    }

    // Restore Window Position & Size
    const restoreWindowState = async () => {
      try {
        const savedPos = localStorage.getItem('protocol_win_pos');
        const savedSize = localStorage.getItem('protocol_win_size');

        if (savedPos) {
          const { x, y } = JSON.parse(savedPos);
          await appWindow.setPosition(new PhysicalPosition(x, y));
        }

        if (savedSize) {
          const { width, height } = JSON.parse(savedSize);
          await appWindow.setSize(new PhysicalSize(width, height));
        }
      } catch (err) {
        console.error("Failed to restore window state:", err);
      }
    };

    restoreWindowState();
  }, []);

  // ---------------------------------------------------------
  // 2. SAVE OPACITY WHENEVER IT CHANGES
  // ---------------------------------------------------------
  useEffect(() => {
    localStorage.setItem('protocol_ui_opacity', uiOpacity.toString());
  }, [uiOpacity]);

  // ---------------------------------------------------------
  // 3. TRACK & SAVE WINDOW MOVES & RESIZES
  // ---------------------------------------------------------
  useEffect(() => {
    const appWindow = getCurrentWindow();

    const saveWindowState = async () => {
      try {
        const pos = await appWindow.outerPosition();
        const size = await appWindow.outerSize();
        localStorage.setItem('protocol_win_pos', JSON.stringify({ x: pos.x, y: pos.y }));
        localStorage.setItem('protocol_win_size', JSON.stringify({ width: size.width, height: size.height }));
      } catch (err) {
        console.error("Failed to save window state:", err);
      }
    };

    let unlistenMoved: (() => void) | undefined;
    let unlistenResized: (() => void) | undefined;

    const setupListeners = async () => {
      unlistenMoved = await appWindow.onMoved(saveWindowState);
      unlistenResized = await appWindow.onResized(saveWindowState);
    };

    setupListeners();

    return () => {
      if (unlistenMoved) unlistenMoved();
      if (unlistenResized) unlistenResized();
    };
  }, []);

  // ---------------------------------------------------------
  // WEBSOCKET HANDLER
  // ---------------------------------------------------------
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
          setActiveLine(payload.active_line || null);
        } else if (payload.status === "sync_cursor" && payload.active_line !== undefined) {
          setActiveLine(payload.active_line);
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

  // Auto-scroll logic when activeLine changes
  useEffect(() => {
    if (activeLineRef.current) {
      activeLineRef.current.scrollIntoView({ behavior: "smooth", block: "center" });
    }
  }, [activeLine, response]);

  // Hotkey / Tauri Event Command Listeners
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
        <div onPointerDown={() => getCurrentWindow().startDragging()} className="flex items-center justify-between border-b border-slate-700 pb-2 cursor-pointer">
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
              className="flex items-center gap-1 text-[10px] bg-slate-800 hover:bg-slate-700 text-white font-bold px-2 py-0.5 rounded border border-slate-600"
            >
              <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" className="w-3 h-3 text-slate-300">
                <circle cx="12" cy="12" r="3"></circle>
                <path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 0 1 0 2.83 2 2 0 0 1-2.83 0l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-2 2 2 2 0 0 1-2-2v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 0 1-2.83 0 2 2 0 0 1 0-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1-2-2 2 2 0 0 1 2-2h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 0 1 0-2.83 2 2 0 0 1 2.83 0l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 2-2 2 2 0 0 1 2 2v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 0 1 2.83 0 2 2 0 0 1 0 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 2 2 2 2 0 0 1-2 2h-.09a1.65 1.65 0 0 0-1.51 1z"></path>
              </svg>
              VAULT
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
          <div className="whitespace-pre-wrap leading-relaxed text-slate-100 font-semibold">
            {response.split('\n').map((line, index) => {
              const isTarget = activeLine === index + 1; 
              
              return (
                <div
                  key={index}
                  ref={isTarget ? activeLineRef : null}
                  className={`transition-all duration-300 ${
                    isTarget 
                      ? "bg-emerald-900/50 border-l-2 border-emerald-400 pl-2 text-emerald-300 py-0.5 -ml-2" 
                      : ""
                  }`}
                >
                  {line || " "}
                </div>
              );
            })}
          </div>
        </div>

        {/* FOOTER */}
        <div onPointerDown={() => getCurrentWindow().startDragging()} className="mt-3 flex items-center justify-between text-[10px] text-slate-300 font-bold pt-2 border-t border-slate-800 cursor-pointer">
          <span className="text-slate-200 pointer-events-none">CASCADE ROUTER: ACTIVE</span>
          <span className="text-emerald-400 pointer-events-none">SNIPE: ALT+S | AUDIO: ALT+A | HIDE: ALT+H</span>
        </div>
      </div>
    </main>
  );
}