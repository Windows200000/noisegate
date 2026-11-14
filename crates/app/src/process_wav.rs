//! `--process-wav <in.wav> <out-dir>`: run a recording through the same
//! front end as live capture (channel selection + sinc resampling to 48 kHz
//! mono) and then through every model / setting combination, writing one WAV
//! per variant. For comparing denoisers by ear on your own mic and noise.

use std::path::Path;

use anyhow::{bail, Context, Result};
use audio_io::resample::{ChannelMixer, SincResampler};
use dsp::{DenoiserOptions, FRAME_SAMPLES, SAMPLE_RATE};

/// (file name, backend key, options)
fn variants() -> Vec<(&'static str, &'static str, DenoiserOptions)> {
    let df = |atten_lim_db, post_filter, low_cut| DenoiserOptions { atten_lim_db, post_filter, low_cut };
    let rn = |low_cut| DenoiserOptions { low_cut, ..Default::default() };
    vec![
        ("02-dfn3-natural", "deepfilter", df(12.0, false, true)),
        ("03-dfn3-strong", "deepfilter", df(24.0, false, true)),
        ("04-dfn3-strong-cleanup", "deepfilter", df(24.0, true, true)),
        ("05-dfn3-maximum", "deepfilter", df(100.0, false, true)),
        ("06-dfn3-maximum-cleanup", "deepfilter", df(100.0, true, true)),
        ("07-rnnoise", "rnnoise", rn(true)),
        ("08-dfn3-strong-no-lowcut", "deepfilter", df(24.0, false, false)),
    ]
}

pub fn run(input: &Path, out_dir: &Path) -> Result<()> {
    let (rate, channels, samples) = read_wav(input)?;
    println!(
        "{}: {} Hz, {} ch, {:.1} s",
        input.display(),
        rate,
        channels,
        samples.len() as f64 / (rate as f64 * channels as f64)
    );

    // Front end, exactly like capture: mix active channels, resample.
    let frames = samples.len() / channels;
    let mut mixer = ChannelMixer::new(channels, rate);
    let mut mono = Vec::new();
    // Prime the channel detection on the whole clip (live capture learns it
    // within the first second).
    for chunk in samples.chunks(channels * (rate as usize / 100).max(1)) {
        let mut scratch = Vec::new();
        mixer.mix(chunk, chunk.len() / channels, &mut scratch);
    }
    mixer.mix(&samples, frames, &mut mono);
    println!("using input channel(s) {:?}", mixer.active_channels().iter().map(|c| c + 1).collect::<Vec<_>>());
    let input48 = if rate == SAMPLE_RATE {
        mono
    } else {
        let mut r = SincResampler::new(rate, SAMPLE_RATE);
        let mut out = Vec::with_capacity(mono.len() * SAMPLE_RATE as usize / rate as usize + 16);
        r.process(&mono, &mut out);
        out
    };

    std::fs::create_dir_all(out_dir)?;
    write_wav(&out_dir.join("01-input.wav"), &input48)?;
    println!("  01-input.wav  (what the denoiser receives)");

    for (name, key, opts) in variants() {
        let mut den = match dsp::denoiser_with(key, opts) {
            Ok(d) => d,
            Err(e) => {
                println!("  {name}: skipped ({e})");
                continue;
            }
        };
        let mut out = Vec::with_capacity(input48.len());
        let start = std::time::Instant::now();
        for chunk in input48.chunks(FRAME_SAMPLES) {
            let mut frame = [0f32; FRAME_SAMPLES];
            frame[..chunk.len()].copy_from_slice(chunk);
            den.process_frame(&mut frame).context("denoising")?;
            out.extend_from_slice(&frame[..chunk.len()]);
        }
        let ms_per_frame = start.elapsed().as_secs_f64() * 1000.0 / (input48.len() / FRAME_SAMPLES).max(1) as f64;
        write_wav(&out_dir.join(format!("{name}.wav")), &out)?;
        println!("  {name}.wav  ({ms_per_frame:.2} ms per 10 ms frame)");
    }
    Ok(())
}

/// Read a PCM (16/24/32-bit int) or 32-bit float WAV, interleaved f32.
fn read_wav(path: &Path) -> Result<(u32, usize, Vec<f32>)> {
    let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    if data.len() < 12 || &data[0..4] != b"RIFF" || &data[8..12] != b"WAVE" {
        bail!("{} is not a WAV file", path.display());
    }
    let u16_at = |i: usize| u16::from_le_bytes([data[i], data[i + 1]]);
    let u32_at = |i: usize| u32::from_le_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]);
    let mut pos = 12;
    let mut fmt = None;
    while pos + 8 <= data.len() {
        let id = &data[pos..pos + 4];
        let len = u32_at(pos + 4) as usize;
        let body = pos + 8;
        if body + len > data.len() && id != b"data" {
            bail!("truncated WAV chunk");
        }
        if id == b"fmt " {
            let mut tag = u16_at(body);
            if tag == 0xFFFE && len >= 26 {
                tag = u16_at(body + 24); // WAVE_FORMAT_EXTENSIBLE sub-format
            }
            fmt = Some((tag, u16_at(body + 2) as usize, u32_at(body + 4), u16_at(body + 14)));
        } else if id == b"data" {
            let (tag, ch, rate, bits) = fmt.context("WAV data before fmt chunk")?;
            let end = (body + len).min(data.len());
            let bytes = &data[body..end];
            let samples: Vec<f32> = match (tag, bits) {
                (1, 16) => bytes.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0).collect(),
                (1, 24) => bytes
                    .chunks_exact(3)
                    .map(|b| (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f32 / 8_388_608.0)
                    .collect(),
                (1, 32) => bytes
                    .chunks_exact(4)
                    .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2_147_483_648.0)
                    .collect(),
                (3, 32) => bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect(),
                _ => bail!("unsupported WAV format (tag {tag}, {bits}-bit)"),
            };
            if ch == 0 {
                bail!("WAV has 0 channels");
            }
            return Ok((rate, ch, samples));
        }
        pos = body + len + (len & 1);
    }
    bail!("no audio data in {}", path.display())
}

/// Write 48 kHz mono 16-bit PCM.
fn write_wav(path: &Path, samples: &[f32]) -> Result<()> {
    let n = samples.len() as u32;
    let mut out = Vec::with_capacity(44 + samples.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + n * 2).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    out.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(n * 2).to_le_bytes());
    for &s in samples {
        out.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0).round() as i16).to_le_bytes());
    }
    std::fs::write(path, out).with_context(|| format!("writing {}", path.display()))
}
