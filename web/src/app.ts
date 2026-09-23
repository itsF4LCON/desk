import {
  createIcons, ArrowRight, ClipboardCopy, ClipboardPaste, Loader, Lock, LogOut, Maximize, Minimize,
  ShieldCheck, Volume2, VolumeX,
} from "lucide";

const ICONS = { ArrowRight, ClipboardCopy, ClipboardPaste, Loader, Lock, LogOut, Maximize, Minimize, ShieldCheck, Volume2, VolumeX };
const HEARTBEAT_MS = 250;
const ICE_GATHER_TIMEOUT_MS = 2500;

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;
const icons = () => createIcons({ icons: ICONS });

type Monitor = { name: string; width: number; height: number };

// ---------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------
function show(view: "login" | "viewer") {
  $("login-view").classList.toggle("hidden-view", view !== "login");
  $("viewer-view").classList.toggle("hidden-view", view !== "viewer");
}

async function api(path: string, body?: unknown): Promise<Response> {
  return fetch(path, {
    method: body === undefined ? "GET" : "POST",
    credentials: "same-origin",
    headers: body === undefined ? {} : { "content-type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
}

async function init() {
  icons();
  document.querySelectorAll("[data-host]").forEach((el) => (el.textContent = location.hostname));
  const me = await api("/api/me").then((r) => (r.ok ? r.json() : { authenticated: false })).catch(() => ({ authenticated: false }));
  if (me.authenticated) {
    startViewer(false);
  } else {
    show("login");
    $("password").focus();
  }
}

$("login-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const button = $<HTMLButtonElement>("login-button");
  const error = $("login-error");
  button.disabled = true;
  error.textContent = "";
  try {
    const res = await api("/api/login", {
      password: $<HTMLInputElement>("password").value,
      code: $<HTMLInputElement>("code").value.replace(/\s/g, ""),
    });
    if (res.ok) {
      $<HTMLInputElement>("password").value = "";
      $<HTMLInputElement>("code").value = "";
      startViewer(true); // user gesture: audio may start unmuted
    } else {
      const body = await res.json().catch(() => ({}));
      error.textContent = body.error ?? `Login failed (${res.status})`;
      $<HTMLInputElement>("code").value = "";
    }
  } catch {
    error.textContent = "Network error";
  } finally {
    button.disabled = false;
  }
});

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------
let pc: RTCPeerConnection | null = null;
let ws: WebSocket | null = null;
let channel: RTCDataChannel | null = null;
let heartbeat = 0;
let statsTimer = 0;

function setStatus(text: string, live = false) {
  $("status-text").textContent = text;
  $("status-dot").className = `w-1.5 h-1.5 rounded-full ${live ? "bg-white" : "bg-muted"}`;
}

function overlay(text: string | null) {
  $("overlay").classList.toggle("hidden-view", text === null);
  if (text) $("overlay-text").textContent = text;
}

function toast(text: string) {
  const t = $("toast");
  t.textContent = text;
  t.classList.remove("hidden-view");
  clearTimeout((t as any)._timer);
  (t as any)._timer = setTimeout(() => t.classList.add("hidden-view"), 2500);
}

function send(msg: object) {
  if (channel?.readyState === "open") channel.send(JSON.stringify(msg));
}

function waitForIce(conn: RTCPeerConnection): Promise<void> {
  if (conn.iceGatheringState === "complete") return Promise.resolve();
  return new Promise((resolve) => {
    const done = () => { conn.removeEventListener("icegatheringstatechange", check); resolve(); };
    const check = () => { if (conn.iceGatheringState === "complete") done(); };
    conn.addEventListener("icegatheringstatechange", check);
    setTimeout(done, ICE_GATHER_TIMEOUT_MS);
  });
}

function startViewer(fromGesture: boolean) {
  show("viewer");
  overlay("Establishing session");
  setStatus("Connecting");
  const video = $<HTMLVideoElement>("screen");
  video.muted = !fromGesture;
  updateAudioIcon();

  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  ws = new WebSocket(`${proto}//${location.host}/ws`);
  ws.onclose = () => {
    if (pc) teardown("Session closed");
  };
  ws.onmessage = async (ev) => {
    const msg = JSON.parse(ev.data);
    if (msg.type === "hello") {
      // `?relay=1` forces the Cloudflare TURN relay (troubleshooting strict networks).
      const relayOnly = new URLSearchParams(location.search).get("relay") === "1";
      pc = new RTCPeerConnection({
        iceServers: msg.iceServers,
        bundlePolicy: "max-bundle",
        iceTransportPolicy: relayOnly ? "relay" : "all",
      });
      pc.addTransceiver("video", { direction: "recvonly" });
      pc.addTransceiver("audio", { direction: "recvonly" });
      channel = pc.createDataChannel("input", { ordered: true });
      channel.onopen = () => {
        heartbeat = window.setInterval(() => send({ t: "hb" }), HEARTBEAT_MS);
      };
      channel.onmessage = (e) => onChannelMessage(JSON.parse(e.data));
      channel.onclose = () => clearInterval(heartbeat);

      pc.ontrack = (e) => {
        const stream = video.srcObject instanceof MediaStream ? video.srcObject : new MediaStream();
        stream.addTrack(e.track);
        video.srcObject = stream;
        // Minimal playout delay: this is interactive, not a movie.
        const r = e.receiver as RTCRtpReceiver & { jitterBufferTarget?: number; playoutDelayHint?: number };
        if ("jitterBufferTarget" in r) r.jitterBufferTarget = 0;
        if ("playoutDelayHint" in r) r.playoutDelayHint = 0;
        video.play().catch(() => {});
      };
      pc.onconnectionstatechange = () => {
        const s = pc?.connectionState;
        if (s === "connected") {
          setStatus("Live", true);
          overlay(null);
          video.focus();
        } else if (s === "disconnected") {
          setStatus("Reconnecting");
          overlay("Connection interrupted");
        } else if (s === "failed") {
          teardown("Connection failed — network may block UDP");
        }
      };
      await pc.setLocalDescription(await pc.createOffer());
      await waitForIce(pc);
      ws!.send(JSON.stringify({ type: "offer", sdp: pc.localDescription!.sdp }));
    } else if (msg.type === "answer") {
      await pc!.setRemoteDescription({ type: "answer", sdp: msg.sdp });
      statsTimer = window.setInterval(updateStats, 1000);
    } else if (msg.type === "error") {
      teardown(msg.message);
    }
  };
}

function teardown(reason: string) {
  clearInterval(heartbeat);
  clearInterval(statsTimer);
  channel?.close();
  pc?.close();
  ws?.close();
  pc = null;
  ws = null;
  channel = null;
  setStatus("Offline");
  overlay(reason);
}

function onChannelMessage(msg: any) {
  if (msg.t === "hello") {
    renderMonitors(msg.monitors, msg.current);
  } else if (msg.t === "clip") {
    navigator.clipboard.writeText(msg.text).then(
      () => toast("PC clipboard copied"),
      () => toast("Browser blocked clipboard write"),
    );
  } else if (msg.t === "stats") {
    lastServerStats = msg;
  } else if (msg.t === "err") {
    toast(msg.message);
  }
}

function renderMonitors(monitors: Monitor[], current: string) {
  const box = $("monitors");
  box.replaceChildren(
    ...monitors.map((m, i) => {
      const b = document.createElement("button");
      const active = m.name === current;
      b.textContent = `${i + 1} · ${m.name}`;
      b.title = `${m.width}×${m.height}`;
      b.className =
        "border border-border px-2 py-0.5 text-[10px] font-mono uppercase tracking-wider transition-colors -ml-px " +
        (active ? "bg-white text-black border-white" : "text-muted hover:text-heading hover:border-borderHover");
      b.onclick = () => {
        if (!active) {
          overlay(`Switching to ${m.name}`);
          send({ t: "mon", name: m.name });
          setTimeout(() => overlay(null), 1200);
        }
      };
      return b;
    }),
  );
}

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------
let lastServerStats: { fps: number; kbps: number; target: number } | null = null;
let lastBytes = 0;
let lastTs = 0;

async function updateStats() {
  if (!pc) return;
  const report = await pc.getStats();
  let rtt: number | null = null;
  let fps: number | null = null;
  let relay = false;
  let mbps: number | null = null;
  report.forEach((s: any) => {
    if (s.type === "candidate-pair" && s.nominated && s.state === "succeeded") {
      rtt = s.currentRoundTripTime ?? rtt;
      const local = report.get(s.localCandidateId);
      relay = local?.candidateType === "relay";
    }
    if (s.type === "inbound-rtp" && s.kind === "video") {
      fps = s.framesPerSecond ?? fps;
      if (lastTs) mbps = ((s.bytesReceived - lastBytes) * 8) / ((s.timestamp - lastTs) * 1000);
      lastBytes = s.bytesReceived;
      lastTs = s.timestamp;
    }
  });
  const parts = [
    rtt !== null ? `${Math.round((rtt as number) * 1000)} ms` : "– ms",
    `${fps ?? "–"} fps`,
    mbps !== null ? `${(mbps as number).toFixed(1)} Mbps` : "– Mbps",
    relay ? "relay" : "direct",
  ];
  $("stats").textContent = parts.join(" · ");
}

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------
const video = $<HTMLVideoElement>("screen");
let pendingMove: { x: number; y: number } | null = null;

/** Maps a client point to 0..1 over the letterboxed video content (object-fit: contain). */
function normalise(clientX: number, clientY: number) {
  const r = video.getBoundingClientRect();
  const vw = video.videoWidth || 16;
  const vh = video.videoHeight || 9;
  const scale = Math.min(r.width / vw, r.height / vh);
  const w = vw * scale;
  const h = vh * scale;
  const left = r.left + (r.width - w) / 2;
  const top = r.top + (r.height - h) / 2;
  return { x: (clientX - left) / w, y: (clientY - top) / h };
}

video.addEventListener("pointermove", (e) => {
  const first = pendingMove === null;
  pendingMove = normalise(e.clientX, e.clientY);
  if (first) {
    requestAnimationFrame(() => {
      if (pendingMove) send({ t: "m", ...pendingMove });
      pendingMove = null;
    });
  }
});
video.addEventListener("pointerdown", (e) => {
  video.focus();
  video.setPointerCapture(e.pointerId);
  send({ t: "m", ...normalise(e.clientX, e.clientY) });
  send({ t: "b", b: e.button, d: true });
  e.preventDefault();
});
video.addEventListener("pointerup", (e) => {
  send({ t: "b", b: e.button, d: false });
  e.preventDefault();
});
video.addEventListener("contextmenu", (e) => e.preventDefault());
video.addEventListener(
  "wheel",
  (e) => {
    const unit = e.deltaMode === 1 ? 1 : e.deltaMode === 2 ? 10 : 1 / 100;
    send({ t: "w", dx: e.deltaX * unit, dy: e.deltaY * unit });
    e.preventDefault();
  },
  { passive: false },
);

function keyHandler(down: boolean) {
  return (e: KeyboardEvent) => {
    if (document.activeElement !== video) return;
    send({ t: "k", c: e.code, d: down });
    e.preventDefault();
  };
}
window.addEventListener("keydown", keyHandler(true));
window.addEventListener("keyup", keyHandler(false));
window.addEventListener("blur", () => send({ t: "rel" }));
document.addEventListener("visibilitychange", () => document.hidden && send({ t: "rel" }));
video.addEventListener("blur", () => send({ t: "rel" }));

// ---------------------------------------------------------------------------
// Toolbar
// ---------------------------------------------------------------------------
function updateAudioIcon() {
  const btn = $("btn-audio");
  btn.innerHTML = `<i data-lucide="${video.muted ? "volume-x" : "volume-2"}" class="w-4 h-4"></i>`;
  icons();
}

$("btn-audio").onclick = () => {
  video.muted = !video.muted;
  video.play().catch(() => {});
  updateAudioIcon();
};

$("btn-fullscreen").onclick = async () => {
  if (document.fullscreenElement) {
    await document.exitFullscreen();
  } else {
    await $("viewer-view").requestFullscreen();
    // Chromium: route Alt+Tab, Super, Esc etc. to the remote PC while fullscreen.
    await (navigator as any).keyboard?.lock?.().catch(() => {});
    video.focus();
  }
};
document.addEventListener("fullscreenchange", () => {
  $("btn-fullscreen").innerHTML = `<i data-lucide="${document.fullscreenElement ? "minimize" : "maximize"}" class="w-4 h-4"></i>`;
  icons();
});

$("btn-clip-send").onclick = async () => {
  try {
    const text = await navigator.clipboard.readText();
    send({ t: "clip-set", text });
    toast("Clipboard sent to PC");
  } catch {
    toast("Browser blocked clipboard read");
  }
  video.focus();
};
$("btn-clip-get").onclick = () => {
  send({ t: "clip-get" });
  video.focus();
};

$("btn-logout").onclick = async () => {
  send({ t: "rel" });
  teardown("Logged out");
  await api("/api/logout", {}).catch(() => {});
  show("login");
};

init();
