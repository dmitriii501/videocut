//! Audio decoding to 16 kHz mono f32 via ffmpeg-next (native FFI).
//!
//! Produces a plain `Vec<f32>` the analysis code can slice without numpy.

use anyhow::{Context, Result};
use ffmpeg_next as ffmpeg;
use ffmpeg::format::sample::Type as SampleType;
use ffmpeg::software::resampling;
use ffmpeg::util::frame::audio::Audio;
use ffmpeg::ChannelLayout;

use std::path::Path;

pub const SAMPLE_RATE: u32 = 16_000;

/// Decode any media file's first audio stream to mono f32 @ 16 kHz.
pub fn decode_mono(path: &Path) -> Result<Vec<f32>> {
    ffmpeg::init().ok();
    let mut ictx = ffmpeg::format::input(&path)
        .with_context(|| format!("open {}", path.display()))?;

    let stream = ictx
        .streams()
        .best(ffmpeg::media::Type::Audio)
        .ok_or_else(|| anyhow::anyhow!("no audio stream in {}", path.display()))?;
    let stream_index = stream.index();
    let time_base = stream.time_base();

    let ctx = ffmpeg::codec::context::Context::from_parameters(stream.parameters())?;
    let mut decoder = ctx.decoder().audio()?;

    let mut resampler = resampling::Context::get(
        decoder.format(),
        decoder.channel_layout(),
        decoder.rate(),
        ffmpeg::format::Sample::F32(SampleType::Planar),
        ChannelLayout::MONO,
        SAMPLE_RATE,
    )?;

    let mut out: Vec<f32> = Vec::new();
    let mut push_frame = |frame: &Audio, out: &mut Vec<f32>| -> Result<()> {
        let mut converted = Audio::empty();
        resampler.run(frame, &mut converted)?;
        // Planar mono: one plane of f32 samples.
        let n = converted.samples();
        if n > 0 {
            unsafe {
                let data = converted.data(0).as_ptr() as *const f32;
                out.extend_from_slice(std::slice::from_raw_parts(data, n));
            }
        }
        Ok(())
    };

    let mut frame = Audio::empty();
    for (s, packet) in ictx.packets() {
        if s.index() != stream_index {
            continue;
        }
        decoder.send_packet(&packet)?;
        while decoder.receive_frame(&mut frame).is_ok() {
            push_frame(&frame, &mut out)?;
        }
    }
    decoder.send_eof()?;
    while decoder.receive_frame(&mut frame).is_ok() {
        push_frame(&frame, &mut out)?;
    }
    // Flush the resampler.
    let mut flushed = Audio::empty();
    while resampler.flush(&mut flushed).is_ok() && flushed.samples() > 0 {
        let n = flushed.samples();
        unsafe {
            let data = flushed.data(0).as_ptr() as *const f32;
            out.extend_from_slice(std::slice::from_raw_parts(data, n));
        }
    }

    let _ = time_base; // silence unused on some feature sets
    Ok(out)
}
