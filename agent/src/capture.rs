//! Screen capture: xdg-desktop-portal ScreenCast (one session per monitor, persisted with a
//! restore token) → PipeWire → GStreamer H.264 encoder → encoded access units.
//!
//! The encoder is chosen at startup by probing, in order: NVIDIA NVENC, VA-API (AMD/Intel),
//! then x264 in software. All produce constrained-baseline H.264, which every browser decodes.

use std::os::fd::{AsRawFd, OwnedFd};

use anyhow::{Context, Result, anyhow, bail};
use ashpd::desktop::PersistMode;
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use serde::Deserialize;
use tokio::sync::mpsc;
use tracing::{info, warn};

/// Bitrate bounds (kbit/s) the adaptive controller may choose between.
pub const MIN_BITRATE_KBPS: u32 = 2_000;
pub const MAX_BITRATE_KBPS: u32 = 20_000;
pub const START_BITRATE_KBPS: u32 = 8_000;
/// PipeWire only produces frames on damage; re-send the last frame this often so an idle
/// desktop still answers keyframe requests.
const KEEPALIVE_MS: u32 = 100;

/// `encoder` setting in config.toml.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum EncoderChoice {
    #[default]
    Auto,
    Nvenc,
    Vaapi,
    X264,
}

/// A concrete, working encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoder {
    Nvenc,
    Vaapi,
    X264,
}

const OUTPUT_CAPS: &str =
    "video/x-h264,profile=constrained-baseline,stream-format=byte-stream,alignment=au";

impl Encoder {
    pub const ALL: [Encoder; 3] = [Encoder::Nvenc, Encoder::Vaapi, Encoder::X264];

    pub fn name(self) -> &'static str {
        match self {
            Encoder::Nvenc => "NVIDIA NVENC (nvh264enc)",
            Encoder::Vaapi => "VA-API (vah264enc)",
            Encoder::X264 => "software x264 (x264enc)",
        }
    }

    fn element(self) -> &'static str {
        match self {
            Encoder::Nvenc => "nvh264enc",
            Encoder::Vaapi => "vah264enc",
            Encoder::X264 => "x264enc",
        }
    }

    /// Pipeline fragment from raw BGRx video to the encoder (named `enc`). Low-latency settings:
    /// no B-frames, CBR, no lookahead; keyframes are forced on demand by the viewer.
    fn fragment(self, kbps: u32) -> String {
        match self {
            // NVENC takes BGRx directly: no CPU colour conversion at all.
            Encoder::Nvenc => format!(
                "nvh264enc name=enc preset=p1 tune=ultra-low-latency rc-mode=cbr zerolatency=true \
                 bframes=0 gop-size=-1 bitrate={kbps} aud=false"
            ),
            Encoder::Vaapi => format!(
                "videoconvert ! video/x-raw,format=NV12 \
                 ! vah264enc name=enc rate-control=cbr bitrate={kbps} b-frames=0 target-usage=7"
            ),
            Encoder::X264 => format!(
                "videoconvert ! video/x-raw,format=I420 \
                 ! x264enc name=enc tune=zerolatency speed-preset=ultrafast bitrate={kbps} \
                   bframes=0 key-int-max=600 sliced-threads=true"
            ),
        }
    }

    /// Encodes a few test frames end-to-end; Ok if the encoder exists, negotiates and runs.
    pub fn probe(self) -> Result<()> {
        gst::init()?;
        if gst::ElementFactory::find(self.element()).is_none() {
            bail!("GStreamer element {} is not installed", self.element());
        }
        let desc = format!(
            "videotestsrc num-buffers=5 ! video/x-raw,format=BGRx,width=1280,height=720,framerate=30/1 \
             ! {} ! {OUTPUT_CAPS} ! h264parse ! fakesink",
            self.fragment(START_BITRATE_KBPS)
        );
        let pipeline = gst::parse::launch(&desc)?;
        pipeline.set_state(gst::State::Playing)?;
        let bus = pipeline.bus().ok_or_else(|| anyhow!("no bus"))?;
        let result = match bus.timed_pop_filtered(
            gst::ClockTime::from_seconds(10),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        ) {
            Some(msg) => match msg.view() {
                gst::MessageView::Eos(_) => Ok(()),
                gst::MessageView::Error(e) => Err(anyhow!("{}", e.error())),
                _ => unreachable!(),
            },
            None => Err(anyhow!("timed out")),
        };
        let _ = pipeline.set_state(gst::State::Null);
        result
    }

    /// Resolves the configured choice to a working encoder (probing when `auto`).
    pub fn select(choice: EncoderChoice) -> Result<Encoder> {
        let candidates: &[Encoder] = match choice {
            EncoderChoice::Auto => &Encoder::ALL,
            EncoderChoice::Nvenc => &[Encoder::Nvenc],
            EncoderChoice::Vaapi => &[Encoder::Vaapi],
            EncoderChoice::X264 => &[Encoder::X264],
        };
        let mut errors = Vec::new();
        for &enc in candidates {
            match enc.probe() {
                Ok(()) => return Ok(enc),
                Err(e) => errors.push(format!("{}: {e:#}", enc.name())),
            }
        }
        bail!("no usable H.264 encoder found:\n  {}", errors.join("\n  "))
    }
}

#[derive(Debug)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
}

/// A live portal ScreenCast stream. Dropping it ends the portal session.
pub struct PortalStream {
    pub node_id: u32,
    pub restore_token: Option<String>,
    fd: OwnedFd,
    _session: ashpd::desktop::Session<Screencast>,
    _proxy: Screencast,
}

/// Opens (or silently restores, when `restore_token` is valid) a monitor ScreenCast.
/// Without a valid token Hyprland shows its picker on the PC.
pub async fn open_portal_stream(restore_token: Option<&str>) -> Result<PortalStream> {
    let proxy = Screencast::new()
        .await
        .context("ScreenCast portal unavailable")?;
    let session = proxy.create_session(Default::default()).await?;
    proxy
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_cursor_mode(CursorMode::Embedded)
                .set_sources(ashpd::enumflags2::BitFlags::from(SourceType::Monitor))
                .set_multiple(false)
                .set_persist_mode(PersistMode::ExplicitlyRevoked)
                .set_restore_token(restore_token),
        )
        .await?;
    let streams = proxy
        .start(&session, None, Default::default())
        .await?
        .response()
        .context("screen share was cancelled or denied")?;
    let stream = streams
        .streams()
        .first()
        .ok_or_else(|| anyhow!("portal returned no streams"))?;
    let (node_id, position, size) = (stream.pipe_wire_node_id(), stream.position(), stream.size());
    let restore_token = streams.restore_token().map(str::to_string);
    let fd = proxy
        .open_pipe_wire_remote(&session, Default::default())
        .await?;
    info!(node_id, ?position, ?size, "portal stream opened");
    Ok(PortalStream {
        node_id,
        restore_token,
        fd,
        _session: session,
        _proxy: proxy,
    })
}

/// A running capture → encode pipeline for one portal stream.
pub struct VideoPipeline {
    pipeline: gst::Pipeline,
    encoder: gst::Element,
    // Keep the portal session (and its PipeWire fd) alive exactly as long as the pipeline.
    _stream: PortalStream,
}

impl VideoPipeline {
    pub fn start(
        stream: PortalStream,
        encoder: Encoder,
        frames: mpsc::Sender<EncodedFrame>,
    ) -> Result<Self> {
        gst::init()?;
        let desc = format!(
            "pipewiresrc fd={fd} path={node} do-timestamp=true keepalive-time={KEEPALIVE_MS} \
               ! video/x-raw,format=BGRx,max-framerate=60/1 \
               ! queue max-size-buffers=1 leaky=downstream \
             ! {enc} ! {OUTPUT_CAPS} \
             ! h264parse config-interval=-1 \
             ! appsink name=sink sync=false max-buffers=1 drop=true emit-signals=false",
            fd = stream.fd.as_raw_fd(),
            node = stream.node_id,
            enc = encoder.fragment(START_BITRATE_KBPS),
        );
        let pipeline = gst::parse::launch(&desc)?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow!("not a pipeline"))?;
        let encoder = pipeline
            .by_name("enc")
            .ok_or_else(|| anyhow!("encoder missing"))?;
        let sink = pipeline
            .by_name("sink")
            .ok_or_else(|| anyhow!("appsink missing"))?
            .downcast::<gst_app::AppSink>()
            .map_err(|_| anyhow!("not an appsink"))?;

        sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let Some(buffer) = sample.buffer() else {
                        return Ok(gst::FlowSuccess::Ok);
                    };
                    let Ok(map) = buffer.map_readable() else {
                        return Ok(gst::FlowSuccess::Ok);
                    };
                    let frame = EncodedFrame {
                        data: map.as_slice().to_vec(),
                    };
                    // If the network side is behind, drop rather than queue latency.
                    if frames.try_send(frame).is_err() && frames.is_closed() {
                        return Err(gst::FlowError::Eos);
                    }
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );

        pipeline
            .set_state(gst::State::Playing)
            .context("failed to start capture pipeline")?;
        Ok(Self {
            pipeline,
            encoder,
            _stream: stream,
        })
    }

    pub fn set_bitrate_kbps(&self, kbps: u32) {
        let kbps = kbps.clamp(MIN_BITRATE_KBPS, MAX_BITRATE_KBPS);
        self.encoder.set_property("bitrate", kbps);
    }

    pub fn force_keyframe(&self) {
        let event = gstreamer_video::UpstreamForceKeyUnitEvent::builder()
            .all_headers(true)
            .build();
        if let Some(pad) = self.encoder.static_pad("src")
            && !pad.send_event(event)
        {
            warn!("force-key-unit event was not handled");
        }
    }

    /// Returns an error if the pipeline posted one (used by the session to restart capture).
    pub fn poll_error(&self) -> Result<()> {
        let Some(bus) = self.pipeline.bus() else {
            return Ok(());
        };
        while let Some(msg) = bus.pop() {
            match msg.view() {
                gst::MessageView::Error(e) => {
                    bail!("capture pipeline error: {} ({:?})", e.error(), e.debug())
                }
                gst::MessageView::Eos(_) => bail!("capture pipeline reached end of stream"),
                _ => {}
            }
        }
        Ok(())
    }
}

impl Drop for VideoPipeline {
    fn drop(&mut self) {
        // Free the NVENC session promptly; consumer GPUs have a small concurrent-session limit.
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}
