//! Sound test: record up to `MAX_SECS` from the virtual mic — the same
//! endpoint TeamSpeak/Discord record from, so it's exactly what they get —
//! then play it back on the speakers / headphones.
//!
//! The clip is also saved as `sound-test.wav` next to the config.

use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{anyhow, Result};
use audio_io::resample::ChannelMode;
use audio_io::wasapi_capture::{Frame, FrameSink};
use audio_io::wasapi_render::FrameSource;
use audio_io::{DeviceList, WasapiCapture, WasapiRender, FRAME_SAMPLES, SAMPLE_RATE};
use tracing::{info, warn};

pub const MAX_SECS: u32 = 30;
const MAX_SAMPLES: usize = MAX_SECS as usize * SAMPLE_RATE as usize;

enum Mode {
    Idle,
    Recording(WasapiCapture),
    Playing { _render: WasapiRender, started: Instant },
}

/// What the panel shows.
#[derive(Debug, Clone, Copy, Default)]
pub struct TestView {
    pub recording: bool,
    pub playing: bool,
    /// Seconds recorded so far / playback position.
    pub pos_secs: f32,
    /// Length of the finished clip, if there is one.
    pub clip_secs: Option<f32>,
    /// Peak of the recording so far (dBFS).
    pub peak_db: f32,
}

pub struct SoundTest {
    clip: Arc<Mutex<Vec<f32>>>,
    recorded: Arc<AtomicUsize>,
    played: Arc<AtomicUsize>,
    peak: Arc<AtomicU32>,
    clip_len: usize,
    mode: Mode,
}

impl SoundTest {
    pub fn new() -> Self {
        Self {
            clip: Arc::new(Mutex::new(Vec::new())),
            recorded: Arc::new(AtomicUsize::new(0)),
            played: Arc::new(AtomicUsize::new(0)),
            peak: Arc::new(AtomicU32::new(0)),
            clip_len: 0,
            mode: Mode::Idle,
        }
    }

    pub fn start_recording(&mut self, virtual_mic_id: &str) -> Result<()> {
        self.stop();
        {
            let mut clip = self.clip.lock().unwrap();
            clip.clear();
            clip.reserve(MAX_SAMPLES);
        }
        self.clip_len = 0;
        self.recorded.store(0, Ordering::Relaxed);
        self.peak.store(0, Ordering::Relaxed);
        let sink = Recorder {
            clip: self.clip.clone(),
            recorded: self.recorded.clone(),
            peak: self.peak.clone(),
        };
        // The cable carries the same mono signal on every channel.
        let capture = WasapiCapture::start_with(virtual_mic_id, ChannelMode::All, Box::new(sink))
            .map_err(|e| anyhow!("can't record the virtual mic: {e}"))?;
        info!("sound test: recording");
        self.mode = Mode::Recording(capture);
        Ok(())
    }

    pub fn play(&mut self) -> Result<()> {
        self.stop();
        let clip = Arc::new(self.clip.lock().unwrap().clone());
        if clip.is_empty() {
            return Err(anyhow!("nothing recorded yet"));
        }
        let (out, out_name) = playback_device()?;
        self.played.store(0, Ordering::Relaxed);
        let render = WasapiRender::start(
            &out,
            Box::new(Player { clip, pos: self.played.clone() }),
        )
        .map_err(|e| anyhow!("can't play the test clip: {e}"))?;
        info!(device = %out_name, "sound test: playing back");
        self.mode = Mode::Playing { _render: render, started: Instant::now() };
        Ok(())
    }

    /// Stop recording or playback. A stopped recording becomes the clip.
    pub fn stop(&mut self) {
        if let Mode::Recording(capture) = std::mem::replace(&mut self.mode, Mode::Idle) {
            // Dropping the capture joins its thread, so the clip is final.
            drop(capture);
            let clip = self.clip.lock().unwrap();
            self.clip_len = clip.len();
            info!(secs = clip.len() as f32 / SAMPLE_RATE as f32, "sound test: recorded");
            let path = crate::config::sound_test_path();
            if let Err(e) = crate::process_wav::write_wav(&path, &clip) {
                warn!(error = %e, "couldn't save the sound test clip");
            }
        }
    }

    pub fn is_active(&self) -> bool {
        !matches!(self.mode, Mode::Idle)
    }

    /// Ends a full recording or a finished playback. Returns true if it did.
    pub fn tick(&mut self) -> bool {
        let done = match &self.mode {
            Mode::Idle => false,
            Mode::Recording(c) => self.recorded.load(Ordering::Relaxed) >= MAX_SAMPLES || !c.is_alive(),
            Mode::Playing { _render, started } => {
                // Let the device buffer drain before closing the stream.
                let len = self.clip_len.max(1) as f32 / SAMPLE_RATE as f32;
                started.elapsed().as_secs_f32() > len + 0.3 || !_render.is_alive()
            }
        };
        if done {
            self.stop();
        }
        done
    }

    pub fn view(&self) -> TestView {
        let secs = |n: usize| n as f32 / SAMPLE_RATE as f32;
        let peak = f32::from_bits(self.peak.load(Ordering::Relaxed));
        TestView {
            recording: matches!(self.mode, Mode::Recording(_)),
            playing: matches!(self.mode, Mode::Playing { .. }),
            pos_secs: match self.mode {
                Mode::Recording(_) => secs(self.recorded.load(Ordering::Relaxed)),
                Mode::Playing { .. } => secs(self.played.load(Ordering::Relaxed)),
                Mode::Idle => 0.0,
            },
            clip_secs: (self.clip_len > 0).then(|| secs(self.clip_len)),
            peak_db: 20.0 * peak.max(1e-6).log10(),
        }
    }
}

/// Windows' default output (the one the volume icon controls), unless
/// that's a virtual device — playing the clip into a cable would send it to
/// everyone in the call, and Steam's virtual speakers go nowhere.
fn playback_device() -> Result<(String, String)> {
    let devices = DeviceList::enumerate().map_err(|e| anyhow!("listing outputs: {e}"))?;
    let real = |d: &&audio_io::Device| {
        let name = d.friendly_name.to_lowercase();
        d.virtual_cable().is_none()
            && !["cable", "steam streaming", "voicemeeter", "virtual"].iter().any(|v| name.contains(v))
    };
    let default = audio_io::wasapi_capture::default_render_id().unwrap_or_default();
    devices
        .render
        .iter()
        .filter(real)
        .find(|d| d.id == default)
        .or_else(|| devices.render.iter().filter(real).find(|d| d.is_default))
        .or_else(|| devices.render.iter().find(real))
        .map(|d| (d.id.clone(), d.friendly_name.clone()))
        .ok_or_else(|| anyhow!("no speakers or headphones to play on"))
}

struct Recorder {
    clip: Arc<Mutex<Vec<f32>>>,
    recorded: Arc<AtomicUsize>,
    peak: Arc<AtomicU32>,
}

impl FrameSink for Recorder {
    fn on_frame(&mut self, frame: &Frame) {
        let mut clip = self.clip.lock().unwrap();
        let take = MAX_SAMPLES.saturating_sub(clip.len()).min(frame.len());
        clip.extend_from_slice(&frame[..take]);
        self.recorded.store(clip.len(), Ordering::Relaxed);
        let p = frame.iter().fold(0f32, |m, s| m.max(s.abs()));
        if p > f32::from_bits(self.peak.load(Ordering::Relaxed)) {
            self.peak.store(p.to_bits(), Ordering::Relaxed);
        }
    }
}

struct Player {
    clip: Arc<Vec<f32>>,
    pos: Arc<AtomicUsize>,
}

impl FrameSource for Player {
    fn next_frame(&mut self) -> Option<Frame> {
        let pos = self.pos.load(Ordering::Relaxed);
        if pos >= self.clip.len() {
            return None;
        }
        let end = (pos + FRAME_SAMPLES).min(self.clip.len());
        let mut f = [0.0; FRAME_SAMPLES];
        f[..end - pos].copy_from_slice(&self.clip[pos..end]);
        self.pos.store(end, Ordering::Relaxed);
        Some(f)
    }
}
