import { useState, useEffect, useRef } from "react";
import { createClient, SupabaseClient } from '@supabase/supabase-js';
import { invoke } from '@tauri-apps/api/core';

// ── multi-node Supabase ────────────────────────────────────────
const urlsEnv =
  import.meta.env.VITE_SUPABASE_URLS ||
  import.meta.env.VITE_SUPABASE_URL ||
  "https://hbeafjnartmylnkwjtfn.supabase.co,https://wbweusamfnzavtzxkjve.supabase.co,https://baxnacoiewnholwjadii.supabase.co,https://vxjlefmnhwzokvnejsvj.supabase.co,https://dngbxkijkrurywvsxqva.supabase.co,https://bulokkhwwmupmjueajwr.supabase.co";

const keysEnv =
  import.meta.env.VITE_SUPABASE_KEYS ||
  import.meta.env.VITE_SUPABASE_ANON_KEY ||
  "";

const supabaseUrls: string[] = urlsEnv.split(',').map((u: string) => u.trim()).filter((u: string) => u.length > 0);
const supabaseKeys: string[] = keysEnv.split(',').map((k: string) => k.trim()).filter((k: string) => k.length > 0);

const supabaseClients: SupabaseClient[] = [];

supabaseUrls.forEach((url: string, index: number) => {
  const key = supabaseKeys[index];
  if (url && key) {
    try {
      supabaseClients.push(createClient(url, key));
    } catch (err) {
      console.error(`[WARN] db node init failed for ${url}:`, err);
    }
  }
});

const REMOTE_VIEWER = "https://pro-buddy-onmv.vercel.app/";
const MIN_W = 360;
const MIN_H = 260;

const REPLY_TIMEOUT_MS = 20_000;
const VM_NAME = (import.meta.env as any).VITE_VM_NAME || "Test-Nartan-Build";
const VM_POLL_MS = 1500;
const VM_POLL_MAX_TICKS = 60;
const PROBE_MIN_B64 = 2000;
const MAX_BUFFERED = 6;

const THREAD_POLL_MS = 2000;
const SWEEP_PAGE = 500;
const MIRROR_WIDTH = 0;

const WORDS_KEY = "protocol_word_bank";
const PROMPT_KEY = "protocol_prompt";

const DEFAULT_OPACITY = 0.96;
const MIN_OPACITY = 0.2;

const clampOpacity = (value: number) =>
  Number.isFinite(value) ? Math.min(1, Math.max(MIN_OPACITY, value)) : DEFAULT_OPACITY;

const DEFAULT_WORDS = [
  "ai", "change module", "chatgpt", "claude", "console", "error", "fast",
  "gemini", "higher", "module", "no", "ok", "other", "send", "test case",
  "time limit exceeded", "to", "wait", "yes",
];

const DEFAULT_PROMPT = `I going to train you for solving DSA and MCQ quetions,I'll give you screenshots...`;

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

interface ThreadRow {
  id: string;
  kind: string;
  sender: string;
  answer_text: string | null;
  created_at: string;
}

interface ChatLine {
  id: string;
  mine: boolean;
  text: string;
}

interface FrameRow {
  id: string;
  batch_id: string;
  image_url: string;
}

const FALLBACK_KEYS: TriggerConfig = {
  version: 3,
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

const WS_URLS = [
  (import.meta.env as any).VITE_WS_URL,
  "wss://provibe.onrender.com",
  isTauri() ? "ws://localhost:8765" : null,
].filter((url): url is string => typeof url === "string" && url.length > 0);

const stamp = () => new Date().toISOString().slice(11, 23);
const wsLog = (...args: unknown[]) => console.log(`[ws ${stamp()}]`, ...args);

const mintCode = () => Math.random().toString(36).substring(2, 8).toUpperCase();

// ── icons ──────────────────────────────────────────────────────
const IconScan = () => (
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round" strokeLinejoin="round" className="h-3 w-3">
    <path d="M4 8V6a2 2 0 0 1 2-2h2" /><path d="M20 8V6a2 2 0 0 0-2-2h-2" />
    <path d="M4 16v2a2 2 0 0 0 2 2h2" /><path d="M20 16v2a2 2 0 0 1-2 2h-2" />
    <circle cx="12" cy="12" r="2.5" />
  </svg>
);

const IconSend = () => (
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round" strokeLinejoin="round" className="h-3 w-3">
    <path d="M12 19V5" /><path d="M6 11l6-6 6 6" />
  </svg>
);

const IconDrop = () => (
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round" strokeLinejoin="round" className="h-3 w-3">
    <path d="M4 7h16" /><path d="M9 7V5h6v2" /><path d="M6 7l1 12h10l1-12" />
  </svg>
);

const IconCopy = () => (
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round" strokeLinejoin="round" className="h-3 w-3">
    <rect x="9" y="9" width="11" height="11" rx="1.5" /><path d="M15 5H5v10" />
  </svg>
);

const IconGrip = () => (
  <svg viewBox="0 0 24 24" fill="currentColor" className="h-3 w-3">
    <circle cx="9" cy="7" r="1.3" /><circle cx="15" cy="7" r="1.3" />
    <circle cx="9" cy="12" r="1.3" /><circle cx="15" cy="12" r="1.3" />
    <circle cx="9" cy="17" r="1.3" /><circle cx="15" cy="17" r="1.3" />
  </svg>
);

const IconResize = () => (
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round" strokeLinejoin="round" className="h-3 w-3">
    <path d="M20 14v6h-6" /><path d="M20 20 13 13" /><path d="M4 10V4h6" /><path d="M4 4l7 7" />
  </svg>
);

const IconMinus = () => (
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" className="h-3 w-3">
    <path d="M6 12h12" />
  </svg>
);

const IconTrash = () => (
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round" strokeLinejoin="round" className="h-3 w-3">
    <path d="M4 7h16" /><path d="M9 7V5h6v2" /><path d="M6 7l1 12h10l1-12" /><path d="M10 11v5" /><path d="M14 11v5" />
  </svg>
);

const IconDownload = () => (
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round" strokeLinejoin="round" className="h-3 w-3">
    <path d="M12 5v10" /><path d="M7 11l5 5 5-5" /><path d="M5 19h14" />
  </svg>
);

export default function App() {
  const [sessionCode, setSessionCode] = useState("");
  const [status, setStatus] = useState("STANDBY");
  const [transcript, setTranscript] = useState("awaiting diagnostic trigger");
  const [response, setResponse] = useState("// agent online — press the summon key.");
  const [chatLines, setChatLines] = useState<ChatLine[]>([]);
  const [keys, setKeys] = useState<TriggerConfig>(FALLBACK_KEYS);
  const [configPath, setConfigPath] = useState("");

  const [uiOpacity, setUiOpacity] = useState(DEFAULT_OPACITY);
  const [captureMode, setCaptureMode] = useState<"VM" | "HOST">("HOST");
  const [buffered, setBuffered] = useState(0);
  const [dbBusy, setDbBusy] = useState<null | "delete" | "download">(null);

  const [activeLine, setActiveLine] = useState<number | null>(null);
  const activeLineRef = useRef<HTMLDivElement>(null);
  const responseRef = useRef(response);
  const chatEndRef = useRef<HTMLDivElement>(null);

  const [composer, setComposer] = useState("");
  const [words, setWords] = useState<string[]>(DEFAULT_WORDS);
  const [addingWord, setAddingWord] = useState(false);
  const [newWord, setNewWord] = useState("");
  const [promptText, setPromptText] = useState(DEFAULT_PROMPT);
  const [showPrompt, setShowPrompt] = useState(false);
  const [editingPrompt, setEditingPrompt] = useState(false);

  const [showSettings, setShowSettings] = useState(false);
  const [apiKey, setApiKey] = useState("");
  const [resumeText, setResumeText] = useState("");
  const [taskMode, setTaskMode] = useState<"AUTO" | "MCQ" | "CODING" | "SQL" | "BEHAVIORAL">("AUTO");

  const [targetBrowser, setTargetBrowser] = useState<"SEB" | "MSB">("SEB");
  const [vmStatus, setVmStatus] = useState<"OFFLINE" | "BOOTING" | "HOOKED">("OFFLINE");

  const wsRef = useRef<WebSocket | null>(null);
  const taskModeRef = useRef(taskMode);
  const captureModeRef = useRef(captureMode);
  const targetBrowserRef = useRef(targetBrowser);
  const imageBufferRef = useRef<string[]>([]);
  const vmPollRef = useRef<number | undefined>(undefined);
  const replyWatchdogRef = useRef<number | undefined>(undefined);
  const seenRef = useRef<Set<string>>(new Set());
  const activeNodeRef = useRef(0);

  useEffect(() => { taskModeRef.current = taskMode; }, [taskMode]);
  useEffect(() => { captureModeRef.current = captureMode; }, [captureMode]);
  useEffect(() => { responseRef.current = response; }, [response]);
  useEffect(() => { targetBrowserRef.current = targetBrowser; }, [targetBrowser]);

  useEffect(() => { setSessionCode(mintCode()); }, []);

  useEffect(() => {
    const saved = localStorage.getItem('protocol_ui_opacity');
    if (saved) {
      const parsed = parseFloat(saved);
      if (Number.isFinite(parsed) && parsed >= MIN_OPACITY && parsed <= 1) {
        setUiOpacity(parsed);
      }
    }
    const savedWords = localStorage.getItem(WORDS_KEY);
    if (savedWords) {
      try {
        const parsed = JSON.parse(savedWords);
        if (Array.isArray(parsed) && parsed.length) setWords(parsed);
      } catch { /* keep defaults */ }
    }
    const savedPrompt = localStorage.getItem(PROMPT_KEY);
    if (savedPrompt) setPromptText(savedPrompt);
  }, []);

  useEffect(() => { localStorage.setItem('protocol_ui_opacity', uiOpacity.toString()); }, [uiOpacity]);

  const clearReplyWatchdog = () => {
    if (replyWatchdogRef.current) {
      clearTimeout(replyWatchdogRef.current);
      replyWatchdogRef.current = undefined;
    }
  };

  const armReplyWatchdog = (label: string) => {
    clearReplyWatchdog();
    replyWatchdogRef.current = window.setTimeout(() => {
      replyWatchdogRef.current = undefined;
      setStatus("NO REPLY");
      setTranscript(`${label} — no answer after ${Math.round(REPLY_TIMEOUT_MS / 1000)}s`);
    }, REPLY_TIMEOUT_MS);
  };

  // ── failover writes ────────────────────────────────────────
  const storeRow = async (payload: Record<string, unknown>): Promise<boolean> => {
    const total = supabaseClients.length;
    if (total === 0) return false;

    let accepted = -1;
    for (let step = 0; step < total; step++) {
      const idx = (activeNodeRef.current + step) % total;
      try {
        const { error } = await supabaseClients[idx].from("copilot_sessions").insert(payload);
        if (!error) { accepted = idx; break; }
      } catch (err) {
        console.warn("node insert error:", err);
      }
    }
    if (accepted === -1) return false;
    activeNodeRef.current = accepted;
    return true;
  };

  // ── thread ingestion ───────────────────────────────────────
  const ingest = (rows: ThreadRow[]) => {
    const fresh = rows.filter((r) => r.id && !seenRef.current.has(r.id));
    if (fresh.length === 0) return;

    const newChat: ChatLine[] = [];
    let latestAnswer: string | null = null;

    for (const row of fresh) {
      seenRef.current.add(row.id);
      const text = row.answer_text ?? "";
      if (row.kind === "chat" && text.trim()) {
        newChat.push({ id: row.id, mine: row.sender === "copilot", text });
      } else if (row.kind === "answer" && text.trim()) {
        latestAnswer = text;
      }
    }

    if (newChat.length) {
      setChatLines((prev) => [...prev, ...newChat]);
      setTranscript(`operator chat: ${newChat[newChat.length - 1].text}`);
    }
    if (latestAnswer !== null) {
      setResponse(latestAnswer);
      setStatus("OVERRIDE");
      setTranscript("operator delivered an answer");
      clearReplyWatchdog();
    }
  };

  // ── WebSocket ──────────────────────────────────────────────
  useEffect(() => {
    if (!sessionCode) return;
    let isMounted = true;
    let reconnectTimer: number | undefined;
    let urlIndex = 0;
    let retryDelay = 3000;

    const connectWs = () => {
      if (!isMounted) return;
      if (wsRef.current && (wsRef.current.readyState === WebSocket.CONNECTING || wsRef.current.readyState === WebSocket.OPEN)) return;

      const wsUrl = WS_URLS[urlIndex % WS_URLS.length];
      wsLog("connecting", wsUrl, "session", sessionCode);
      setStatus("RECONNECTING...");
      const socket = new WebSocket(wsUrl);
      wsRef.current = socket;

      socket.onopen = () => {
        if (!isMounted) return;
        wsLog("open");
        setStatus("CONNECTED");
        retryDelay = 3000;
        socket.send(JSON.stringify({ action: "set_session", session_code: sessionCode }));
        socket.send(JSON.stringify({ action: "set_mode", mode: taskModeRef.current }));
      };

      socket.onclose = (ev) => {
        wsLog("closed", { code: ev.code });
        if (!isMounted) return;
        clearReplyWatchdog();
        setStatus("DISCONNECTED");
        wsRef.current = null;
        urlIndex = (urlIndex + 1) % WS_URLS.length;
        reconnectTimer = window.setTimeout(connectWs, retryDelay);
        retryDelay = Math.min(retryDelay * 2, 15000);
      };

      socket.onerror = (err) => { wsLog("error", err); };

      socket.onmessage = (event: MessageEvent) => {
        if (!isMounted) return;
        clearReplyWatchdog();
        wsLog("<-", event.data);
        try {
          const payload = JSON.parse(event.data);

          // 1. Direct answer delivery (from ProtoBuddy)
          const answerText = payload.data || payload.answer || payload.text;
          if (
            (payload.status === "success" || payload.status === "answer" || payload.status === "override") &&
            answerText &&
            answerText !== "Text query received."
          ) {
            setResponse(answerText);
            setStatus("READY");
            setTranscript(payload.transcript || "answer received from operator");
            if (payload.active_line !== undefined) setActiveLine(payload.active_line);
            return;
          }

          // 2. Direct chat message delivery (from ProtoBuddy)
          if (payload.status === "chat" || payload.action === "chat") {
            const chatMsg = payload.text || payload.data;
            if (chatMsg) {
              setChatLines((prev) => [
                ...prev,
                { id: String(Date.now()) + Math.random(), mine: payload.sender === "copilot", text: chatMsg },
              ]);
              setTranscript(`operator chat: ${chatMsg}`);
            }
            return;
          }

          if (payload.status === "loading") {
            setStatus("THINKING...");
            setTranscript(payload.transcript || "processing...");
          } else if (payload.status === "awaiting_friend") {
            setStatus("AWAITING");
            setTranscript(payload.transcript || "frames with operator");
          } else if (payload.status === "sync_cursor" && payload.active_line !== undefined) {
            setActiveLine(payload.active_line);
          } else if (payload.status === "nav_up") {
            setActiveLine((p) => (p === null ? 1 : Math.max(1, p - 1)));
          } else if (payload.status === "nav_down") {
            setActiveLine((p) => {
              const max = responseRef.current.split('\n').length;
              return p === null ? 1 : Math.min(max, p + 1);
            });
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
      clearReplyWatchdog();
      if (wsRef.current) {
        wsRef.current.onclose = null;
        wsRef.current.close();
        wsRef.current = null;
      }
    };
  }, [sessionCode]);

  useEffect(() => {
    if (chatEndRef.current) chatEndRef.current.scrollIntoView({ behavior: "smooth", block: "end" });
  }, [chatLines]);

  // ── capture ────────────────────────────────────────────────
  const captureFrame = async () => {
    if (!isTauri()) {
      (window as any).__browserBuffered = true;
      setTranscript("browser mode — press ENTER to capture and send");
      return;
    }
    if (imageBufferRef.current.length >= MAX_BUFFERED) {
      setTranscript(`buffer full (${MAX_BUFFERED}) — send or drop first`);
      return;
    }

    try {
      let b64: string;
      if (captureModeRef.current === "VM") {
        try {
          b64 = await invoke("capture_rdp_window", { ip: VM_NAME });
        } catch {
          b64 = await invoke("capture_screen_native");
          setTranscript("captured monitor");
        }
      } else {
        b64 = await invoke("capture_screen_native");
      }

      if (!b64 || b64.length < PROBE_MIN_B64) {
        setTranscript("capture empty — retry");
        return;
      }

      imageBufferRef.current.push(`data:image/jpeg;base64,${b64}`);
      setBuffered(imageBufferRef.current.length);
      setTranscript(`held ${imageBufferRef.current.length} frame(s) — press send`);
    } catch (err) {
      console.error(err);
      setTranscript("capture failed");
    }
  };

  const dropBuffer = () => {
    const had = imageBufferRef.current.length;
    imageBufferRef.current = [];
    setBuffered(0);
    setTranscript(had ? `dropped ${had} held frame(s)` : "buffer empty");
    setStatus("READY");
  };

  // ── FIXED: SEND SCREENSHOTS VIA WEBSOCKET FIRST ────────────
  const flushBuffer = async () => {
    const mode = taskModeRef.current;
    const frames = [...imageBufferRef.current];

    if (frames.length === 0) {
      setTranscript("nothing held — capture a frame first (SS)");
      return;
    }
    if (!sessionCode) {
      setStatus("ERROR");
      setTranscript("no session code");
      return;
    }

    setStatus("UPLOADING...");
    setTranscript(`transmitting ${frames.length} frame(s)...`);
    const batchId = `frame_${Date.now()}`;

    // 1. Instantly transmit over live WebSocket
    if (wsRef.current?.readyState === WebSocket.OPEN) {
      try {
        wsRef.current.send(
          JSON.stringify({
            action: "browser_image_batch_upload",
            images: frames,
            mode,
            session_code: sessionCode,
          })
        );
        setStatus("READY");
        setTranscript(`sent ${frames.length} frame(s) to operator`);
      } catch (err) {
        wsLog("socket push threw", err);
      }
    }

    // 2. Background database backup (never halts or errors out the UI)
    void (async () => {
      for (const frame of frames) {
        await storeRow({
          session_code: sessionCode,
          batch_id: batchId,
          kind: "frame",
          sender: "copilot",
          image_url: frame,
        }).catch(() => undefined);
      }
    })();

    imageBufferRef.current = [];
    setBuffered(0);
    armReplyWatchdog(`${frames.length} frame(s)`);
  };

  // ── FIXED: SEND CHAT/WORDS VIA WEBSOCKET FIRST ─────────────
  const sendChat = async () => {
    const text = composer.trim();
    if (!text || !sessionCode) return;

    // 1. Immediately send via live WebSocket to ProtoBuddy
    if (wsRef.current?.readyState === WebSocket.OPEN) {
      wsRef.current.send(
        JSON.stringify({
          action: "chat",
          text,
          sender: "copilot",
          session_code: sessionCode,
        })
      );
    }

    // 2. Display in local chat window
    setChatLines((prev) => [...prev, { id: String(Date.now()), mine: true, text }]);

    // 3. Background DB backup (won't block UI or show error if DB is down)
    void storeRow({
      session_code: sessionCode,
      batch_id: `chat_${Date.now()}`,
      kind: "chat",
      sender: "copilot",
      answer_text: text,
    }).catch(() => undefined);

    setComposer("");
    setTranscript(`sent: ${text}`);
    setStatus("READY");
  };

  const addWordToComposer = (word: string) => {
    setComposer((prev) => (prev ? `${prev} ${word}` : word));
  };

  const backspaceComposer = () => {
    setComposer((prev) => {
      const trimmed = prev.replace(/\s+$/, "");
      const cut = trimmed.lastIndexOf(" ");
      return cut === -1 ? "" : trimmed.slice(0, cut);
    });
  };

  const commitNewWord = () => {
    const word = newWord.trim().toLowerCase();
    setNewWord("");
    setAddingWord(false);
    if (!word) return;
    setWords((prev) => {
      if (prev.includes(word)) return prev;
      const next = [...prev, word].sort((a, b) => a.localeCompare(b));
      localStorage.setItem(WORDS_KEY, JSON.stringify(next));
      return next;
    });
    addWordToComposer(word);
  };

  const completeSession = () => {
    const code = mintCode();
    setSessionCode(code);
    setChatLines([]);
    setComposer("");
    imageBufferRef.current = [];
    setBuffered(0);
    setStatus("NEW SESSION");
    setTranscript(`new session code: ${code}`);
  };

  const copyText = async (text: string, label: string) => {
    try {
      await navigator.clipboard.writeText(text);
      setTranscript(label);
    } catch {
      setTranscript(`clipboard blocked — ${text}`);
    }
  };

  const shareLink = () => copyText(`${REMOTE_VIEWER}?session=${sessionCode}`, `share link copied`);

  const startDrag = async () => {
    if (!isTauri()) return;
    const { getCurrentWindow } = await import('@tauri-apps/api/window');
    getCurrentWindow().startDragging();
  };

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

  const pillTone =
    status === "ERROR" || status === "DISCONNECTED" ? "pill pill--err" : "pill pill--ok";

  return (
    <main className="shell" style={{ opacity: clampOpacity(uiOpacity) }}>
      <div className="frame">
        <div className="bar" onPointerDown={startDrag}>
          <div className="brand">
            <span className="dot" />
            PROTOCOL
            <em>{isTauri() ? "NATIVE" : "WEB"}</em>
          </div>

          <div className="idbox">
            <span className="k">ID</span>
            <span className="v">{sessionCode || "..."}</span>
            <button className="btn" tabIndex={-1} onClick={() => copyText(sessionCode, `copied ${sessionCode}`)}>COPY</button>
            <button className="btn" tabIndex={-1} onClick={shareLink}>SHARE</button>
          </div>

          <div className="tools">
            <button className={captureMode === "HOST" ? "btn btn--on" : "btn"} tabIndex={-1} onClick={() => setCaptureMode("HOST")}>HOST</button>
            <button className="btn btn--danger" tabIndex={-1} onClick={completeSession}>END</button>
            <span className={pillTone}>{status}</span>
          </div>
        </div>

        <div className="transcript">{transcript}</div>

        <div className="split">
          <div className="pane">
            <div className="pane__head">
              <span>CHAT</span>
              <span>{chatLines.length}</span>
            </div>
            <div className="pane__body">
              {chatLines.length === 0 && <div className="empty">no operator chat</div>}
              {chatLines.map((line) => (
                <div key={line.id} className="msg">
                  <b>{line.mine ? "ME" : "OP"}</b>{line.text}
                </div>
              ))}
              <div ref={chatEndRef} />
            </div>
          </div>

          <div className="pane">
            <div className="pane__head">
              <span>ANSWER</span>
              <button className="btn" tabIndex={-1} onClick={() => copyText(response, "answer copied")}>COPY</button>
            </div>
            <div className="pane__body pane__body--answer">
              {response.split('\n').map((line, index) => (
                <div key={index} className={activeLine === index + 1 ? "line line--active" : "line"}>
                  {line || " "}
                </div>
              ))}
            </div>
          </div>
        </div>

        <div className="chips">
          {words.map((word) => (
            <button key={word} className="chip" tabIndex={-1} onClick={() => addWordToComposer(word)}>{word}</button>
          ))}
          {addingWord ? (
            <input
              className="chip" autoFocus value={newWord} placeholder="new word" tabIndex={-1}
              style={{ width: 84, outline: "none" }}
              onChange={(e) => setNewWord(e.target.value)}
              onBlur={commitNewWord}
              onKeyDown={(e) => { if (e.key === "Enter") commitNewWord(); }}
            />
          ) : (
            <button className="chip chip--add" tabIndex={-1} onClick={() => setAddingWord(true)}>+</button>
          )}
        </div>

        <div className="composer">
          <div className={composer ? "composer__text" : "composer__text composer__text--empty"}>
            {composer || "tap words to compose"}
          </div>
          <button className="btn" tabIndex={-1} onClick={backspaceComposer} disabled={!composer}>DEL</button>
          <button className="btn" tabIndex={-1} onClick={() => setComposer("")} disabled={!composer}>CLR</button>
          <button className="btn btn--accent" tabIndex={-1} onClick={sendChat} disabled={!composer}>
            <IconSend />SEND
          </button>
        </div>

        <div className="strip">
          <button className="btn ss" tabIndex={-1} onClick={captureFrame}>
            <IconScan />SS
            {buffered > 0 && <span className="badge">{buffered}</span>}
          </button>

          <button className="btn btn--danger" tabIndex={-1} onClick={dropBuffer} disabled={buffered === 0}>
            <IconDrop />DROP
          </button>

          <button className="btn btn--accent" tabIndex={-1} onClick={flushBuffer}>
            <IconSend />SEND
          </button>

          <input type="range" min={MIN_OPACITY} max={1} step={0.05} value={uiOpacity} tabIndex={-1}
            onChange={(e) => setUiOpacity(clampOpacity(parseFloat(e.target.value)))} />

          <button className="btn" tabIndex={-1} onPointerDown={startDrag}><IconGrip /></button>
          <button className="btn" tabIndex={-1} onPointerDown={startResize}><IconResize /></button>
        </div>
      </div>
    </main>
  );
}