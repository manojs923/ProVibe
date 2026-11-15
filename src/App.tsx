import { useState, useEffect, useRef } from "react";
import { createClient, SupabaseClient } from '@supabase/supabase-js';
import { invoke } from '@tauri-apps/api/core';

const urlsEnv = import.meta.env.VITE_SUPABASE_URLS || import.meta.env.VITE_SUPABASE_URL || "";
const keysEnv = import.meta.env.VITE_SUPABASE_KEYS || import.meta.env.VITE_SUPABASE_ANON_KEY || "";

const supabaseUrls: string[] = urlsEnv.split(',').map((u: string) => u.trim()).filter((u: string) => u.length > 0);
const supabaseKeys: string[] = keysEnv.split(',').map((k: string) => k.trim()).filter((k: string) => k.length > 0);

const supabaseClients: SupabaseClient[] = [];

supabaseUrls.forEach((url: string, index: number) => {
  const key = supabaseKeys[index];
  if (url && key) {
    try {
      supabaseClients.push(createClient(url, key));
    } catch (err) {
      console.error(`[⚠️] Failed to init UI DB client for ${url}:`, err);
    }
  }
});

if (supabaseClients.length === 0) {
  console.error("CRITICAL: Supabase env vars missing — realtime sync disabled.");
}

const REMOTE_VIEWER = "https://copilate-frontend.vercel.app/";

interface WebSocketPayload {
  status: 'loading' | 'success' | 'error' | 'config_updated' | 'sync_cursor' | 'nav_up' | 'nav_down';
  data?: string;
  transcript?: string;
  active_line?: number;
}

interface TriggerConfig {
  summon_double_tap: string;
  sniper_double_tap: string;
  flush_double_tap: string;
  audio_double_tap: string;
  toggle_double_tap: string;
  opacity_up_double_tap: string;
  opacity_down_double_tap: string;
  double_tap_window_ms: number;
  cooldown_min_ms: number;
  cooldown_max_ms: number;
}

const FALLBACK_KEYS: TriggerConfig = {
  summon_double_tap: "CapsLock",
  sniper_double_tap: "-",
  flush_double_tap: "Return",
  audio_double_tap: ".",
  toggle_double_tap: "ScrollLock",
  opacity_up_double_tap: "=",
  opacity_down_double_tap: "[",
  double_tap_window_ms: 400,
  cooldown_min_ms: 1500,
  cooldown_max_ms: 2000,
};

const isTauri = () =>
  typeof window !== 'undefined' &&
  ('__TAURI__' in window || '__TAURI_INTERNALS__' in window || '__TAURI_IPC__' in window);

const displayKey = (k: string) => (k === "Return" ? "ENTER" : k === "CapsLock" ? "CAPS" : k);

export default function App() {
  const [sessionCode, setSessionCode] = useState("");
  const [status, setStatus] = useState("STANDBY");
  const [transcript, setTranscript] = useState("Awaiting diagnostic trigger...");
  const [response, setResponse] = useState("// agent online — press the summon key.");
  const [keys, setKeys] = useState<TriggerConfig>(FALLBACK_KEYS);

  const [isVisible, setIsVisible] = useState(true);
  const [uiOpacity, setUiOpacity] = useState(0.95);
  const [captureMode, setCaptureMode] = useState<"VM" | "HOST">("VM");

  const [activeLine, setActiveLine] = useState<number | null>(null);
  const activeLineRef = useRef<HTMLDivElement>(null);
  const responseRef = useRef(response);

  const [showSettings, setShowSettings] = useState(false);
  const [apiKey, setApiKey] = useState("");
  const [resumeText, setResumeText] = useState("");
  const [taskMode, setTaskMode] = useState<"AUTO" | "MCQ" | "CODING" | "SQL" | "BEHAVIORAL">("AUTO");

  const [targetBrowser, setTargetBrowser] = useState<"SEB" | "MSB">("SEB");
  const [vmStatus, setVmStatus] = useState<"OFFLINE" | "BOOTING" | "HOOKED">("OFFLINE");

  const wsRef = useRef<WebSocket | null>(null);
  const taskModeRef = useRef(taskMode);
  const captureModeRef = useRef(captureMode);
  const imageBufferRef = useRef<string[]>([]);

  useEffect(() => { taskModeRef.current = taskMode; }, [taskMode]);
  useEffect(() => { captureModeRef.current = captureMode; }, [captureMode]);
  useEffect(() => { responseRef.current = response; }, [response]);

  useEffect(() => {
    const code = Math.random().toString(36).substring(2, 8).toUpperCase();
    setSessionCode(code);
  }, []);

  useEffect(() => {
    const saved = localStorage.getItem('protocol_ui_opacity');
    if (saved) setUiOpacity(parseFloat(saved));
  }, []);

  useEffect(() => {
    localStorage.setItem('protocol_ui_opacity', uiOpacity.toString());
  }, [uiOpacity]);

  // ── Trigger legend, sourced from trigger_config.json ──────────
  useEffect(() => {
    if (!isTauri()) return;
    let unlisten: (() => void) | undefined;

    (async () => {
      try {
        const cfg = await invoke<TriggerConfig>("get_trigger_config");
        setKeys(cfg);
        const { listen } = await import('@tauri-apps/api/event');
        unlisten = await listen<TriggerConfig>('trigger-config', (e) => setKeys(e.payload));
      } catch (err) {
        console.error("trigger config unavailable", err);
      }
    })();

    return () => { if (unlisten) unlisten(); };
  }, []);

  // ── Supabase realtime ─────────────────────────────────────────
  useEffect(() => {
    if (!sessionCode || supabaseClients.length === 0) return;

    const channels = supabaseClients.map((client, index) =>
      client
        .channel(`listen-answers-${sessionCode}-${index}`)
        .on('postgres_changes',
          { event: 'INSERT', schema: 'public', table: 'copilot_sessions', filter: `session_code=eq.${sessionCode}` },
          (payload) => {
            console.log(`[DB ${index + 1}] payload:`, payload);
            if (payload.new && payload.new.answer_text) {
              setResponse(payload.new.answer_text);
              setStatus("OVERRIDE");
              setTranscript("👤 Remote operator updated the profile.");
            }
          })
        .subscribe());

    return () => { channels.forEach((c, i) => supabaseClients[i].removeChannel(c)); };
  }, [sessionCode]);

  // ── WebSocket ─────────────────────────────────────────────────
  useEffect(() => {
    if (!sessionCode) return;
    let isMounted = true;
    let reconnectTimer: number | undefined;

    const connectWs = () => {
      if (!isMounted) return;
      if (wsRef.current && (wsRef.current.readyState === WebSocket.CONNECTING || wsRef.current.readyState === WebSocket.OPEN)) return;

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

      socket.onerror = (err) => console.error("WebSocket error:", err);

      socket.onmessage = (event: MessageEvent) => {
        if (!isMounted) return;
        try {
          const payload: WebSocketPayload = JSON.parse(event.data);
          if (payload.status === "loading") {
            setStatus("THINKING...");
            setTranscript(payload.transcript || "Processing...");
            setIsVisible(true);
          } else if (payload.status === "success" && payload.data) {
            setStatus("READY");
            setTranscript(payload.transcript || "Processed.");
            setResponse(payload.data);
            setActiveLine(payload.active_line || null);
          } else if (payload.status === "sync_cursor" && payload.active_line !== undefined) {
            setActiveLine(payload.active_line);
          } else if (payload.status === "nav_up") {
            setActiveLine(p => (p === null ? 1 : Math.max(1, p - 1)));
          } else if (payload.status === "nav_down") {
            setActiveLine(p => {
              const max = responseRef.current.split('\n').length;
              return p === null ? 1 : Math.min(max, p + 1);
            });
          } else if (payload.status === "error" && payload.data) {
            setStatus("ERROR");
            setResponse("⚠️ " + payload.data);
          } else if (payload.status === "config_updated") {
            setStatus("CONFIG SAVED");
            setTimeout(() => { if (isMounted) setStatus("READY"); }, 2000);
          }
        } catch (err) {
          console.error("bad socket payload", err);
        }
      };
    };

    connectWs();

    return () => {
      isMounted = false;
      if (reconnectTimer) clearTimeout(reconnectTimer);
      if (wsRef.current) {
        wsRef.current.onclose = null;
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

  // ── Actions ───────────────────────────────────────────────────
  const engageHypervisor = async () => {
    if (!isTauri()) return;
    try {
      setVmStatus("BOOTING");
      setTranscript(`initiating hyper-v container for ${targetBrowser}...`);
      await invoke("launch_stealth_vm", { target: targetBrowser });
      setVmStatus("HOOKED");
      setCaptureMode("VM");
      setTranscript(`loopback established. ${targetBrowser} sandboxed.`);
    } catch (err) {
      console.error(err);
      setVmStatus("OFFLINE");
      setTranscript("critical failure: hypervisor hook rejected.");
    }
  };

  const captureFrame = async () => {
    if (!isTauri()) {
      (window as any).__browserBuffered = true;
      setTranscript("📸 browser mode — press ENTER to capture + send");
      return;
    }
    try {
      const b64: string = captureModeRef.current === "VM"
        ? await invoke("capture_rdp_window", { ip: "127.0.0.2" })
        : await invoke("capture_screen_native");
      imageBufferRef.current.push(`data:image/jpeg;base64,${b64}`);
      setTranscript(`📸 buffered ${imageBufferRef.current.length} frame(s) — press ENTER to send`);
    } catch (err) {
      console.error(err);
      setTranscript(captureModeRef.current === "VM"
        ? "capture failed: is the console window open?"
        : "capture failed.");
    }
  };

  const flushBuffer = async () => {
    const mode = taskModeRef.current;

    if (isTauri()) {
      if (wsRef.current?.readyState !== WebSocket.OPEN) {
        setStatus("ERROR");
        setTranscript("backend disconnected.");
        return;
      }
      if (imageBufferRef.current.length === 0) {
        setTranscript("buffer empty — capture a frame first.");
        return;
      }
      setStatus("UPLOADING...");
      setTranscript(`transmitting ${imageBufferRef.current.length} frame(s)...`);
      wsRef.current.send(JSON.stringify({
        action: "browser_image_batch_upload",
        images: imageBufferRef.current,
        mode,
      }));
      imageBufferRef.current = [];
      setStatus("READY");
      setTranscript("frames synced.");
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
          wsRef.current.send(JSON.stringify({ action: "browser_image_batch_upload", images: [dataUrl], mode }));
        }
        setStatus("READY");
      } catch (err) {
        console.error(err);
        setStatus("ERROR");
        setTranscript("screen capture denied.");
      } finally {
        stream?.getTracks().forEach(t => t.stop());
      }
    }
  };

  const triggerAudio = async () => {
    if (isTauri()) {
      if (wsRef.current?.readyState === WebSocket.OPEN) {
        wsRef.current.send(JSON.stringify({ action: "toggle_audio" }));
      }
      return;
    }
    const SR = (window as any).SpeechRecognition || (window as any).webkitSpeechRecognition;
    if (!SR) {
      setStatus("ERROR");
      setTranscript("speech recognition unavailable — use Chrome/Edge.");
      return;
    }
    const rec = new SR();
    rec.lang = 'en-US';
    rec.interimResults = false;
    rec.maxAlternatives = 1;
    rec.onstart = () => { setStatus("LISTENING..."); setTranscript("🔴 recording..."); };
    rec.onresult = (e: any) => {
      const text = e.results[0][0].transcript;
      setTranscript(`heard: ${text}`);
      if (wsRef.current?.readyState === WebSocket.OPEN) {
        wsRef.current.send(JSON.stringify({ action: "text_query", text }));
      }
    };
    rec.onerror = (e: any) => { setStatus("ERROR"); setTranscript(`mic error: ${e.error}`); };
    rec.onend = () => setStatus("READY");
    rec.start();
  };

  // ── Native event bus ──────────────────────────────────────────
  useEffect(() => {
    if (!isTauri()) return;
    let unlisten: (() => void) | undefined;

    (async () => {
      try {
        const { listen } = await import('@tauri-apps/api/event');
        unlisten = await listen<string>('app-command', (event) => {
          const c = event.payload;
          if (c === "TRIGGER_SNIPER") captureFrame();
          if (c === "FLUSH_BUFFER") flushBuffer();
          if (c === "TRIGGER_AUDIO") triggerAudio();
          if (c === "SUMMON_COPILOT") setIsVisible(true);
          if (c === "TOGGLE_VISIBILITY") setIsVisible(v => !v);
          if (c === "OPACITY_UP") setUiOpacity(p => Math.min(1.0, p + 0.1));
          if (c === "OPACITY_DOWN") setUiOpacity(p => Math.max(0.1, p - 0.1));
        });
      } catch (e) {
        console.error("event setup failed", e);
      }
    })();

    return () => { if (unlisten) unlisten(); };
  }, []);

  // ── Browser-mode keyboard mirror ──────────────────────────────
  useEffect(() => {
    if (isTauri()) return;
    const last: Record<string, number> = {};

    const onKey = (e: KeyboardEvent) => {
      const now = Date.now();
      const k = e.key;
      const map: Record<string, string> = {
        [keys.summon_double_tap]: "SUMMON_COPILOT",
        [keys.sniper_double_tap]: "TRIGGER_SNIPER",
        [keys.flush_double_tap]: "FLUSH_BUFFER",
        [keys.audio_double_tap]: "TRIGGER_AUDIO",
        [keys.toggle_double_tap]: "TOGGLE_VISIBILITY",
        [keys.opacity_up_double_tap]: "OPACITY_UP",
        [keys.opacity_down_double_tap]: "OPACITY_DOWN",
      };
      const cmd = map[k];
      if (!cmd) return;

      e.preventDefault();
      const prev = last[k] ?? 0;
      if (now - prev <= keys.double_tap_window_ms) {
        last[k] = 0;
        if (cmd === "TRIGGER_SNIPER") captureFrame();
        if (cmd === "FLUSH_BUFFER") flushBuffer();
        if (cmd === "TRIGGER_AUDIO") triggerAudio();
        if (cmd === "SUMMON_COPILOT") setIsVisible(true);
        if (cmd === "TOGGLE_VISIBILITY") setIsVisible(v => !v);
        if (cmd === "OPACITY_UP") setUiOpacity(p => Math.min(1.0, p + 0.1));
        if (cmd === "OPACITY_DOWN") setUiOpacity(p => Math.max(0.1, p - 0.1));
      } else {
        last[k] = now;
      }
    };

    window.addEventListener('keydown', onKey, { capture: true });
    return () => window.removeEventListener('keydown', onKey, { capture: true });
  }, [keys]);

  const saveSettings = () => {
    if (wsRef.current?.readyState === WebSocket.OPEN) {
      wsRef.current.send(JSON.stringify({ action: "update_config", api_key: apiKey, resume_text: resumeText }));
      setShowSettings(false);
    }
  };

  const copySessionCode = async () => {
    try {
      await navigator.clipboard.writeText(sessionCode);
      setTranscript(`✅ copied ${sessionCode}`);
    } catch {
      setTranscript("❌ clipboard blocked");
    }
    setTimeout(() => setTranscript("Awaiting diagnostic trigger..."), 3000);
  };

  const handleHeaderDrag = async (e: React.PointerEvent) => {
    const target = e.target as HTMLElement;
    if (target.closest("button, select, input, textarea, a")) return;
    if (!isTauri()) return;
    const { getCurrentWindow } = await import('@tauri-apps/api/window');
    getCurrentWindow().startDragging();
  };

  const reloadKeys = async () => {
    if (!isTauri()) return;
    try {
      const cfg = await invoke<TriggerConfig>("reload_trigger_config");
      setKeys(cfg);
    } catch (err) {
      console.error(err);
    }
  };

  const Hint = ({ k, label }: { k: string; label: string }) => (
    <span className="flex items-center gap-1 whitespace-nowrap">
      <kbd className="px-1 py-[1px] rounded bg-slate-800 border border-slate-600 text-slate-200 text-[9px]">
        {displayKey(k)}
      </kbd>
      <span className="text-slate-600 text-[9px]">x2</span>
      <span className="text-slate-400 text-[9px] uppercase tracking-wide">{label}</span>
    </span>
  );

  return (
    <main
      className="h-screen w-screen p-3 select-none font-mono relative overflow-hidden"
      style={{ opacity: isVisible ? uiOpacity : 0, backgroundColor: 'transparent' }}
    >
      <div className="h-full w-full bg-slate-950 border-2 border-slate-700/80 rounded-xl p-4 flex flex-col justify-between text-white relative shadow-2xl">

        <div onPointerDown={handleHeaderDrag} className={`flex items-center justify-between border-b border-slate-700 pb-2 ${isTauri() ? "cursor-grab" : ""}`}>
          <div className="flex items-center gap-2 pointer-events-none">
            <span className="h-2.5 w-2.5 rounded-full bg-emerald-400 animate-pulse" />
            <span className="text-xs font-bold tracking-widest text-white uppercase flex items-center">
              TELEMETRY
              <span className="ml-2 text-[9px] px-1 py-0.5 rounded bg-slate-800 text-slate-400 border border-slate-700">
                {isTauri() ? "NATIVE" : "WEB"}
              </span>
            </span>
          </div>

          <div className="flex items-center gap-2 bg-slate-900 border border-slate-700 rounded pl-3 pr-1 py-0.5">
            <span className="text-[10px] font-bold text-slate-400">ID:</span>
            <span className="text-[11px] font-bold text-emerald-400 tracking-wider">{sessionCode || "GEN..."}</span>
            <button onClick={copySessionCode} tabIndex={-1} className="ml-2 text-[9px] bg-slate-800 hover:bg-slate-700 text-white px-2 py-0.5 rounded">
              COPY
            </button>
          </div>

          <div className="flex items-center gap-2">
            <select
              value={targetBrowser}
              onChange={(e) => setTargetBrowser(e.target.value as any)}
              tabIndex={-1}
              className="bg-slate-900 border border-slate-700 text-[10px] text-purple-400 font-bold rounded px-1 py-0.5 outline-none"
            >
              <option value="SEB">SEB</option>
              <option value="MSB">MSB</option>
            </select>

            <button
              onClick={engageHypervisor}
              disabled={vmStatus !== "OFFLINE"}
              tabIndex={-1}
              className={`text-[10px] font-bold px-2 py-0.5 rounded border ${
                vmStatus === "OFFLINE" ? "bg-red-900/50 text-red-400 border-red-700" :
                vmStatus === "BOOTING" ? "bg-yellow-900/50 text-yellow-400 border-yellow-700" :
                "bg-purple-900/50 text-purple-400 border-purple-700"
              }`}
            >
              {vmStatus === "OFFLINE" ? "ENGAGE VM" : vmStatus}
            </button>

            <select
              value={captureMode}
              onChange={(e) => setCaptureMode(e.target.value as any)}
              tabIndex={-1}
              className="bg-slate-900 border border-slate-700 text-[10px] text-sky-400 font-bold rounded px-1 py-0.5 outline-none"
              title="VM captures only the console window. HOST captures the full screen."
            >
              <option value="VM">CAP: VM</option>
              <option value="HOST">CAP: HOST</option>
            </select>

            <select
              value={taskMode}
              onChange={(e) => setTaskMode(e.target.value as any)}
              tabIndex={-1}
              className="bg-slate-900 border border-slate-700 text-[10px] text-emerald-400 font-bold rounded px-1 py-0.5 outline-none"
            >
              <option value="AUTO">AUTO</option>
              <option value="MCQ">DIAGNOSTIC</option>
              <option value="CODING">CODE</option>
              <option value="SQL">SQL</option>
              <option value="BEHAVIORAL">SYSTEM</option>
            </select>

            <button onClick={() => setShowSettings(!showSettings)} tabIndex={-1} className="text-[10px] bg-slate-800 hover:bg-slate-700 text-white font-bold px-2 py-0.5 rounded border border-slate-600">
              CFG
            </button>

            <span className="text-[10px] font-bold px-2 py-0.5 rounded bg-slate-800 text-emerald-400 border border-emerald-500/50 pointer-events-none">
              {status}
            </span>
          </div>
        </div>

        {showSettings && (
          <div className="absolute inset-x-4 top-14 bottom-12 bg-slate-900/95 backdrop-blur-md border border-slate-700 rounded-lg p-4 z-50 flex flex-col justify-between">
            <div>
              <h3 className="text-xs font-bold text-emerald-400 mb-2 uppercase tracking-wider">Engine Configuration</h3>
              <label className="text-[10px] text-slate-400 block mb-1">Upstream routing key:</label>
              <input
                type="password"
                placeholder="AIzaSy..."
                value={apiKey}
                onChange={(e) => setApiKey(e.target.value)}
                tabIndex={-1}
                className="w-full bg-slate-950 border border-slate-700 text-white text-xs p-2 rounded mb-3 outline-none focus:border-emerald-500"
              />
              <label className="text-[10px] text-slate-400 block mb-1">Context parameters:</label>
              <textarea
                rows={4}
                placeholder="Paste environment context here..."
                value={resumeText}
                onChange={(e) => setResumeText(e.target.value)}
                tabIndex={-1}
                className="w-full bg-slate-950 border border-slate-700 text-white text-xs p-2 rounded outline-none focus:border-emerald-500 font-sans resize-none"
              />
              <div className="mt-3 text-[9px] text-slate-500 leading-relaxed">
                Keys live in <span className="text-slate-300">trigger_config.json</span> beside the exe.
                <button onClick={reloadKeys} tabIndex={-1} className="ml-2 text-emerald-400 underline">reload now</button>
              </div>
              <a
                href={REMOTE_VIEWER}
                target="_blank"
                rel="noreferrer"
                tabIndex={-1}
                className="mt-2 inline-block text-[10px] text-sky-400 underline"
              >
                open remote viewer ↗
              </a>
            </div>
            <div className="flex gap-2">
              <button onClick={saveSettings} tabIndex={-1} className="flex-1 bg-emerald-600 hover:bg-emerald-500 text-white text-xs font-bold py-2 rounded uppercase">Save</button>
              <button onClick={() => setShowSettings(false)} tabIndex={-1} className="bg-slate-800 text-slate-300 text-xs font-bold px-4 py-2 rounded">Cancel</button>
            </div>
          </div>
        )}

        <div className="my-2 p-2 bg-slate-900 rounded-lg border border-slate-700 text-xs">
          <p className="text-emerald-400 font-medium italic truncate">{transcript}</p>
        </div>

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

        {/* KEY LEGEND — always matches trigger_config.json */}
        <div className="mt-2 pt-2 border-t border-slate-800 flex flex-wrap items-center gap-x-3 gap-y-1">
          <Hint k={keys.summon_double_tap} label="summon" />
          <Hint k={keys.sniper_double_tap} label="grab" />
          <Hint k={keys.flush_double_tap} label="send" />
          <Hint k={keys.audio_double_tap} label="audio" />
          <Hint k={keys.toggle_double_tap} label="hide" />
          <Hint k={keys.opacity_up_double_tap} label="dim+" />
          <Hint k={keys.opacity_down_double_tap} label="dim-" />
          <span className="ml-auto text-[9px] text-slate-600 uppercase">{captureMode} cap</span>
        </div>

      </div>
    </main>
  );
}