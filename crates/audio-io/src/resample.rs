//! Capture-side format conversion: channel selection and band-limited
//! resampling, so the denoiser gets the voice at full level and without
//! interpolation artefacts.
//!
//! - `ChannelMixer` averages only the channels that carry signal. Audio
//!   interfaces often expose a mic on one input of a stereo pair ("Analog
//!   1/2" with the mic on input 2); a plain average would halve the voice's
//!   level and add the idle input's noise.
//! - `SincResampler` is a windowed-sinc (Kaiser) resampler with a finely
//!   sampled, interpolated kernel table: no aliasing hiss or high-frequency
//!   dulling, unlike linear interpolation, for any pair of rates.

/// Which input channels to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChannelMode {
    /// Mix the channels that carry signal (see `ChannelMixer`).
    #[default]
    Auto,
    /// Only this channel (0-based).
    Only(usize),
    /// Average all channels.
    All,
}

impl ChannelMode {
    /// Parse the config value: "auto", "all", or a 1-based channel number.
    pub fn from_config(s: &str) -> Self {
        match s {
            "all" | "both" => Self::All,
            n => match n.parse::<usize>() {
                Ok(n) if n >= 1 => Self::Only(n - 1),
                _ => Self::Auto,
            },
        }
    }
}

/// Per-channel level tracking for choosing which channels to mix.
pub struct ChannelMixer {
    channels: usize,
    mode: ChannelMode,
    /// Smoothed mean-square level per channel.
    power: Vec<f64>,
    /// Channels currently mixed (weight 1) or ignored (0).
    active: Vec<bool>,
    /// Frames since the active set was last re-evaluated.
    since_eval: usize,
    eval_every: usize,
    /// Smoothing factor per buffer, from the time constant.
    rate: u32,
}

impl ChannelMixer {
    /// A channel is mixed if it's within this many dB of the loudest one.
    const WITHIN_DB: f64 = 20.0;
    /// Below this level a channel counts as unconnected (dBFS).
    const SILENT_DB: f64 = -80.0;

    pub fn new(channels: usize, rate: u32) -> Self {
        Self::with_mode(channels, rate, ChannelMode::Auto)
    }

    pub fn with_mode(channels: usize, rate: u32, mode: ChannelMode) -> Self {
        let active = match mode {
            ChannelMode::Only(c) if c < channels => (0..channels).map(|i| i == c).collect(),
            _ => vec![true; channels],
        };
        Self {
            channels,
            mode,
            power: vec![0.0; channels],
            active,
            since_eval: 0,
            // Re-evaluate once a second: often enough to follow a re-plugged
            // mic, rarely enough not to flip-flop during pauses.
            eval_every: rate as usize,
            rate,
        }
    }

    /// Indices of the channels currently mixed (for logging).
    pub fn active_channels(&self) -> Vec<usize> {
        (0..self.channels).filter(|&c| self.active[c]).collect()
    }

    /// Down-mix interleaved `frames` into `out` (cleared first). Returns
    /// true if the set of mixed channels changed.
    pub fn mix(&mut self, interleaved: &[f32], frames: usize, out: &mut Vec<f32>) -> bool {
        out.clear();
        let ch = self.channels;
        if ch == 1 {
            out.extend_from_slice(&interleaved[..frames]);
            return false;
        }

        // Track levels: one-pole smoothing with a ~2 s time constant.
        if frames > 0 {
            let alpha = 1.0 - (-(frames as f64) / (2.0 * self.rate as f64)).exp();
            for c in 0..ch {
                let ms = (0..frames)
                    .map(|f| {
                        let s = interleaved[f * ch + c] as f64;
                        s * s
                    })
                    .sum::<f64>()
                    / frames as f64;
                self.power[c] += alpha * (ms - self.power[c]);
            }
        }

        let mut changed = false;
        self.since_eval += frames;
        if self.mode == ChannelMode::Auto && self.since_eval >= self.eval_every {
            self.since_eval = 0;
            let db = |p: f64| 10.0 * p.max(1e-20).log10();
            let loudest = self.power.iter().copied().fold(0.0, f64::max);
            if db(loudest) > Self::SILENT_DB {
                for c in 0..ch {
                    let on = db(self.power[c]) > Self::SILENT_DB
                        && db(self.power[c]) >= db(loudest) - Self::WITHIN_DB;
                    changed |= on != self.active[c];
                    self.active[c] = on;
                }
            }
        }

        let n = self.active.iter().filter(|&&a| a).count().max(1) as f32;
        for f in 0..frames {
            let base = f * ch;
            let mut acc = 0.0f32;
            for c in 0..ch {
                if self.active[c] {
                    acc += interleaved[base + c];
                }
            }
            out.push(acc / n);
        }
        changed
    }
}

/// Streaming windowed-sinc resampler (mono, push model).
pub struct SincResampler {
    /// Input samples per output sample.
    step: f64,
    /// Kernel cutoff relative to the input Nyquist (≤ 1).
    cutoff: f64,
    /// Half-width of the kernel, in input samples.
    half: usize,
    /// Kernel h(u) for u = 0, 1/PHASES, …, ZEROS (symmetric).
    table: Vec<f32>,
    /// Input history: past samples the kernel still needs, plus new input.
    hist: Vec<f32>,
    /// Position of the next output sample, in `hist` indices.
    pos: f64,
}

impl SincResampler {
    /// Zero crossings on each side of the kernel (at cutoff 1).
    const ZEROS: usize = 16;
    /// Table resolution per zero crossing.
    const PHASES: usize = 512;
    /// Kaiser window shape: ~90 dB stopband.
    const BETA: f64 = 9.0;

    pub fn new(src_rate: u32, dst_rate: u32) -> Self {
        let step = src_rate as f64 / dst_rate as f64;
        // Pass band up to 95% of the lower Nyquist, so downsampling doesn't
        // alias and upsampling doesn't image.
        let cutoff = 0.95 * (1.0f64).min(1.0 / step);
        let half = (Self::ZEROS as f64 / cutoff).ceil() as usize;
        let n = Self::ZEROS * Self::PHASES;
        let i0_beta = bessel_i0(Self::BETA);
        let table = (0..=n + 1)
            .map(|j| {
                let u = j as f64 / Self::PHASES as f64; // in zero crossings
                let sinc = if u == 0.0 {
                    1.0
                } else {
                    (std::f64::consts::PI * u).sin() / (std::f64::consts::PI * u)
                };
                let r = (u / Self::ZEROS as f64).min(1.0);
                let w = bessel_i0(Self::BETA * (1.0 - r * r).sqrt()) / i0_beta;
                (sinc * w) as f32
            })
            .collect();
        Self {
            step,
            cutoff,
            half,
            table,
            // `half` zeros of "past" so the first output lines up with the
            // first input sample (no time offset, only look-ahead latency).
            hist: vec![0.0; half],
            pos: half as f64,
        }
    }

    /// Kernel value at `t` input samples from the centre.
    #[inline]
    fn kernel(&self, t: f64) -> f32 {
        let u = (t * self.cutoff).abs() * Self::PHASES as f64;
        let j = u as usize;
        if j >= Self::ZEROS * Self::PHASES {
            return 0.0;
        }
        let frac = (u - j as f64) as f32;
        let a = self.table[j];
        a + (self.table[j + 1] - a) * frac
    }

    /// Resample `input`, appending to `out` (not cleared).
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        self.hist.extend_from_slice(input);
        let half = self.half as isize;
        let gain = self.cutoff as f32;
        // An output at `pos` needs samples up to floor(pos) + half.
        while (self.pos.floor() as isize + half) < self.hist.len() as isize {
            let centre = self.pos.floor() as isize;
            let frac = self.pos - centre as f64;
            let mut acc = 0.0f32;
            for k in (1 - half)..=half {
                let x = self.hist[(centre + k) as usize];
                acc += x * self.kernel(k as f64 - frac);
            }
            out.push(acc * gain);
            self.pos += self.step;
        }
        // Drop history no future output needs.
        let keep_from = (self.pos.floor() as isize - half + 1).max(0) as usize;
        if keep_from > 0 {
            self.hist.drain(..keep_from);
            self.pos -= keep_from as f64;
        }
    }
}

/// Modified Bessel function of the first kind, order 0 (for the Kaiser window).
fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let q = x * x / 4.0;
    for k in 1..50 {
        term *= q / (k * k) as f64;
        sum += term;
        if term < 1e-12 * sum {
            break;
        }
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rms(x: &[f32]) -> f64 {
        (x.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / x.len() as f64).sqrt()
    }

    /// Resample a sine and compare against the ideal sine at the new rate.
    fn sine_error_db(src: u32, dst: u32, hz: f64) -> f64 {
        let n = src as usize; // 1 s
        let input: Vec<f32> = (0..n)
            .map(|i| (0.5 * (2.0 * std::f64::consts::PI * hz * i as f64 / src as f64).sin()) as f32)
            .collect();
        let mut r = SincResampler::new(src, dst);
        let mut out = Vec::new();
        for chunk in input.chunks(441) {
            r.process(chunk, &mut out);
        }
        // Output sample m corresponds to input time m·step.
        let skip = dst as usize / 10;
        let ideal: Vec<f32> = (0..out.len())
            .map(|m| (0.5 * (2.0 * std::f64::consts::PI * hz * m as f64 / dst as f64).sin()) as f32)
            .collect();
        let err: Vec<f32> = out[skip..out.len() - skip]
            .iter()
            .zip(&ideal[skip..out.len() - skip])
            .map(|(a, b)| a - b)
            .collect();
        20.0 * (rms(&err) / 0.5f64.sqrt() / 0.5).log10()
    }

    #[test]
    fn upsample_44k1_to_48k_is_clean() {
        for hz in [100.0, 1000.0, 8000.0, 16000.0] {
            let e = sine_error_db(44_100, 48_000, hz);
            assert!(e < -60.0, "{hz} Hz: error {e:.1} dB");
        }
    }

    #[test]
    fn downsample_96k_to_48k_is_clean() {
        for hz in [100.0, 1000.0, 8000.0] {
            let e = sine_error_db(96_000, 48_000, hz);
            assert!(e < -60.0, "{hz} Hz: error {e:.1} dB");
        }
    }

    #[test]
    fn output_count_matches_ratio() {
        let mut r = SincResampler::new(44_100, 48_000);
        let mut out = Vec::new();
        for _ in 0..100 {
            r.process(&[0.0; 441], &mut out);
        }
        // 1 s in → 1 s out, minus the kernel's look-ahead.
        assert!((out.len() as i64 - 48_000).abs() < 40, "{}", out.len());
    }

    #[test]
    fn mixer_ignores_a_silent_channel() {
        let mut m = ChannelMixer::new(2, 48_000);
        let mut out = Vec::new();
        // Right channel carries a signal, left is silent.
        let buf: Vec<f32> = (0..48_000).flat_map(|i| [0.0, ((i as f32) * 0.05).sin() * 0.3]).collect();
        for chunk in buf.chunks(960) {
            m.mix(chunk, chunk.len() / 2, &mut out);
        }
        assert_eq!(m.active_channels(), vec![1]);
        // Now at full level, not halved.
        let peak = out.iter().fold(0.0f32, |a, &s| a.max(s.abs()));
        assert!(peak > 0.29, "peak {peak}");
    }

    #[test]
    fn mixer_keeps_both_channels_of_real_stereo() {
        let mut m = ChannelMixer::new(2, 48_000);
        let mut out = Vec::new();
        let buf: Vec<f32> = (0..48_000)
            .flat_map(|i| {
                let s = ((i as f32) * 0.05).sin() * 0.3;
                [s, s * 0.5]
            })
            .collect();
        for chunk in buf.chunks(960) {
            m.mix(chunk, chunk.len() / 2, &mut out);
        }
        assert_eq!(m.active_channels(), vec![0, 1]);
    }
}
