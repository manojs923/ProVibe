import { useState, useEffect, useRef } from "react";
import { getCurrentWindow } from '@tauri-apps/api/window';
import { listen } from '@tauri-apps/api/event';
import { createClient } from '@supabase/supabase-js';
import { writeText } from '@tauri-apps/plugin-clipboard-manager';

// 🛡️ SUPABASE CLOUD CONNECTION
// Injected with your verified correct URL and Key from the Next.js setup!
const SUPABASE_URL = "https://yfyiizovbhsddpmgduvs.supabase.co";
const SUPABASE_ANON_KEY = "sb_publishable_NoyP-ECcNTrOdxc27CQdtw_9iopHIN9";

if (!SUPABASE_URL || !SUPABASE_ANON_KEY) {
  console.error("CRITICAL ERROR: Supabase environment variables are missing!");
}

const supabase = createClient(SUPABASE_URL, SUPABASE_ANON_KEY);

interface WebSocketPayload {
  status: 'loading' | 'success' | 'error' | 'config_updated' | 'sync_cursor' | 'nav_up' | 'nav_down';
  data?: string;
  transcript?: string;
  active_line?: number;
}

export default function App() {
  const [sessionCode, setSessionCode] = useState("");
  const [status, setStatus] = useState("STANDBY");
  const [transcript, setTranscript] = useState("Awaiting Double-Tab / Double-[-] snipe or audio input...");
  
  const [response, setResponse] = useState(
`// 🛡️ PROTOCOL STEALTH SUITE INITIALIZED
//
// 📸 Double-Tab / Double-[-] : Capture & Upload to Cloud
// 🎙️ Alt+A                   : Record Audio
// 👻 Alt+H                   : Hide / Unhide Window
// ↕️ Alt+Up/Down             : Adjust UI Opacity`
  );
  
  const [isVisible, setIsVisible] = useState(true);
  const [uiOpacity, setUiOpacity] = useState(0.95);

  const [activeLine, setActiveLine] = useState<number | null>(null);
  const activeLineRef = useRef<HTMLDivElement>(null);
  const responseRef = useRef(response);
  
  const [showSettings, setShowSettings] = useState(false);
  const [apiKey, setApiKey] = useState("");
  const [resumeText, setResumeText] = useState("");
  const [taskMode, setTaskMode] = useState<"AUTO" | "MCQ" | "CODING" | "SQL" | "BEHAVIORAL">("AUTO");

  const wsRef = useRef<WebSocket | null>(null);

  // 1. GENERATE SESSION CODE ON STARTUP
  useEffect(() => {
    const code = Math.random().toString(36).substring(2, 8).toUpperCase();
    setSessionCode(code);
  }, []);

  useEffect(() => {
    responseRef.current = response;
  }, [response]);

  useEffect(() => {
    const savedOpacity = localStorage.getItem('protocol_ui_opacity');
    if (savedOpacity) setUiOpacity(parseFloat(savedOpacity));
  }, []);

  useEffect(() => {
    localStorage.setItem('protocol_ui_opacity', uiOpacity.toString());
  }, [uiOpacity]);

  // 2. SUPABASE REAL-TIME LISTENER (FRIEND'S CLOUD OVERRIDE)
  useEffect(() => {
    if (!sessionCode) return;

    // Listens for your friend inserting an answer into the cloud database
    const channel = supabase
      .channel(`listen-answers-${sessionCode}`)
      .on(
        'postgres_changes',
        { 
          event: 'INSERT', 
          schema: 'public', 
          table: 'copilot_sessions',
          filter: `session_code=eq.${sessionCode}` 
        },
        (payload) => {
          console.log("Realtime payload received in Tauri:", payload);
          
          // STRICT CHECK: Only trigger if the row contains an answer_text
          // This ensures it completely ignores when screenshots are uploaded
          if (payload.new && payload.new.answer_text) {
            setResponse(payload.new.answer_text);
            setStatus("OVERRIDE RECEIVED");
            setTranscript("👤 Friend has transmitted the solution.");
          }
        }
      )
      .subscribe();

    return () => {
      supabase.removeChannel(channel);
    };
  }, [sessionCode]);

  // 3. WEBSOCKET HANDLER (PYTHON ENGINE)
  useEffect(() => {
    const socket = new WebSocket("ws://localhost:8765");
    wsRef.current = socket;
    
    socket.onopen = () => {
      setStatus("CONNECTED");
      // Send the session code to Python so it knows where to upload screenshots
      if (sessionCode) {
        socket.send(JSON.stringify({ action: "set_session", session_code: sessionCode }));
      }
      socket.send(JSON.stringify({ action: "set_mode", mode: "AUTO" }));
    };
    
    socket.onclose = () => setStatus("DISCONNECTED");

    socket.onmessage = (event: MessageEvent) => {
      try {
        const payload: WebSocketPayload = JSON.parse(event.data);
        
        if (payload.status === "loading") {
          setStatus("THINKING...");
          setTranscript(payload.transcript || "Processing request...");
          setIsVisible(true);
        } else if (payload.status === "success" && payload.data) {
          setStatus("READY");
          setTranscript(payload.transcript || "Processed successfully.");
          setResponse(payload.data);
          setActiveLine(payload.active_line || null);
        } else if (payload.status === "sync_cursor" && payload.active_line !== undefined) {
          setActiveLine(payload.active_line);
        } else if (payload.status === "nav_up") {
          setActiveLine(prev => (prev === null ? 1 : Math.max(1, prev - 1)));
        } else if (payload.status === "nav_down") {
          setActiveLine(prev => {
            const maxLines = responseRef.current.split('\n').length;
            return prev === null ? 1 : Math.min(maxLines, prev + 1);
          });
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
  }, [sessionCode]);

  useEffect(() => {
    if (wsRef.current?.readyState === WebSocket.OPEN) {
      wsRef.current.send(JSON.stringify({ action: "set_mode", mode: taskMode }));
    }
  }, [taskMode]);

  useEffect(() => {
    if (activeLineRef.current) {
      activeLineRef.current.scrollIntoView({ behavior: "smooth", block: "center" });
    }
  }, [activeLine, response]);

  useEffect(() => {
    const unlisten = listen<string>('app-command', (event) => {
      const command = event.payload; 

      if (command === "TRIGGER_SNIPER" && wsRef.current?.readyState === WebSocket.OPEN) {
        wsRef.current.send(JSON.stringify({ action: "capture_screen", mode: taskMode })); 
      }
      
      if (command === "TRIGGER_AUDIO" && wsRef.current?.readyState === WebSocket.OPEN) {
        wsRef.current.send(JSON.stringify({ action: "toggle_audio" })); 
      }
      
      if (command === "SUMMON_COPILOT" || command === "TOGGLE_VISIBILITY") {
        setIsVisible(prev => {
          const next = command === "SUMMON_COPILOT" ? true : !prev;
          return next;
        });
      }

      if (command === "OPACITY_UP") setUiOpacity(prev => Math.min(1.0, prev + 0.1));
      if (command === "OPACITY_DOWN") setUiOpacity(prev => Math.max(0.1, prev - 0.1));
    });

    return () => { unlisten.then(f => f()); };
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

  const copySessionCode = async () => {
    try {
      await writeText(sessionCode);
      setTranscript(`✅ Copied Session Code: ${sessionCode}`);
    } catch (err) {
      console.error("Failed to write to native clipboard:", err);
      setTranscript(`❌ Failed to copy code`);
    }
    setTimeout(() => setTranscript("Awaiting Double-Tab / Double-[-] snipe or audio input..."), 3000);
  };

  return (
    <main 
      className="h-screen w-screen p-3 select-none font-mono relative overflow-hidden"
      style={{ opacity: isVisible ? uiOpacity : 0, visibility: isVisible ? 'visible' : 'hidden', backgroundColor: 'transparent' }}
    >
      <div className="h-full w-full bg-slate-950 border-2 border-slate-700/80 rounded-xl p-4 flex flex-col justify-between text-white relative shadow-2xl">
        
        {/* HEADER BAR */}
        <div onPointerDown={() => getCurrentWindow().startDragging()} className="flex items-center justify-between border-b border-slate-700 pb-2 cursor-pointer">
          <div className="flex items-center gap-2 pointer-events-none">
            <span className="h-2.5 w-2.5 rounded-full bg-emerald-400 animate-pulse" />
            <span className="text-xs font-bold tracking-widest text-white uppercase">PROTOCOL v2.0 ENGINE</span>
          </div>
          
          {/* THE NEW SESSION CODE UI */}
          <div className="flex items-center gap-2 bg-slate-900 border border-slate-700 rounded pl-3 pr-1 py-0.5">
            <span className="text-[10px] font-bold text-slate-400">ID:</span>
            <span className="text-[11px] font-bold text-emerald-400 tracking-wider">{sessionCode || "GEN..."}</span>
            <button 
              onClick={copySessionCode}
              className="ml-2 text-[9px] bg-slate-800 hover:bg-slate-700 text-white px-2 py-0.5 rounded cursor-pointer transition-colors"
            >
              COPY
            </button>
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

              <label className="text-[10px] text-slate-400 block mb-1">Candidate Resume Context:</label>
              <textarea 
                rows={5}
                placeholder="Paste your resume, project highlights, or background info here..."
                value={resumeText}
                onChange={(e) => setResumeText(e.target.value)}
                className="w-full bg-slate-950 border border-slate-700 text-white text-xs p-2 rounded outline-none focus:border-emerald-500 font-sans resize-none"
              />
            </div>

            <div className="flex gap-2">
              <button onClick={saveSettings} className="flex-1 bg-emerald-600 hover:bg-emerald-500 text-white text-xs font-bold py-2 rounded uppercase tracking-wider">Save & Inject Vault</button>
              <button onClick={() => setShowSettings(false)} className="bg-slate-800 text-slate-300 text-xs font-bold px-4 py-2 rounded">Cancel</button>
            </div>
          </div>
        )}

        {/* INPUT STREAM / TELEMETRY */}
        <div className="my-2 p-2 bg-slate-900 rounded-lg border border-slate-700 text-xs">
          <p className="text-emerald-400 font-medium italic truncate">{transcript}</p>
        </div>

        {/* INTELLIGENCE FEED */}
        <div className="flex-1 p-3 bg-black rounded-lg border border-slate-700 text-xs font-mono overflow-y-auto">
          <div className="whitespace-pre-wrap leading-relaxed text-slate-100 font-semibold">
            {response.split('\n').map((line, index) => {
              const isTarget = activeLine === index + 1; 
              return (
                <div key={index} ref={isTarget ? activeLineRef : null} className={`transition-all duration-300 ${isTarget ? "bg-emerald-900/50 border-l-2 border-emerald-400 pl-2 text-emerald-300 py-0.5 -ml-2" : ""}`}>
                  {line || " "}
                </div>
              );
            })}
          </div>
        </div>

      </div>
    </main>
  );
}