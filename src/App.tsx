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
      console.error(`[WARN] Failed to init UI DB client for ${url}:`, err);
    }
  }
});

if (supabaseClients.length === 0) {
  console.error("CRITICAL: Supabase env vars missing — realtime sync disabled.");
}

const REMOTE_VIEWER = "https://copilate-frontend.vercel.app/";
const MIN_W = 360;
const MIN_H = 240;

/** Must match DEFAULT_VM_NAME / PROTOCOL_VM_NAME on the Rust side. */
const VM_NAME = (import.meta.env as any).VITE_VM_NAME || "Test-Nartan-Build";

/** Status poll cadence and ceiling. 90 s is past a cold sysprep boot. */
const VM_POLL_MS = 1500;
const VM_POLL_MAX_TICKS = 60;

type Destination = "AI" | "FRD";

interface WebSocketPayload {
  status:
    | 'loading'
    | 'success'
    | 'error'
    | 'config_updated'
    | 'sync_cursor'
    | 'nav_up'
    | 'nav_down'
    | 'awaiting_friend';
  data?: string;
  transcript?: string;
  active_line?: number;
}

interface VmStatusPayload {
  running: boolean;
  vm_name: string;
  state: string;
  uptime_seconds: number;
  cpu_usage: number;
  memory_assigned_mb: number;
  memory_usage_mb: number;
}

interface TriggerConfig {
  version: number;
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
  version: 2,
  summon_double_tap: "CapsLock",
  sniper_double_tap: "/",
  flush_double_tap: "Return",
  audio_double_tap: ".",
  toggle_double_tap: "-",
  opacity_up_double_tap: "=",
  opacity_down_double_tap: "[",
  double_tap_window_ms: 400,
  cooldown_min_ms: 800,
  cooldown_max_ms: 1200,
};

const isTauri = () =>
  typeof window !== 'undefined' &&
  ('__TAURI__' in window || '__TAURI_INTERNALS__' in window || '__TAURI_IPC__' in window);

const labelKey = (raw: string) => {
  if (!raw) return "?";
  if (raw === "Return") return "ENTER";
  if (raw === "CapsLock") return "CAPS";
  if (raw === "ScrollLock") return "SCRLK";
  if (raw === "Tab") return "TAB";
  if (raw === "BackQuote") return "`";
  return raw.toUpperCase();
};

// ── Icons (inline SVG, currentColor, no emoji) ──────────────────
const IconScan = () => (
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round" className="h-3.5 w-3.5">
    <path d="M4 8V6a2 2 0 0 1 2-2h2" />
    <path d="M20 8V6a2 2 0 0 0-2-2h-2" />
    <path d="M4 16v2a2 2 0 0 0 2 2h2" />
    <path d="M20 16v2a2 2 0 0 1-2 2h-2" />
    <circle cx="12" cy="12" r="2.5" />
  </svg>
);

const IconChip = () => (
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round" className="h-3.5 w-3.5">
    <rect x="7" y="7" width="10" height="10" rx="2" />
    <path d="M10 3v4M14 3v4M10 17v4M14 17v4M3 10h4M3 14h4M17 10h4M17 14h4" />
  </svg>
);

const IconPeople = () => (
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round" className="h-3.5 w-3.5">
    <circle cx="9" cy="8" r="3" />
    <path d="M3.5 19a5.5 5.5 0 0 1 11 0" />
    <path d="M16 6.5a2.5 2.5 0 0 1 0 5" />
    <path d="M17.5 19a4.5 4.5 0 0 0-2.2-3.9" />
  </svg>
);

const IconSend = () => (
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round" className="h-3.5 w-3.5">
    <path d="M12 19V5" />
    <path d="M6 11l6-6 6 6" />
  </svg>
);

const IconGrip = () => (
  <svg viewBox="0 0 24 24" fill="currentColor" className="h-3.5 w-3.5">
    <circle cx="9" cy="7" r="1.4" />
    <circle cx="15" cy="7" r="1.4" />
    <circle cx="9" cy="12" r="1.4" />
    <circle cx="15" cy="12" r="1.4" />
    <circle cx="9" cy="17" r="1.4" />
    <circle cx="15" cy="17" r="1.4" />
  </svg>
);

const IconResize = () => (
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round" className="h-3.5 w-3.5">
    <path d="M20 14v6h-6" />
    <path d="M20 20 13 13" />
    <path d="M4 10V4h6" />
    <path d="M4 4l7 7" />
  </svg>
);

const IconMinus = () => (
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.2" strokeLinecap="round" className="h-3.5 w-3.5">
    <path d="M6 12h12" />
  </svg>
);

export default function App() {
  const [sessionCode, setSessionCode] = useState("");
  const [status, setStatus] = useState("STANDBY");
  const [transcript, setTranscript] = useState("Awaiting diagnostic trigger...");
  const [response, setResponse] = useState("// agent online — press the summon key.");
  const [keys, setKeys] = useState<TriggerConfig>(FALLBACK_KEYS);

  const [uiOpacity, setUiOpacity] = useState(0.95);
  const [captureMode, setCaptureMode] = useState<"VM" | "HOST">("VM");
  const [destination, setDestination] = useState<Destination>("AI");
  const [buffered, setBuffered] = useState(0);

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
  const destinationRef = useRef<Destination>(destination);
  const targetBrowserRef = useRef(targetBrowser);
  const imageBufferRef = useRef<string[]>([]);
  const vmPollRef = useRef<number | undefined>(undefined);

  useEffect(() => { taskModeRef.current = taskMode; }, [taskMode]);
  useEffect(() => { captureModeRef.current = captureMode; }, [captureMode]);
  useEffect(() => { destinationRef.current = destination; }, [destination]);
  useEffect(() => { responseRef.current = response; }, [response]);
  useEffect(() => { targetBrowserRef.current = targetBrowser; }, [targetBrowser]);

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

  // ── Trigger config, for the browser-mode mirror ───────────────
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
              setTranscript("remote operator updated the profile.");
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
          } else if (payload.status === "awaiting_friend") {
            setStatus("AWAITING");
            setTranscript(payload.transcript || "frames with operator.");
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
            setResponse("[!] " + payload.data);
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

  // ── Focus borrowing ───────────────────────────────────────────
  //
  // The window carries WS_EX_NOACTIVATE so a click never steals the exam's
  // focus. A text field still needs real keyboard focus, so it is borrowed for
  // as long as the field is focused and handed straight back on blur.
  const borrowFocus = async () => {
    if (!isTauri()) return;
    try { await invoke("begin_text_input"); } catch (e) { console.error(e); }
  };

  const releaseFocus = async () => {
    if (!isTauri()) return;
    try { await invoke("end_text_input"); } catch (e) { console.error(e); }
  };

  // ── VM status watch ───────────────────────────────────────────
  const stopVmWatch = () => {
    if (vmPollRef.current) {
      clearInterval(vmPollRef.current);
      vmPollRef.current = undefined;
    }
  };

  const settleHooked = (label: string) => {
    stopVmWatch();
    setVmStatus("HOOKED");
    setCaptureMode("VM");
    setTranscript(`loopback established. ${label} sandboxed.`);
  };

  /**
   * ENGAGE returns as soon as Start-VM is issued, so the transition to Running
   * is watched here. Polling beats a blocking invoke: the overlay stays usable
   * and the transcript shows real state instead of a frozen "please wait".
   */
  const startVmWatch = () => {
    stopVmWatch();
    let ticks = 0;

    vmPollRef.current = window.setInterval(async () => {
      ticks += 1;
      try {
        const s = await invoke<VmStatusPayload>("get_vm_status", { vmName: VM_NAME });
        if (s.running) {
          settleHooked(targetBrowserRef.current);
          return;
        }
        if (s.state === "Starting" || s.state === "Off") {
          setTranscript(`booting ${VM_NAME} — ${s.state.toLowerCase()} (${ticks})`);
        }
      } catch (err) {
        console.warn("status poll failed", err);
      }

      if (ticks >= VM_POLL_MAX_TICKS) {
        stopVmWatch();
        setVmStatus("OFFLINE");
        setTranscript("boot watch timed out — inspect the console window.");
      }
    }, VM_POLL_MS);
  };

  // Reconcile with a guest that is already up when the overlay opens.
  useEffect(() => {
    if (!isTauri()) return;
    let cancelled = false;

    (async () => {
      try {
        const s = await invoke<VmStatusPayload>("get_vm_status", { vmName: VM_NAME });
        if (!cancelled && s.running) settleHooked(targetBrowserRef.current);
      } catch { /* guest simply is not there */ }
    })();

    return () => { cancelled = true; stopVmWatch(); };
  }, []);

  // ── Actions ───────────────────────────────────────────────────
  const engageHypervisor = async () => {
    if (!isTauri()) return;
    try {
      setVmStatus("BOOTING");
      setTranscript(`initiating hyper-v container for ${targetBrowser}...`);
      const msg = await invoke<string>("launch_stealth_vm", { target: targetBrowser });
      setTranscript(String(msg));
      startVmWatch();
    } catch (err) {
      console.error(err);
      stopVmWatch();
      setVmStatus("OFFLINE");
      setTranscript(`critical failure: ${String(err)}`);
    }
  };

  const captureFrame = async () => {
    if (!isTauri()) {
      (window as any).__browserBuffered = true;
      setTranscript("browser mode — press ENTER to capture and send");
      return;
    }

    const wantVm = captureModeRef.current === "VM";

    try {
      let b64: string;
      if (wantVm) {
        try {
          b64 = await invoke("capture_rdp_window", { ip: "127.0.0.2" });
        } catch (vmErr) {
          console.warn("console capture failed, falling back to host", vmErr);
          b64 = await invoke("capture_screen_native");
          setTranscript("no console window — captured monitor instead");
        }
      } else {
        b64 = await invoke("capture_screen_native");
      }

      imageBufferRef.current.push(`data:image/jpeg;base64,${b64}`);
      setBuffered(imageBufferRef.current.length);
      setTranscript(`held ${imageBufferRef.current.length} frame(s) — press send`);
    } catch (err) {
      console.error(err);
      setTranscript("capture failed.");
    }
  };

  const flushBuffer = async () => {
    const mode = taskModeRef.current;
    const route = destinationRef.current;

    if (isTauri()) {
      if (wsRef.current?.readyState !== WebSocket.OPEN) {
        setStatus("ERROR");
        setTranscript("backend disconnected — retrying");
        return;
      }
      if (imageBufferRef.current.length === 0) {
        setTranscript("nothing held — capture a frame first.");
        return;
      }
      setStatus("UPLOADING...");
      setTranscript(`transmitting ${imageBufferRef.current.length} frame(s) to ${route}...`);
      wsRef.current.send(JSON.stringify({
        action: "browser_image_batch_upload",
        images: imageBufferRef.current,
        mode,
        route,
      }));
      imageBufferRef.current = [];
      setBuffered(0);
      setStatus(route === "FRD" ? "AWAITING" : "READY");
      setTranscript(route === "FRD" ? "frames with operator." : "frames synced.");
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
          wsRef.current.send(JSON.stringify({ action: "browser_image_batch_upload", images: [dataUrl], mode, route }));
        }
        setStatus(route === "FRD" ? "AWAITING" : "READY");
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
    rec.onstart = () => { setStatus("LISTENING..."); setTranscript("recording..."); };
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
    let unlistenProvisioned: (() => void) | undefined;
    let unlistenFailed: (() => void) | undefined;

    (async () => {
      try {
        const { listen } = await import('@tauri-apps/api/event');
        unlisten = await listen<string>('app-command', (event) => {
          const c = event.payload;
          if (c === "TRIGGER_SNIPER") captureFrame();
          if (c === "FLUSH_BUFFER") flushBuffer();
          if (c === "TRIGGER_AUDIO") triggerAudio();
          if (c === "OPACITY_UP") setUiOpacity(p => Math.min(1.0, p + 0.1));
          if (c === "OPACITY_DOWN") setUiOpacity(p => Math.max(0.1, p - 0.1));
          // SUMMON and TOGGLE are handled at the OS level by Rust.
        });

        unlistenProvisioned = await listen<string>('vm-provisioned', () => {
          setTranscript("guest started — waiting for Running state");
        });

        unlistenFailed = await listen<string>('vm-provision-failed', (event) => {
          stopVmWatch();
          setVmStatus("OFFLINE");
          setTranscript(`engage failed: ${event.payload}`);
        });
      } catch (e) {
        console.error("event setup failed", e);
      }
    })();

    return () => {
      if (unlisten) unlisten();
      if (unlistenProvisioned) unlistenProvisioned();
      if (unlistenFailed) unlistenFailed();
    };
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
      releaseFocus();
    }
  };

  const copyText = async (text: string, label: string) => {
    try {
      await navigator.clipboard.writeText(text);
      setTranscript(label);
    } catch {
      setTranscript(`clipboard blocked — ${text}`);
    }
    setTimeout(() => setTranscript("Awaiting diagnostic trigger..."), 4000);
  };

  const shareLink = () => {
    const url = `${REMOTE_VIEWER}?session=${sessionCode}`;
    copyText(url, `share link copied — ${url}`);
  };

  const startDrag = async () => {
    if (!isTauri()) return;
    const { getCurrentWindow } = await import('@tauri-apps/api/window');
    getCurrentWindow().startDragging();
  };

  /**
   * Manual resize. The window is declared `resizable: false` so Windows never
   * paints its own edge cursor; size is driven from pointer movement instead.
   */
  const startResize = async (e: React.PointerEvent) => {
    if (!isTauri()) return;
    e.preventDefault();

    const { getCurrentWindow } = await import('@tauri-apps/api/window');
    const { LogicalSize } = await import('@tauri-apps/api/dpi');
    const win = getCurrentWindow();

    const scale = await win.scaleFactor();
    const size = await win.outerSize();
    const baseW = size.width / scale;
    const baseH = size.height / scale;
    const originX = e.screenX;
    const originY = e.screenY;

    const onMove = (ev: PointerEvent) => {
      const w = Math.max(MIN_W, Math.round(baseW + (ev.screenX - originX)));
      const h = Math.max(MIN_H, Math.round(baseH + (ev.screenY - originY)));
      win.setSize(new LogicalSize(w, h));
    };
    const onUp = () => {
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerup", onUp);
    };

    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerup", onUp);
  };

  const handleHeaderDrag = async (e: React.PointerEvent) => {
    const target = e.target as HTMLElement;
    if (target.closest("button, select, input, textarea, a")) return;
    if (!isTauri()) return;
    startDrag();
  };

  const toggleHidden = async () => {
    if (!isTauri()) return;
    await invoke("toggle_overlay");
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

  const destActive = (d: Destination) =>
    destination === d
      ? d === "AI"
        ? "bg-emerald-700 text-white border-emerald-500"
        : "bg-sky-700 text-white border-sky-500"
      : "bg-slate-900 text-slate-500 border-slate-700";

  return (
    <main
      className="h-screen w-screen p-3 select-none font-mono relative overflow-hidden"
      style={{ opacity: uiOpacity, backgroundColor: 'transparent' }}
    >
      <div className="h-full w-full bg-slate-950 border-2 border-slate-700/80 rounded-xl p-3 flex flex-col justify-between text-white relative shadow-2xl">

        <div onPointerDown={handleHeaderDrag} className="flex items-center justify-between border-b border-slate-700 pb-2">
          <div className="flex items-center gap-2 pointer-events-none">
            <span className="h-2.5 w-2.5 rounded-full bg-emerald-400 animate-pulse" />
            <span className="text-xs font-bold tracking-widest text-white uppercase flex items-center">
              PROTOCOL
              <span className="ml-2 text-[9px] px-1 py-0.5 rounded bg-slate-800 text-slate-400 border border-slate-700">
                {isTauri() ? "NATIVE" : "WEB"}
              </span>
            </span>
          </div>

          <div className="flex items-center gap-1.5 bg-slate-900 border border-slate-700 rounded pl-2 pr-1 py-0.5">
            <span className="text-[10px] font-bold text-slate-400">ID:</span>
            <span className="text-[11px] font-bold text-emerald-400 tracking-wider">{sessionCode || "GEN..."}</span>
            <button onClick={() => copyText(sessionCode, `copied ${sessionCode}`)} tabIndex={-1} className="text-[9px] bg-slate-800 hover:bg-slate-700 text-white px-1.5 py-0.5 rounded">
              COPY
            </button>
            <button onClick={shareLink} tabIndex={-1} className="text-[9px] bg-sky-800 hover:bg-sky-700 text-white px-1.5 py-0.5 rounded font-bold">
              SHARE
            </button>
          </div>

          <div className="flex items-center gap-1.5">
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
              {vmStatus === "OFFLINE" ? "ENGAGE VM" : vmStatus === "BOOTING" ? "BOOTING" : "HOOKED"}
            </button>

            <div className="flex rounded border border-slate-700 overflow-hidden">
              <button
                onClick={() => setCaptureMode("VM")}
                tabIndex={-1}
                className={`text-[10px] font-bold px-1.5 py-0.5 ${captureMode === "VM" ? "bg-sky-700 text-white" : "bg-slate-900 text-slate-500"}`}
              >
                VM
              </button>
              <button
                onClick={() => setCaptureMode("HOST")}
                tabIndex={-1}
                className={`text-[10px] font-bold px-1.5 py-0.5 ${captureMode === "HOST" ? "bg-sky-700 text-white" : "bg-slate-900 text-slate-500"}`}
              >
                HOST
              </button>
            </div>

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
                onFocus={borrowFocus}
                onBlur={releaseFocus}
                tabIndex={-1}
                className="w-full bg-slate-950 border border-slate-700 text-white text-xs p-2 rounded mb-3 outline-none focus:border-emerald-500"
              />
              <label className="text-[10px] text-slate-400 block mb-1">Context parameters:</label>
              <textarea
                rows={4}
                placeholder="Paste environment context here..."
                value={resumeText}
                onChange={(e) => setResumeText(e.target.value)}
                onFocus={borrowFocus}
                onBlur={releaseFocus}
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
                open remote viewer
              </a>
            </div>
            <div className="flex gap-2">
              <button onClick={saveSettings} tabIndex={-1} className="flex-1 bg-emerald-600 hover:bg-emerald-500 text-white text-xs font-bold py-2 rounded uppercase">Save</button>
              <button
                onClick={() => { setShowSettings(false); releaseFocus(); }}
                tabIndex={-1}
                className="bg-slate-800 text-slate-300 text-xs font-bold px-4 py-2 rounded"
              >
                Cancel
              </button>
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

        {/* CONTROL STRIP */}
        <div className="mt-2 pt-2 border-t border-slate-800 flex items-center gap-1.5">

          <button
            onClick={captureFrame}
            tabIndex={-1}
            className="relative flex items-center gap-1 text-[10px] font-bold px-2 py-1 rounded border bg-sky-900/60 text-sky-200 border-sky-700 hover:bg-sky-800/70"
          >
            <IconScan />
            SS
            {buffered > 0 && (
              <span className="absolute -top-1.5 -right-1.5 min-w-[14px] h-[14px] px-0.5 rounded-full bg-emerald-500 text-slate-950 text-[9px] font-bold flex items-center justify-center">
                {buffered}
              </span>
            )}
          </button>

          <div className="flex rounded border border-slate-700 overflow-hidden">
            <button
              onClick={() => setDestination("AI")}
              tabIndex={-1}
              className={`flex items-center gap-1 text-[10px] font-bold px-2 py-1 border-r border-slate-700 ${destActive("AI")}`}
            >
              <IconChip />
              AI
            </button>
            <button
              onClick={() => setDestination("FRD")}
              tabIndex={-1}
              className={`flex items-center gap-1 text-[10px] font-bold px-2 py-1 ${destActive("FRD")}`}
            >
              <IconPeople />
              FRD
            </button>
          </div>

          <button
            onClick={flushBuffer}
            tabIndex={-1}
            className="flex items-center gap-1 text-[10px] font-bold px-2 py-1 rounded border bg-emerald-700 text-white border-emerald-500 hover:bg-emerald-600"
          >
            <IconSend />
            SEND
          </button>

          <input
            type="range"
            min={0.2}
            max={1}
            step={0.05}
            value={uiOpacity}
            onChange={(e) => setUiOpacity(parseFloat(e.target.value))}
            tabIndex={-1}
            className="flex-1 min-w-[52px] h-3"
          />

          <button
            onPointerDown={startDrag}
            tabIndex={-1}
            className="text-slate-400 hover:text-slate-200 px-1 py-1 rounded border border-slate-700 bg-slate-900"
          >
            <IconGrip />
          </button>

          <button
            onPointerDown={startResize}
            tabIndex={-1}
            className="text-slate-400 hover:text-slate-200 px-1 py-1 rounded border border-slate-700 bg-slate-900"
          >
            <IconResize />
          </button>

          <button
            onClick={toggleHidden}
            tabIndex={-1}
            className="text-slate-400 hover:text-slate-200 px-1 py-1 rounded border border-slate-700 bg-slate-900"
          >
            <IconMinus />
          </button>

        </div>

        {/* BINDING LEGEND — driven off the live config, so a rebind updates it */}
        <div className="mt-1 flex items-center justify-center gap-2 text-[8.5px] text-slate-600 pointer-events-none leading-none">
          <span>{labelKey(keys.summon_double_tap)} SUMMON</span>
          <span>{labelKey(keys.sniper_double_tap)} SHOT</span>
          <span>{labelKey(keys.toggle_double_tap)} HIDE</span>
          <span>{labelKey(keys.flush_double_tap)} SEND</span>
          <span>{labelKey(keys.audio_double_tap)} AUDIO</span>
          <span>{labelKey(keys.opacity_up_double_tap)}{labelKey(keys.opacity_down_double_tap)} OPACITY</span>
        </div>

      </div>
    </main>
  );
}