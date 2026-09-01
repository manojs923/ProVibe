import { useState, useEffect, useRef } from "react";
import { createClient } from '@supabase/supabase-js';

const SUPABASE_URL = import.meta.env.VITE_SUPABASE_URL;
const SUPABASE_ANON_KEY = import.meta.env.VITE_SUPABASE_ANON_KEY;

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

// Environment Detection Helper
const isTauri = () => {
  return typeof window !== 'undefined' && 
    ('__TAURI__' in window || '__TAURI_INTERNALS__' in window || '__TAURI_IPC__' in window);
};

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
  const taskModeRef = useRef(taskMode);

  // Sync state to refs for use in event listeners
  useEffect(() => {
    taskModeRef.current = taskMode;
  }, [taskMode]);

  useEffect(() => {
    responseRef.current = response;
  }, [response]);

  // 1. GENERATE SESSION CODE ON STARTUP
  useEffect(() => {
    const code = Math.random().toString(36).substring(2, 8).toUpperCase();
    setSessionCode(code);
  }, []);

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
          console.log("Realtime payload received:", payload);
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

  // 3. WEBSOCKET HANDLER (PYTHON ENGINE) - ROBUST LIFECYCLE
  useEffect(() => {
    if (!sessionCode) return;
    let isMounted = true;
    let reconnectTimer: number | undefined;

    const connectWs = () => {
      if (!isMounted) return;
      if (wsRef.current && (wsRef.current.readyState === WebSocket.CONNECTING || wsRef.current.readyState === WebSocket.OPEN)) {
        return;
      }

      // Allow production web builds to use a secure WSS endpoint
      // const wsUrl = import.meta.env.VITE_WS_URL || "ws://localhost:8765";
      const socket = new WebSocket("wss://protocol-i59x.onrender.com");
      wsRef.current = socket;
      
      socket.onopen = () => {
        if (!isMounted) return;
        setStatus("CONNECTED");
        socket.send(JSON.stringify({ action: "set_session", session_code: sessionCode }));
        socket.send(JSON.stringify({ action: "set_mode", mode: taskModeRef.current }));
      };
      
      socket.onclose = () => {
        if (!isMounted) return;
        setStatus("DISCONNECTED");
        wsRef.current = null;
        reconnectTimer = window.setTimeout(connectWs, 3000);
      };

      socket.onerror = (err) => {
        console.error("WebSocket error:", err);
      };

      socket.onmessage = (event: MessageEvent) => {
        if (!isMounted) return;
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
            setTimeout(() => { if (isMounted) setStatus("READY") }, 2000);
          }
        } catch (err) {
          console.error("Failed to parse socket payload", err);
        }
      };
    };

    connectWs();

    return () => {
      isMounted = false;
      if (reconnectTimer) clearTimeout(reconnectTimer);
      if (wsRef.current) {
        wsRef.current.onclose = null; // Prevent reconnect loop on unmount
        wsRef.current.close();
        wsRef.current = null;
      }
    };
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

  // 4. MULTI-PLATFORM MEDIA & ACTION TRIGGERS
  const triggerScreenCapture = async () => {
    const mode = taskModeRef.current;
    if (isTauri()) {
      if (wsRef.current?.readyState === WebSocket.OPEN) {
        wsRef.current.send(JSON.stringify({ action: "capture_screen", mode }));
      }
    } else {
      let stream: MediaStream | null = null;
      try {
        setStatus("CAPTURING...");
        stream = await navigator.mediaDevices.getDisplayMedia({ video: true });
        const video = document.createElement('video');
        video.srcObject = stream;
        await video.play();
        
        const canvas = document.createElement('canvas');
        canvas.width = video.videoWidth;
        canvas.height = video.videoHeight;
        canvas.getContext('2d')?.drawImage(video, 0, 0);
        
        const dataUrl = canvas.toDataURL('image/jpeg', 0.8); 
        
        if (wsRef.current?.readyState === WebSocket.OPEN) {
          wsRef.current.send(JSON.stringify({ 
            action: "browser_image_upload", 
            image_data: dataUrl,
            mode: mode
          }));
        }
        
        setTranscript("Browser screen capture transmitted to backend for processing.");
        setStatus("READY");
      } catch (err) {
        console.error(err);
        setStatus("ERROR");
        setTranscript("Screen capture permission denied or failed.");
      } finally {
        if (stream) {
          stream.getTracks().forEach(t => t.stop());
        }
      }
    }
  };

  const triggerAudio = async () => {
    if (isTauri()) {
      if (wsRef.current?.readyState === WebSocket.OPEN) {
        wsRef.current.send(JSON.stringify({ action: "toggle_audio" }));
      }
    } else {
      const SpeechRecognition = (window as any).SpeechRecognition || (window as any).webkitSpeechRecognition;
      if (SpeechRecognition) {
        const recognition = new SpeechRecognition();
        recognition.lang = 'en-US';
        recognition.interimResults = false;
        recognition.maxAlternatives = 1;

        recognition.onstart = () => {
          setStatus("LISTENING...");
          setTranscript("🔴 Recording browser audio...");
        };
        
        recognition.onresult = (event: any) => {
          const text = event.results[0][0].transcript;
          setTranscript(`Heard: ${text}`);
          if (wsRef.current?.readyState === WebSocket.OPEN) {
            wsRef.current.send(JSON.stringify({ action: "text_query", text }));
          }
        };
        
        recognition.onerror = (event: any) => {
          setStatus("ERROR");
          setTranscript(`Microphone error: ${event.error}`);
        };
        
        recognition.onend = () => {
          setStatus("READY");
        };
        
        recognition.start();
      } else {
        setStatus("ERROR");
        setTranscript("Speech recognition not supported in this browser. Please use Chrome/Edge.");
      }
    }
  };

  // 5. TAURI NATIVE EVENT LISTENERS
  useEffect(() => {
    if (!isTauri()) return;
    let unlisten: () => void;
    
    const setupTauriEvents = async () => {
      try {
        const { listen } = await import('@tauri-apps/api/event');
        unlisten = await listen<string>('app-command', (event) => {
          const command = event.payload; 

          if (command === "TRIGGER_SNIPER") triggerScreenCapture();
          if (command === "TRIGGER_AUDIO") triggerAudio();
          if (command === "SUMMON_COPILOT" || command === "TOGGLE_VISIBILITY") {
            setIsVisible(prev => (command === "SUMMON_COPILOT" ? true : !prev));
          }
          if (command === "OPACITY_UP") setUiOpacity(prev => Math.min(1.0, prev + 0.1));
          if (command === "OPACITY_DOWN") setUiOpacity(prev => Math.max(0.1, prev - 0.1));
        });
      } catch (e) {
        console.error("Failed to setup Tauri events", e);
      }
    };
    
    setupTauriEvents();
    return () => { if (unlisten) unlisten(); };
  }, []);

  // 6. BROWSER KEYBOARD SHORTCUTS FALLBACK
  useEffect(() => {
    if (isTauri()) return;

    const tabHistory: number[] = [];
    const minusHistory: number[] = [];
    let cooldown = 0;

    const handleKeyDown = (e: KeyboardEvent) => {
      const now = Date.now();

      if (e.altKey && e.key.toLowerCase() === 'a') {
        e.preventDefault();
        triggerAudio();
        return;
      }
      if (e.altKey && e.key.toLowerCase() === 'h') {
        e.preventDefault();
        setIsVisible(v => !v);
        return;
      }
      if (e.altKey && e.key === 'ArrowUp') {
        e.preventDefault();
        setUiOpacity(prev => Math.min(1.0, prev + 0.1));
        return;
      }
      if (e.altKey && e.key === 'ArrowDown') {
        e.preventDefault();
        setUiOpacity(prev => Math.max(0.1, prev - 0.1));
        return;
      }

      const trackDoubleTap = (history: number[], keyName: string) => {
        if (now < cooldown) return;
        history.push(now);
        if (history.length > 2) history.shift();
        if (history.length === 2 && history[1] - history[0] <= 500) {
          setTranscript(`📸 Buffered screenshot via ${keyName} (Browser)... Press [ENTER] to capture`);
          history.length = 0;
          cooldown = now + 1000;
          (window as any).__browserBuffered = true;
        }
      };

      if (e.key === 'Tab') trackDoubleTap(tabHistory, 'Tab');
      if (e.key === '-') trackDoubleTap(minusHistory, '-');

      if (e.key === 'Enter') {
        if ((window as any).__browserBuffered && now > cooldown) {
          (window as any).__browserBuffered = false;
          cooldown = now + 3000;
          triggerScreenCapture();
        }
      }
    };

    window.addEventListener('keydown', handleKeyDown);
    return () => window.removeEventListener('keydown', handleKeyDown);
  }, []);

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
      if (isTauri()) {
        const { writeText } = await import('@tauri-apps/plugin-clipboard-manager');
        await writeText(sessionCode);
      } else {
        if (navigator.clipboard) {
          await navigator.clipboard.writeText(sessionCode);
        } else {
          throw new Error("Clipboard API not available in browser");
        }
      }
      setTranscript(`✅ Copied Session Code: ${sessionCode}`);
    } catch (err) {
      console.error("Failed to copy to clipboard:", err);
      setTranscript(`❌ Failed to copy code`);
    }
    setTimeout(() => setTranscript("Awaiting Double-Tab / Double-[-] snipe or audio input..."), 3000);
  };

  const handleHeaderDrag = async () => {
    if (isTauri()) {
      const { getCurrentWindow } = await import('@tauri-apps/api/window');
      getCurrentWindow().startDragging();
    }
  };

  return (
    <main 
      className="h-screen w-screen p-3 select-none font-mono relative overflow-hidden"
      style={{ opacity: isVisible ? uiOpacity : 0, visibility: isVisible ? 'visible' : 'hidden', backgroundColor: 'transparent' }}
    >
      <div className="h-full w-full bg-slate-950 border-2 border-slate-700/80 rounded-xl p-4 flex flex-col justify-between text-white relative shadow-2xl">
        
        {/* HEADER BAR */}
        <div onPointerDown={handleHeaderDrag} className={`flex items-center justify-between border-b border-slate-700 pb-2 ${isTauri() ? "cursor-pointer" : ""}`}>
          <div className="flex items-center gap-2 pointer-events-none">
            <span className="h-2.5 w-2.5 rounded-full bg-emerald-400 animate-pulse" />
            <span className="text-xs font-bold tracking-widest text-white uppercase flex items-center">
              PROTOCOL v2.0 ENGINE
              <span className="ml-2 text-[9px] px-1 py-0.5 rounded bg-slate-800 text-slate-400 border border-slate-700">
                {isTauri() ? "DESKTOP" : "WEB"}
              </span>
            </span>
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