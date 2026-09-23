//! Desktop audio: the default sink's monitor (what you hear) → Opus 48 kHz, 10 ms frames.

use anyhow::{Context, Result, anyhow};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use tokio::sync::mpsc;

pub struct OpusPacket {
    pub data: Vec<u8>,
}

pub struct AudioPipeline {
    pipeline: gst::Pipeline,
}

impl AudioPipeline {
    pub fn start(packets: mpsc::Sender<OpusPacket>) -> Result<Self> {
        gst::init()?;
        let pipeline = gst::parse::launch(
            "pulsesrc device=@DEFAULT_MONITOR@ do-timestamp=true buffer-time=20000 latency-time=10000 \
             ! audio/x-raw,rate=48000,channels=2 ! audioconvert ! audioresample \
             ! opusenc frame-size=10 bitrate=128000 audio-type=generic \
             ! appsink name=sink sync=false max-buffers=4 drop=true",
        )?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow!("not a pipeline"))?;
        let sink = pipeline
            .by_name("sink")
            .and_then(|e| e.downcast::<gst_app::AppSink>().ok())
            .ok_or_else(|| anyhow!("appsink missing"))?;
        sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    if let Some(buffer) = sample.buffer()
                        && let Ok(map) = buffer.map_readable()
                    {
                        let _ = packets.try_send(OpusPacket {
                            data: map.as_slice().to_vec(),
                        });
                    }
                    if packets.is_closed() {
                        Err(gst::FlowError::Eos)
                    } else {
                        Ok(gst::FlowSuccess::Ok)
                    }
                })
                .build(),
        );
        pipeline
            .set_state(gst::State::Playing)
            .context("failed to start audio pipeline")?;
        Ok(Self { pipeline })
    }
}

impl Drop for AudioPipeline {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}
