//! One viewer's WebRTC peer (str0m, sans-IO) and the task that drives it: UDP I/O, video/audio
//! writes, the `input` data channel, monitor switching, bitrate adaptation and cleanup.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use serde_json::json;
use str0m::bwe::{Bitrate, BweKind};
use str0m::change::SdpOffer;
use str0m::channel::ChannelId;
use str0m::format::Codec;
use str0m::media::{Frequency, MediaKind, MediaTime, Mid, Pt};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc, RtcConfig};
use tokio::net::UdpSocket;
use tokio::sync::{Mutex, mpsc, oneshot};
use tracing::{debug, info, warn};

use crate::audio::{AudioPipeline, OpusPacket};
use crate::capture::{self, EncodedFrame, VideoPipeline};
use crate::config::{PortalState, StateDir};
use crate::desktop::{self, Monitor};
use crate::input::{Injector, InputMsg};

const STUN_SERVER: &str = "stun.cloudflare.com:3478";
const DISCONNECT_GRACE: Duration = Duration::from_secs(10);
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(1);
const KEYFRAME_DEBOUNCE: Duration = Duration::from_millis(250);
const BITRATE_UPDATE_EVERY: Duration = Duration::from_millis(500);
const MAX_UDP: usize = 2000;

/// Shared, persisted portal restore tokens.
pub struct PortalStore {
    pub state: Mutex<PortalState>,
    pub dir: StateDir,
}

pub struct PeerSettings {
    pub udp_port: u16,
    pub portal: Arc<PortalStore>,
    pub encoder: capture::Encoder,
    /// Public host name, used in the notifications shown on the PC.
    pub host: String,
}

/// Messages on the data channel from the browser that aren't raw input.
#[derive(Deserialize)]
#[serde(tag = "t")]
enum Control {
    #[serde(rename = "mon")]
    Monitor { name: String },
    #[serde(rename = "clip-set")]
    ClipSet { text: String },
    #[serde(rename = "clip-get")]
    ClipGet,
}

/// Accepts the browser's offer and spawns the peer task. Returns the SDP answer and a stop handle.
pub async fn start(
    offer_sdp: &str,
    settings: PeerSettings,
) -> Result<(String, oneshot::Sender<()>)> {
    let local_ip = crate::stun::primary_local_ip()?;
    let socket = UdpSocket::bind(SocketAddr::new(local_ip, settings.udp_port)).await?;
    let local = socket.local_addr()?;

    let mut rtc = RtcConfig::new()
        .clear_codecs()
        .enable_h264(true)
        .enable_opus(true)
        .enable_bwe(Some(Bitrate::kbps(capture::START_BITRATE_KBPS as u64)))
        .build(Instant::now());
    rtc.add_local_candidate(Candidate::host(local, "udp")?);
    match crate::stun::server_reflexive(&socket, STUN_SERVER).await {
        Ok(public) if public != local => {
            debug!(%public, "server-reflexive candidate");
            rtc.add_local_candidate(Candidate::server_reflexive(public, local, "udp")?);
        }
        Ok(_) => {}
        Err(e) => warn!("STUN failed, offering host candidate only: {e:#}"),
    }
    rtc.bwe()
        .set_desired_bitrate(Bitrate::kbps(capture::MAX_BITRATE_KBPS as u64));

    // Diagnostics: which kinds of candidates did the browser gather (host / srflx / relay)?
    let mut kinds = std::collections::BTreeMap::<&str, u32>::new();
    for line in offer_sdp.lines().filter(|l| l.starts_with("a=candidate:")) {
        let kind = line
            .split_whitespace()
            .skip_while(|w| *w != "typ")
            .nth(1)
            .unwrap_or("?");
        let proto = line.split_whitespace().nth(2).unwrap_or("?");
        *kinds
            .entry(if proto.eq_ignore_ascii_case("tcp") {
                "tcp"
            } else {
                kind
            })
            .or_default() += 1;
    }
    info!(local = %local, browser_candidates = ?kinds, "offer received");
    if !kinds.contains_key("relay") {
        warn!(
            "browser offered no relay candidates; strict NATs / UDP-blocking networks will fail without TURN"
        );
    }

    let offer = SdpOffer::from_sdp_string(offer_sdp).context("invalid SDP offer")?;
    let answer = rtc
        .sdp_api()
        .accept_offer(offer)
        .context("offer not acceptable")?;

    let monitors = desktop::monitors().await?;
    let (stop_tx, stop_rx) = oneshot::channel();
    tokio::spawn(async move {
        let host = settings.host.clone();
        let peer = Peer::new(
            rtc,
            socket,
            monitors,
            settings.portal,
            settings.encoder,
            settings.host,
        );
        match peer {
            Ok(peer) => {
                if let Err(e) = peer.run(stop_rx).await {
                    warn!("session ended with error: {e:#}");
                }
            }
            Err(e) => warn!("session setup failed: {e:#}"),
        }
        desktop::notify(
            "Remote session ended",
            &format!("{host} viewer disconnected"),
        );
    });
    Ok((answer.to_sdp_string(), stop_tx))
}

struct Peer {
    rtc: Rtc,
    socket: UdpSocket,
    local: SocketAddr,
    monitors: Vec<Monitor>,
    current: Monitor,
    portal: Arc<PortalStore>,
    encoder: capture::Encoder,
    host: String,
    injector: Injector,
    video: Option<VideoPipeline>,
    _audio: Option<AudioPipeline>,
    video_tx: mpsc::Sender<EncodedFrame>,
    video_rx: mpsc::Receiver<EncodedFrame>,
    audio_rx: mpsc::Receiver<OpusPacket>,
    out_tx: mpsc::Sender<String>,
    out_rx: mpsc::Receiver<String>,
    video_track: Option<(Mid, Pt)>,
    audio_track: Option<(Mid, Pt)>,
    channel: Option<ChannelId>,
    connected: bool,
    disconnected_since: Option<Instant>,
    last_heartbeat: Instant,
    last_keyframe_req: Instant,
    last_bitrate_update: Instant,
    bitrate_kbps: u32,
    t0: Instant,
    stats: (u32, usize, Instant),
}

impl Peer {
    fn new(
        rtc: Rtc,
        socket: UdpSocket,
        monitors: Vec<Monitor>,
        portal: Arc<PortalStore>,
        encoder: capture::Encoder,
        host: String,
    ) -> Result<Self> {
        let current = monitors
            .first()
            .cloned()
            .ok_or_else(|| anyhow!("no monitors found"))?;
        let injector = Injector::new(desktop::layout(&monitors), current.rect())?;
        let (video_tx, video_rx) = mpsc::channel(4);
        let (audio_tx, audio_rx) = mpsc::channel(16);
        let (out_tx, out_rx) = mpsc::channel(16);
        let audio = AudioPipeline::start(audio_tx)
            .map_err(|e| warn!("audio disabled: {e:#}"))
            .ok();
        let local = socket.local_addr()?;
        let now = Instant::now();
        Ok(Self {
            rtc,
            socket,
            local,
            monitors,
            current,
            portal,
            encoder,
            host,
            injector,
            video: None,
            _audio: audio,
            video_tx,
            video_rx,
            audio_rx,
            out_tx,
            out_rx,
            video_track: None,
            audio_track: None,
            channel: None,
            connected: false,
            disconnected_since: None,
            last_heartbeat: now,
            last_keyframe_req: now - KEYFRAME_DEBOUNCE,
            last_bitrate_update: now,
            bitrate_kbps: capture::START_BITRATE_KBPS,
            t0: now,
            stats: (0, 0, now),
        })
    }

    async fn run(mut self, mut stop_rx: oneshot::Receiver<()>) -> Result<()> {
        let first = self.current.name.clone();
        self.switch_monitor(&first).await?;
        let mut buf = vec![0u8; MAX_UDP];
        let mut tick = tokio::time::interval(Duration::from_millis(250));

        loop {
            let timeout = self.drain_output().await?;
            if !self.rtc.is_alive() {
                return Ok(());
            }
            tokio::select! {
                _ = &mut stop_rx => {
                    info!("session replaced or closed by signaling");
                    return Ok(());
                }
                res = self.socket.recv_from(&mut buf) => {
                    let (n, source) = res?;
                    if let Ok(contents) = buf[..n].try_into() {
                        self.rtc.handle_input(Input::Receive(
                            Instant::now(),
                            Receive { proto: Protocol::Udp, source, destination: self.local, contents },
                        ))?;
                    }
                }
                Some(frame) = self.video_rx.recv() => self.write_video(frame)?,
                Some(packet) = self.audio_rx.recv() => self.write_audio(packet)?,
                Some(msg) = self.out_rx.recv() => self.send_channel(&msg),
                _ = tokio::time::sleep_until(timeout.into()) => {
                    self.rtc.handle_input(Input::Timeout(Instant::now()))?;
                }
                _ = tick.tick() => self.housekeeping().await?,
            }
        }
    }

    /// Flushes everything str0m wants to send and handles its events; returns the next timeout.
    async fn drain_output(&mut self) -> Result<Instant> {
        loop {
            match self.rtc.poll_output()? {
                Output::Timeout(t) => return Ok(t),
                Output::Transmit(t) => {
                    let _ = self.socket.send_to(&t.contents, t.destination).await;
                }
                Output::Event(e) => self.handle_event(e).await?,
            }
        }
    }

    async fn handle_event(&mut self, event: Event) -> Result<()> {
        match event {
            Event::IceConnectionStateChange(state) => {
                info!(?state, "ICE");
                match state {
                    IceConnectionState::Disconnected => {
                        self.disconnected_since.get_or_insert(Instant::now());
                        self.injector.release_all();
                    }
                    _ => self.disconnected_since = None,
                }
            }
            Event::Connected => {
                info!("peer connected");
                self.connected = true;
                desktop::notify(
                    "Remote session started",
                    &format!(
                        "Someone is viewing and controlling this PC via {}",
                        self.host
                    ),
                );
                self.request_keyframe();
            }
            Event::MediaAdded(m) => {
                let Some(writer) = self.rtc.writer(m.mid) else {
                    return Ok(());
                };
                match m.kind {
                    MediaKind::Video => {
                        let params: Vec<_> = writer.payload_params().cloned().collect();
                        let h264 = |cb: bool| {
                            params.iter().find(|p| {
                                let s = p.spec();
                                s.codec == Codec::H264
                                    && s.format.packetization_mode == Some(1)
                                    && (!cb
                                        || s.format
                                            .profile_level_id
                                            .is_some_and(|id| id >> 16 == 0x42))
                            })
                        };
                        if let Some(p) = h264(true).or_else(|| h264(false)) {
                            debug!(pt = ?p.pt(), fmtp = ?p.spec().format, "video payload");
                            self.video_track = Some((m.mid, p.pt()));
                        } else {
                            warn!("browser offered no usable H.264");
                        }
                    }
                    MediaKind::Audio => {
                        if let Some(p) = writer
                            .payload_params()
                            .find(|p| p.spec().codec == Codec::Opus)
                        {
                            self.audio_track = Some((m.mid, p.pt()));
                        }
                    }
                }
            }
            Event::KeyframeRequest(_) => self.request_keyframe(),
            Event::EgressBitrateEstimate(BweKind::Twcc(b))
            | Event::EgressBitrateEstimate(BweKind::Remb(_, b)) => {
                // Leave headroom for audio, RTP overhead and retransmissions.
                let target = ((b.as_u64() as f64 * 0.8) / 1000.0) as u32;
                let target = target.clamp(capture::MIN_BITRATE_KBPS, capture::MAX_BITRATE_KBPS);
                let changed = (target as i64 - self.bitrate_kbps as i64).unsigned_abs()
                    > self.bitrate_kbps as u64 / 10;
                if changed && self.last_bitrate_update.elapsed() >= BITRATE_UPDATE_EVERY {
                    self.bitrate_kbps = target;
                    self.last_bitrate_update = Instant::now();
                    if let Some(v) = &self.video {
                        v.set_bitrate_kbps(target);
                    }
                }
            }
            Event::ChannelOpen(id, label) if label == "input" => {
                self.channel = Some(id);
                self.last_heartbeat = Instant::now();
                self.send_hello();
            }
            Event::ChannelData(d) if Some(d.id) == self.channel => {
                self.handle_channel(&d.data).await;
            }
            Event::ChannelClose(id) if Some(id) == self.channel => {
                self.injector.release_all();
                self.channel = None;
            }
            _ => {}
        }
        Ok(())
    }

    async fn handle_channel(&mut self, data: &[u8]) {
        if data.len() > 2 * 1024 * 1024 {
            return;
        }
        if let Ok(msg) = serde_json::from_slice::<InputMsg>(data) {
            self.last_heartbeat = Instant::now();
            if let Err(e) = self.injector.handle(msg) {
                warn!("input injection failed: {e:#}");
            }
            return;
        }
        match serde_json::from_slice::<Control>(data) {
            Ok(Control::Monitor { name }) => {
                if let Err(e) = self.switch_monitor(&name).await {
                    warn!("monitor switch failed: {e:#}");
                    self.send_channel(
                        &json!({ "t": "err", "message": format!("{e:#}") }).to_string(),
                    );
                }
            }
            Ok(Control::ClipSet { text }) => {
                tokio::spawn(async move {
                    if let Err(e) = desktop::clipboard_set(&text).await {
                        warn!("clipboard set failed: {e:#}");
                    }
                });
            }
            Ok(Control::ClipGet) => {
                let out = self.out_tx.clone();
                tokio::spawn(async move {
                    let text = desktop::clipboard_get().await.unwrap_or_default();
                    let _ = out
                        .send(json!({ "t": "clip", "text": text }).to_string())
                        .await;
                });
            }
            Err(_) => debug!("ignoring unknown data channel message"),
        }
    }

    async fn switch_monitor(&mut self, name: &str) -> Result<()> {
        let monitor = self
            .monitors
            .iter()
            .find(|m| m.name == name)
            .cloned()
            .ok_or_else(|| anyhow!("no monitor {name}"))?;
        // Stop the old encoder first: consumer NVENC allows few concurrent sessions.
        self.video = None;
        let token = self.portal.state.lock().await.tokens.get(name).cloned();
        let stream = {
            let _pick = desktop::request_pick(name)?;
            capture::open_portal_stream(token.as_deref()).await?
        };
        if let Some(t) = &stream.restore_token {
            let mut st = self.portal.state.lock().await;
            st.tokens.insert(name.to_string(), t.clone());
            if let Err(e) = self.portal.dir.write("portal.json", &*st) {
                warn!("could not persist portal token: {e:#}");
            }
        }
        let video = VideoPipeline::start(stream, self.encoder, self.video_tx.clone())?;
        video.set_bitrate_kbps(self.bitrate_kbps);
        self.video = Some(video);
        self.injector.set_monitor(monitor.rect());
        self.current = monitor;
        info!(monitor = %self.current.name, "capturing");
        self.send_hello();
        Ok(())
    }

    fn request_keyframe(&mut self) {
        if self.last_keyframe_req.elapsed() >= KEYFRAME_DEBOUNCE {
            self.last_keyframe_req = Instant::now();
            if let Some(v) = &self.video {
                v.force_keyframe();
            }
        }
    }

    fn write_video(&mut self, frame: EncodedFrame) -> Result<()> {
        let (Some((mid, pt)), true) = (self.video_track, self.connected) else {
            return Ok(());
        };
        let now = Instant::now();
        let ticks = (now - self.t0).as_micros() as u64 * 90 / 1000;
        self.stats.0 += 1;
        self.stats.1 += frame.data.len();
        if let Some(w) = self.rtc.writer(mid) {
            w.write(
                pt,
                now,
                MediaTime::new(ticks, Frequency::NINETY_KHZ),
                frame.data,
            )?;
        }
        Ok(())
    }

    fn write_audio(&mut self, packet: OpusPacket) -> Result<()> {
        let (Some((mid, pt)), true) = (self.audio_track, self.connected) else {
            return Ok(());
        };
        let now = Instant::now();
        let ticks = (now - self.t0).as_micros() as u64 * 48 / 1000;
        if let Some(w) = self.rtc.writer(mid) {
            w.write(
                pt,
                now,
                MediaTime::new(ticks, Frequency::FORTY_EIGHT_KHZ),
                packet.data,
            )?;
        }
        Ok(())
    }

    fn send_channel(&mut self, msg: &str) {
        if let Some(mut ch) = self.channel.and_then(|id| self.rtc.channel(id)) {
            let _ = ch.write(false, msg.as_bytes());
        }
    }

    fn send_hello(&mut self) {
        let msg = json!({ "t": "hello", "monitors": self.monitors, "current": self.current.name });
        self.send_channel(&msg.to_string());
    }

    async fn housekeeping(&mut self) -> Result<()> {
        if self.last_heartbeat.elapsed() > HEARTBEAT_TIMEOUT {
            self.injector.release_all();
        }
        if self
            .disconnected_since
            .is_some_and(|t| t.elapsed() > DISCONNECT_GRACE)
        {
            info!("peer unreachable; ending session");
            self.rtc.disconnect();
            return Ok(());
        }
        if let Some(Err(e)) = self.video.as_ref().map(|v| v.poll_error()) {
            warn!("{e:#}; restarting capture");
            let name = self.current.name.clone();
            self.switch_monitor(&name).await?;
        }
        let (frames, bytes, since) = self.stats;
        let secs = since.elapsed().as_secs_f64();
        if secs >= 1.0 {
            let msg = json!({
                "t": "stats",
                "fps": (frames as f64 / secs).round(),
                "kbps": ((bytes * 8) as f64 / secs / 1000.0).round(),
                "target": self.bitrate_kbps,
            });
            self.send_channel(&msg.to_string());
            self.stats = (0, 0, Instant::now());
        }
        Ok(())
    }
}
