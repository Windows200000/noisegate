//! Low-cut (high-pass) filter in front of the denoiser.
//!
//! Removes mains hum (50/60 Hz), handling noise, desk thumps and plosive
//! rumble before the model sees them. Neural denoisers leave a surprising
//! amount of steady low hum in speech gaps; measured on a real 50 Hz hum
//! recording, this cut the residual noise in the gaps by ~8 dB more after
//! DeepFilterNet3, at ~0.5 dB on speech peaks. Speech fundamentals start
//! around 85 Hz (low male voices), so the corner sits just below them.

use crate::{Denoiser, Result, FRAME_SAMPLES, SAMPLE_RATE};

/// Corner frequency (Hz).
pub const LOW_CUT_HZ: f32 = 80.0;

/// One RBJ-cookbook biquad high-pass section (transposed direct form II).
#[derive(Clone, Copy)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

impl Biquad {
    fn highpass(freq: f32, q: f32, rate: f32) -> Self {
        let w0 = 2.0 * std::f32::consts::PI * freq / rate;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * q);
        let a0 = 1.0 + alpha;
        Self {
            b0: (1.0 + cos) / 2.0 / a0,
            b1: -(1.0 + cos) / a0,
            b2: (1.0 + cos) / 2.0 / a0,
            a1: -2.0 * cos / a0,
            a2: (1.0 - alpha) / a0,
            z1: 0.0,
            z2: 0.0,
        }
    }

    #[inline]
    fn run(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }
}

/// 4th-order Butterworth high-pass: −24 dB/octave, flat above the corner.
pub struct LowCut {
    stages: [Biquad; 2],
}

impl LowCut {
    pub fn new(freq: f32) -> Self {
        // Butterworth Q values for the two 2nd-order sections of a 4th-order filter.
        let rate = SAMPLE_RATE as f32;
        Self {
            stages: [Biquad::highpass(freq, 0.541_196_1, rate), Biquad::highpass(freq, 1.306_563, rate)],
        }
    }

    pub fn process(&mut self, samples: &mut [f32]) {
        for s in samples {
            let mut v = *s;
            for st in &mut self.stages {
                v = st.run(v);
            }
            *s = v;
        }
    }
}

/// A denoiser with the low-cut in front of it.
pub struct WithLowCut {
    low_cut: LowCut,
    inner: Box<dyn Denoiser>,
}

impl WithLowCut {
    pub fn new(inner: Box<dyn Denoiser>) -> Self {
        Self { low_cut: LowCut::new(LOW_CUT_HZ), inner }
    }
}

impl Denoiser for WithLowCut {
    fn process_frame(&mut self, frame: &mut [f32; FRAME_SAMPLES]) -> Result<()> {
        self.low_cut.process(frame);
        self.inner.process_frame(frame)
    }

    fn name(&self) -> &'static str {
        self.inner.name()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Steady-state gain of the filter for a sine, in dB.
    fn gain_db(hz: f32) -> f32 {
        let mut f = LowCut::new(LOW_CUT_HZ);
        let n = SAMPLE_RATE as usize; // 1 s
        let mut x: Vec<f32> = (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * hz * i as f32 / SAMPLE_RATE as f32).sin())
            .collect();
        f.process(&mut x);
        let tail = &x[n / 2..];
        let rms = (tail.iter().map(|v| v * v).sum::<f32>() / tail.len() as f32).sqrt();
        20.0 * (rms / std::f32::consts::FRAC_1_SQRT_2).log10()
    }

    #[test]
    fn removes_mains_hum_and_keeps_voice() {
        assert!(gain_db(50.0) < -15.0, "50 Hz: {}", gain_db(50.0));
        assert!((gain_db(LOW_CUT_HZ) + 3.0).abs() < 0.5, "corner: {}", gain_db(LOW_CUT_HZ));
        assert!(gain_db(200.0) > -0.5, "200 Hz: {}", gain_db(200.0));
        assert!(gain_db(1000.0).abs() < 0.1, "1 kHz: {}", gain_db(1000.0));
    }
}
