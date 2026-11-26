//! Compose the final vertical reel natively with ffmpeg-next:
//! stream screen on top (letterboxed on a blurred fill), gameplay clip or
//! streamer webcam below, burned-in ASS subtitles. Video -> H.264, audio ->
//! AAC, muxed to MP4.
//!
//! Layout (default 1080x1920, 9:16):
//!   TOP    (50%) = full stream frame, letterboxed on a blurred fill.
//!   BOTTOM (50%) = gameplay strip if requested, else the streamer webcam
//!                  panel cropped to the band and scaled up.

use anyhow::{anyhow, Result};
use ffmpeg_next as ffmpeg;
use ffmpeg::format::Pixel;
use ffmpeg::ChannelLayout;
use ffmpeg::{codec, format, frame, media, Dictionary, Rational, Rescale};
use std::path::{Path, PathBuf};

use crate::config;
use crate::utils::{ensure_dir, has_audio, log, short_hash, video_size};
use crate::vod::webcam::{Panel, DEFAULT_PANEL};

#[derive(Debug, Clone, Copy)]
pub struct Layout {
    pub width: u32,
    pub height: u32,
    pub top_h: u32,
    pub bot_h: u32,
}

pub fn layout() -> Layout {
    let w = config::reel_w() / 2 * 2;
    let h = config::reel_h() / 2 * 2;
    let top_h = h / 2 / 2 * 2;
    let bot_h = h - top_h;
    Layout {
        width: w,
        height: h,
        top_h,
        bot_h,
    }
}

/// Static webcam crop rect (x, y, w, h) in source pixels, centered on the
/// detected panel and shaped to the output band aspect.
fn webcam_crop(src_w: u32, src_h: u32, panel: Panel, panel_w: u32, panel_h: u32) -> (u32, u32, u32, u32) {
    let px0 = panel.x0 * src_w as f64;
    let py0 = panel.y0 * src_h as f64;
    let px1 = panel.x1 * src_w as f64;
    let py1 = panel.y1 * src_h as f64;
    let panel_px_w = (px1 - px0).max(2.0);

    let aspect = panel_w as f64 / panel_h as f64;
    let mut crop_w = panel_px_w;
    let mut crop_h = crop_w / aspect;
    if crop_h > src_h as f64 {
        crop_h = src_h as f64;
        crop_w = crop_h * aspect;
    }
    let cx = (px0 + px1) / 2.0;
    let cy = (py0 + py1) / 2.0;
    let x = (cx - crop_w / 2.0).clamp(0.0, (src_w as f64 - crop_w).max(0.0));
    let y = (cy - crop_h / 2.0).clamp(0.0, (src_h as f64 - crop_h).max(0.0));
    (
        x.round().max(0.0) as u32,
        y.round().max(0.0) as u32,
        (crop_w as u32) / 2 * 2,
        (crop_h as u32) / 2 * 2,
    )
}

fn escape_filter_path(path: &Path) -> String {
    // Inside a filtergraph argument, escape backslashes, colons, quotes.
    let s = path.to_string_lossy();
    s.replace('\\', "\\\\")
        .replace(':', "\\:")
        .replace('\'', "\\'")
}

/// Escape a path for `movie=` by wrapping it in single quotes (so `:` and `,`
/// inside the path stay literal) and escaping only backslashes and quotes.
fn escape_movie_path(path: &Path) -> String {
    let s = path.to_string_lossy();
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// A pre-rendered animated banner overlay to composite during [start, end].
struct BannerOverlay {
    path: PathBuf,
    start: f64,
    end: f64,
}

/// Cyrillic-capable bold fonts we try, in order.
const BANNER_FONTS: &[&str] = &[
    "/usr/share/fonts/noto/NotoSans-Bold.ttf",
    "/usr/share/fonts/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
    "/usr/share/fonts/dejavu/DejaVuSans-Bold.ttf",
];

fn resolve_banner_font() -> Option<PathBuf> {
    if let Some(p) = config::banner_font_file() {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return Some(pb);
        }
    }
    BANNER_FONTS.iter().map(PathBuf::from).find(|p| p.exists())
}

/// Escape text for a `drawtext=text='...'` value.
fn escape_drawtext(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('\'', "\\'")
        .replace(':', "\\:")
        .replace('%', "\\%")
}

/// Generate (once, cached) the animated alpha banner overlay as a qtrle `.mov`
/// sized to the reel layout, then return its path.
fn banner_overlay_asset(lay: Layout) -> Option<PathBuf> {
    let dur = config::banner_duration();
    if dur <= 0.0 {
        return None;
    }
    let font = resolve_banner_font()?;
    let site_raw = config::banner_site();
    let dir = config::cache().join("assets");
    ensure_dir(&dir).ok()?;
    let path = dir.join(format!(
        "banner-v2-{}x{}-{:.2}-{}.mov",
        lay.width,
        lay.height,
        dur,
        &short_hash(&site_raw)[..8]
    ));
    if path.exists() && path.metadata().map(|m| m.len() > 0).unwrap_or(false) {
        return Some(path);
    }

    // Sizes are tuned on a 1080-wide reel and scale with the canvas.
    let s = lay.width as f64 / 1080.0;
    let bar = (150.0 * s).round().max(48.0) as i32;
    let inset = (36.0 * s).round() as i32;
    let arm = (72.0 * s).round() as i32;
    let thick = (8.0 * s).round().max(3.0) as i32;
    let gap = (24.0 * s).round() as i32;
    let fs_mark = (280.0 * s).round() as u32;
    let fs_sub = (52.0 * s).round() as u32;
    let fs_top = (64.0 * s).round() as u32;
    let fs_site = (54.0 * s).round() as u32;
    let rule_h = (10.0 * s).round().max(3.0) as i32;
    let rule_above = (300.0 * s).round() as i32;
    let rule_below = (250.0 * s).round() as i32;
    let sub_dy = (150.0 * s).round() as i32;
    let top_ty = (46.0 * s).round() as i32;
    let bot_pad = (112.0 * s).round() as i32;
    let w = lay.width as i32;
    let h = lay.height as i32;
    let y_top = bar + gap;
    let y_bot_v = h - bar - gap - arm;
    let y_bot_h = h - bar - gap - thick;
    let x_r_v = w - inset - thick;
    let x_r_h = w - inset - arm;
    let font_s = escape_drawtext(&font.to_string_lossy());
    let site = escape_drawtext(&site_raw);
    let fade_out_d = 0.32_f64.min(dur / 4.0);
    let fade_in = 0.22_f64.min(dur / 4.0);
    let fade_out = (dur - fade_out_d).max(0.0);

    let vf = format!(
        "format=rgba,\
         drawbox=x=0:y=0:w=iw:h=ih:color=0x090A0D@0.88:t=fill,\
         drawbox=x=0:y=0:w=iw:h='{bar}*min(1\\,t/0.28)':color=0xFF5500:t=fill,\
         drawbox=x=0:y='ih-{bar}*min(1\\,t/0.28)':w=iw:h='{bar}*min(1\\,t/0.28)+1':color=0xFF5500:t=fill,\
         drawbox=x={inset}:y={y_top}:w={arm}:h={thick}:color=0xFF5500:t=fill,\
         drawbox=x={inset}:y={y_top}:w={thick}:h={arm}:color=0xFF5500:t=fill,\
         drawbox=x={x_r_h}:y={y_top}:w={arm}:h={thick}:color=0xFF5500:t=fill,\
         drawbox=x={x_r_v}:y={y_top}:w={thick}:h={arm}:color=0xFF5500:t=fill,\
         drawbox=x={inset}:y={y_bot_v}:w={thick}:h={arm}:color=0xFF5500:t=fill,\
         drawbox=x={inset}:y={y_bot_h}:w={arm}:h={thick}:color=0xFF5500:t=fill,\
         drawbox=x={x_r_v}:y={y_bot_v}:w={thick}:h={arm}:color=0xFF5500:t=fill,\
         drawbox=x={x_r_h}:y={y_bot_h}:w={arm}:h={thick}:color=0xFF5500:t=fill,\
         drawbox=x='(iw-iw*0.72*min(1\\,max(0\\,(t-0.18)/0.32)))/2':y='ih/2-{rule_above}':w='iw*0.72*min(1\\,max(0\\,(t-0.18)/0.32))':h={rule_h}:color=0xFF5500:t=fill,\
         drawbox=x='(iw-iw*0.72*min(1\\,max(0\\,(t-0.18)/0.32)))/2':y='ih/2+{rule_below}':w='iw*0.72*min(1\\,max(0\\,(t-0.18)/0.32))':h={rule_h}:color=0xFF5500:t=fill,\
         drawbox=enable='lt(t\\,0.55)':x=0:y='(ih-{bar})*min(t/0.45\\,1)':w=iw:h={thick}:color=0xF4F1EA@0.95:t=fill,\
         drawtext=fontfile='{font}':text='ЧИК':fontcolor=0xF4F1EA:fontsize={fs_mark}:x=(w-text_w)/2:y=(h-text_h)/2-30,\
         drawtext=fontfile='{font}':text='РЕЖЕТ СТРИМ':fontcolor=0xFF5500:fontsize={fs_sub}:x=(w-text_w)/2:y=(h/2)+{sub_dy},\
         drawtext=fontfile='{font}':text='CHIK':fontcolor=0x090A0D:fontsize={fs_top}:x=(w-text_w)/2:y={top_ty},\
         drawtext=fontfile='{font}':text='{site}':fontcolor=0x090A0D:fontsize={fs_site}:x=(w-text_w)/2:y=h-{bot_pad},\
         fade=t=in:st=0:d={fade_in}:alpha=1,fade=t=out:st={fade_out}:d={fade_out_d}:alpha=1",
        bar = bar,
        inset = inset,
        y_top = y_top,
        arm = arm,
        thick = thick,
        x_r_h = x_r_h,
        x_r_v = x_r_v,
        y_bot_v = y_bot_v,
        y_bot_h = y_bot_h,
        rule_above = rule_above,
        rule_below = rule_below,
        rule_h = rule_h,
        font = font_s,
        fs_mark = fs_mark,
        fs_sub = fs_sub,
        sub_dy = sub_dy,
        fs_top = fs_top,
        top_ty = top_ty,
        site = site,
        fs_site = fs_site,
        bot_pad = bot_pad,
        fade_in = fade_in,
        fade_out = fade_out,
        fade_out_d = fade_out_d,
    );

    let src = format!(
        "color=c=0x090A0D@0.0:s={}x{}:r={}:d={}",
        lay.width,
        lay.height,
        config::fps(),
        dur
    );
    log(format!(
        "banner: rendering overlay {} ({}x{}, {:.1}s)",
        path.display(),
        lay.width,
        lay.height,
        dur
    ));
    let status = std::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            &src,
            "-vf",
            &vf,
            "-c:v",
            "qtrle",
        ])
        .arg(&path)
        .status();
    match status {
        Ok(s) if s.success() => Some(path),
        Ok(s) => {
            log(format!("banner: ffmpeg failed with {s}"));
            None
        }
        Err(e) => {
            log(format!("banner: failed to run ffmpeg: {e}"));
            None
        }
    }
}

/// Build the filtergraph spec (input pad unlabeled -> [out]).
///
/// The bottom band is either the gameplay strip (when `gameplay` is set) or the
/// streamer's webcam crop.
#[allow(clippy::too_many_arguments)]
fn build_spec(
    lay: Layout,
    src_w: u32,
    src_h: u32,
    ass_path: Option<&Path>,
    panel: Panel,
    gameplay: Option<&Path>,
    dur: f64,
    banner: Option<&BannerOverlay>,
) -> String {
    let w = lay.width;
    let top = lay.top_h;
    let bot = lay.bot_h;
    let blur = format!("boxblur={}:{}", config::blur_radius(), config::blur_power());

    let mut spec = match gameplay {
        Some(strip) => format!(
            "split=2[sbg][sfg];\
             [sbg]scale={w}:{top}:force_original_aspect_ratio=increase:flags=lanczos,crop={w}:{top},{blur}[bg];\
             [sfg]scale={w}:{top}:force_original_aspect_ratio=decrease:flags=lanczos[fg];\
             [bg][fg]overlay=(W-w)/2:(H-h)/2,setsar=1[vtop];\
             movie={},trim=duration={dur},setpts=PTS-STARTPTS,setsar=1[vbot];\
             [vtop][vbot]vstack=inputs=2[vs];",
            escape_movie_path(strip),
        ),
        None => {
            let (wx, wy, ww, wh) = webcam_crop(src_w, src_h, panel, w, bot);
            format!(
                "split=3[sbg][sfg][swc];\
                 [sbg]scale={w}:{top}:force_original_aspect_ratio=increase:flags=lanczos,crop={w}:{top},{blur}[bg];\
                 [sfg]scale={w}:{top}:force_original_aspect_ratio=decrease:flags=lanczos[fg];\
                 [bg][fg]overlay=(W-w)/2:(H-h)/2,setsar=1[vtop];\
                 [swc]crop={ww}:{wh}:{wx}:{wy},scale={w}:{bot}:flags=lanczos,setsar=1[vbot];\
                 [vtop][vbot]vstack=inputs=2[vs];"
            )
        }
    };

    match ass_path {
        Some(p) if p.exists() && p.metadata().map(|m| m.len() > 0).unwrap_or(false) => {
            spec.push_str(&format!("[vs]ass='{}'[sub]", escape_filter_path(p)));
        }
        _ => spec.push_str("[vs]null[sub]"),
    }

    match banner {
        Some(b) => {
            // Play the alpha overlay once, shifted to start at b.start; then
            // overlay it only during its window so the reel continues after.
            spec.push_str(&format!(
                ";movie={},setpts=PTS-STARTPTS+{start}/TB[bn];\
                 [sub][bn]overlay=0:0:eof_action=pass:enable='between(t,{start},{end})'[out]",
                escape_movie_path(&b.path),
                start = b.start,
                end = b.end,
            ));
        }
        None => spec.push_str(";[sub]null[out]"),
    }
    spec
}

struct VideoPipeline {
    filter: ffmpeg::filter::Graph,
    decoder: codec::decoder::Video,
    encoder: codec::encoder::Video,
    stream_index: usize,
    in_time_base: Rational,
    out_time_base: Rational,
    start_pts: i64,
    end_pts: i64,
    fps: f64,
    frame_count: i64,
    pending_pts: std::collections::VecDeque<i64>,
}

impl VideoPipeline {
    fn add_frame_to_filter(&mut self, frame: &frame::Video) -> Result<()> {
        self.filter
            .get("in")
            .ok_or_else(|| anyhow!("missing filter 'in'"))?
            .source()
            .add(frame)
            .map_err(|e| anyhow!("filter add: {e}"))
    }

    fn flush_filter(&mut self) {
        if let Some(mut ctx) = self.filter.get("in") {
            let _ = ctx.source().flush();
        }
    }

    fn drain_filtered(&mut self, octx: &mut format::context::Output) -> Result<()> {
        loop {
            let mut filtered = frame::Video::empty();
            let res = self
                .filter
                .get("out")
                .ok_or_else(|| anyhow!("missing filter 'out'"))?
                .sink()
                .frame(&mut filtered);
            match res {
                Ok(()) => self.encode_frame(&filtered, octx)?,
                Err(_) => break,
            }
        }
        Ok(())
    }

    fn encode_frame(&mut self, filtered: &frame::Video, octx: &mut format::context::Output) -> Result<()> {
        // Filtered frames carry no pts; pull the intended output tick from the
        // queue filled in receive_decoded().
        let out_pts = match self.pending_pts.pop_front() {
            Some(p) => p,
            None => return Ok(()),
        };
        let mut f = filtered.clone();
        f.set_pts(Some(out_pts));
        self.frame_count = out_pts;
        self.encoder
            .send_frame(&f)
            .map_err(|e| anyhow!("video encoder send: {e}"))?;
        self.drain_encoded(octx)
    }

    fn drain_encoded(&mut self, octx: &mut format::context::Output) -> Result<()> {
        let mut pkt = ffmpeg::Packet::empty();
        while self.encoder.receive_packet(&mut pkt).is_ok() {
            pkt.set_stream(self.stream_index);
            pkt.rescale_ts(self.out_time_base, octx.stream(self.stream_index).unwrap().time_base());
            pkt.write_interleaved(octx)
                .map_err(|e| anyhow!("write video packet: {e}"))?;
        }
        Ok(())
    }

    fn receive_decoded(&mut self, octx: &mut format::context::Output) -> Result<()> {
        let mut decoded = frame::Video::empty();
        while self.decoder.receive_frame(&mut decoded).is_ok() {
            // Decoded frames carry absolute stream pts; drop the keyframe
            // pre-roll before the requested clip start.
            let in_pts = decoded.timestamp().unwrap_or(0) - self.start_pts;
            if in_pts < 0 {
                continue;
            }
            let t_sec = in_pts as f64 * f64::from(self.in_time_base);
            let clip_dur = self.end_pts as f64 * f64::from(self.in_time_base)
                - self.start_pts as f64 * f64::from(self.in_time_base);
            if t_sec >= clip_dur {
                continue;
            }
            let out_pts = (t_sec * self.fps).round() as i64;
            // Higher-fps source: several inputs map to one output tick.
            if out_pts <= self.frame_count {
                continue;
            }
            self.frame_count = out_pts;
            decoded.set_pts(Some(in_pts));
            self.add_frame_to_filter(&decoded)?;
            self.pending_pts.push_back(out_pts);
            self.drain_filtered(octx)?;
        }
        Ok(())
    }
}

fn open_video_encoder(
    octx: &mut format::context::Output,
    width: u32,
    height: u32,
    time_base: Rational,
    global_header: bool,
) -> Result<(codec::encoder::Video, usize)> {
    let codec = codec::encoder::find(codec::Id::H264)
        .ok_or_else(|| anyhow!("libx264 encoder not available"))?;
    let mut ost = octx
        .add_stream(codec)
        .map_err(|e| anyhow!("add video stream: {e}"))?;
    let index = ost.index();

    let mut enc = codec::context::Context::new_with_codec(codec)
        .encoder()
        .video()
        .map_err(|e| anyhow!("video encoder ctx: {e}"))?;
    enc.set_width(width);
    enc.set_height(height);
    enc.set_format(Pixel::YUV420P);
    enc.set_frame_rate(Some(Rational(config::fps() as i32, 1)));
    enc.set_time_base(time_base);
    if global_header {
        enc.set_flags(codec::Flags::GLOBAL_HEADER);
    }

    let mut opts = Dictionary::new();
    opts.set("preset", &config::video_preset());
    opts.set("crf", &config::video_crf().to_string());

    let enc = enc
        .open_with(opts)
        .map_err(|e| anyhow!("open x264: {e}"))?;
    ost.set_parameters(&enc);
    Ok((enc, index))
}

/// Parse an audio bitrate like "128k", "192k" or "128000" into bits/sec.
fn parse_bitrate(spec: &str) -> usize {
    let s = spec.trim().to_lowercase();
    let (num, mult) = if let Some(v) = s.strip_suffix('k') {
        (v, 1000.0)
    } else if let Some(v) = s.strip_suffix('m') {
        (v, 1_000_000.0)
    } else {
        (s.as_str(), 1.0)
    };
    num.trim()
        .parse::<f64>()
        .map(|n| (n * mult) as usize)
        .unwrap_or(128_000)
}

fn open_audio_encoder(
    octx: &mut format::context::Output,
    rate: u32,
    layout: ChannelLayout,
    sample_fmt: ffmpeg::format::Sample,
    global_header: bool,
) -> Result<(codec::encoder::Audio, usize)> {
    let codec = codec::encoder::find(codec::Id::AAC)
        .ok_or_else(|| anyhow!("aac encoder not available"))?;
    let mut ost = octx
        .add_stream(codec)
        .map_err(|e| anyhow!("add audio stream: {e}"))?;
    let index = ost.index();

    let mut enc = codec::context::Context::new_with_codec(codec)
        .encoder()
        .audio()
        .map_err(|e| anyhow!("audio encoder ctx: {e}"))?;
    enc.set_rate(rate as i32);
    enc.set_channel_layout(layout);
    enc.set_format(sample_fmt);
    enc.set_bit_rate(parse_bitrate(&config::audio_bitrate()));
    enc.set_time_base(Rational(1, rate as i32));
    if global_header {
        enc.set_flags(codec::Flags::GLOBAL_HEADER);
    }
    let enc = enc.open_as(codec).map_err(|e| anyhow!("open aac: {e}"))?;
    ost.set_parameters(&enc);
    Ok((enc, index))
}

/// Render one reel segment.
#[allow(clippy::too_many_arguments)]
pub fn render_reel(
    source: &Path,
    start: f64,
    dur: f64,
    ass_path: Option<&Path>,
    out_path: &Path,
    panel: Option<Panel>,
    banner: bool,
    gameplay_clip: Option<&Path>,
) -> Result<PathBuf> {
    ffmpeg::init().ok();
    if let Some(parent) = out_path.parent() {
        ensure_dir(parent)?;
    }

    let lay = layout();
    let (src_w, src_h) = video_size(source)?;
    let panel = panel.unwrap_or(Panel::new(DEFAULT_PANEL));

    // Materialise the bottom-band gameplay strip, if requested. On failure we
    // log and fall back to the webcam band rather than failing the whole reel.
    let gameplay_strip = match gameplay_clip {
        Some(clip) => {
            let strip = crate::vod::gameplay::strip_for(lay.width, lay.bot_h, clip, dur);
            if strip.is_none() {
                log("gameplay: strip unavailable, falling back to webcam band");
            }
            strip
        }
        None => None,
    };

    // Build the mid-roll banner overlay (centered in the clip) if enabled and
    // the clip is long enough to fit it with a little breathing room.
    let banner_dur = config::banner_duration();
    let banner_overlay = if banner && banner_dur > 0.0 && dur > banner_dur + 1.0 {
        let t_start = (dur - banner_dur) / 2.0;
        banner_overlay_asset(lay).map(|path| BannerOverlay {
            path,
            start: t_start,
            end: t_start + banner_dur,
        })
    } else {
        None
    };

    let spec = build_spec(
        lay,
        src_w,
        src_h,
        ass_path,
        panel,
        gameplay_strip.as_deref(),
        dur,
        banner_overlay.as_ref(),
    );

    let mut ictx = format::input(source).map_err(|e| anyhow!("open input: {e}"))?;

    // Seek to the clip start (stream time base).
    let vstream = ictx
        .streams()
        .best(media::Type::Video)
        .ok_or_else(|| anyhow!("no video stream in {}", source.display()))?;
    let vindex = vstream.index();
    let in_time_base = vstream.time_base();
    let in_frame_rate = vstream.avg_frame_rate();
    let start_pts = (start / f64::from(in_time_base)).round() as i64;
    let end_pts = ((start + dur) / f64::from(in_time_base)).round() as i64;
    // seek(ts, ..) with stream_index -1 expects AV_TIME_BASE microseconds.
    let seek_us = (start * 1_000_000.0).round() as i64;
    ictx.seek(seek_us, ..seek_us)
        .map_err(|e| anyhow!("seek: {e}"))?;

    let vstream = ictx.stream(vindex).unwrap();
    let decoder = codec::context::Context::from_parameters(vstream.parameters())
        .map_err(|e| anyhow!("video decoder ctx: {e}"))?
        .decoder()
        .video()
        .map_err(|e| anyhow!("video decoder: {e}"))?;

    let mut octx = format::output(out_path).map_err(|e| anyhow!("open output: {e}"))?;
    let global_header = octx
        .format()
        .flags()
        .contains(format::Flags::GLOBAL_HEADER);

    let venc_time_base = Rational(1, config::fps() as i32);
    let (venc, vout_index) = open_video_encoder(&mut octx, lay.width, lay.height, venc_time_base, global_header)?;

    // --- Build the video filter graph ---
    let mut graph = ffmpeg::filter::Graph::new();
    let args = format!(
        "video_size={src_w}x{src_h}:pix_fmt={}:time_base={}:frame_rate={}",
        decoder
            .format()
            .descriptor()
            .map(|d| d.name())
            .unwrap_or("yuv420p"),
        in_time_base,
        in_frame_rate
    );
    graph
        .add(&ffmpeg::filter::find("buffer").ok_or_else(|| anyhow!("no buffer filter"))?, "in", &args)
        .map_err(|e| anyhow!("add buffer: {e}"))?;
    graph
        .add(&ffmpeg::filter::find("buffersink").ok_or_else(|| anyhow!("no buffersink"))?, "out", "")
        .map_err(|e| anyhow!("add buffersink: {e}"))?;
    graph
        .get("out")
        .unwrap()
        .set_pixel_format(Pixel::YUV420P);
    graph
        .output("in", 0)
        .map_err(|e| anyhow!("graph output: {e}"))?
        .input("out", 0)
        .map_err(|e| anyhow!("graph input: {e}"))?
        .parse(&spec)
        .map_err(|e| anyhow!("parse filtergraph: {e}\n{spec}"))?;
    graph.validate().map_err(|e| anyhow!("validate graph: {e}"))?;

    let mut vpipe = VideoPipeline {
        filter: graph,
        decoder,
        encoder: venc,
        stream_index: vout_index,
        in_time_base,
        out_time_base: venc_time_base,
        start_pts,
        end_pts,
        fps: config::fps() as f64,
        frame_count: -1,
        pending_pts: std::collections::VecDeque::new(),
    };

    // --- Audio (optional, passthrough re-encode to AAC) ---
    let audio_plan = if has_audio(source) {
        setup_audio(&mut octx, &mut ictx, global_header, start, dur)
    } else {
        None
    };
    let mut audio = audio_plan;

    octx.write_header().map_err(|e| anyhow!("write header: {e}"))?;

    // Demux loop.
    let mut v_done = false;
    let packets = ictx.packets();
    for (stream, packet) in packets {
        let idx = stream.index();
        if idx == vindex {
            vpipe
                .decoder
                .send_packet(&packet)
                .map_err(|e| anyhow!("video decode send: {e}"))?;
            vpipe.receive_decoded(&mut octx)?;
        } else if let Some(a) = audio.as_mut() {
            if idx == a.stream_index_in {
                a.decoder
                    .send_packet(&packet)
                    .map_err(|e| anyhow!("audio decode send: {e}"))?;
                a.receive_decoded(&mut octx)?;
            }
        }
        // Stop early once the video stream passed the clip end. Rescale this
        // packet's pts into the video stream time_base (packets arrive with
        // per-stream time_bases).
        if vpipe.end_pts > 0 {
            if let Some(p) = packet.pts() {
                let pv = p.rescale(stream.time_base(), in_time_base);
                if pv > end_pts + 1000 {
                    v_done = true;
                    break;
                }
            }
        }
    }
    let _ = v_done;

    // Flush video.
    vpipe.decoder.send_eof().ok();
    vpipe.receive_decoded(&mut octx)?;
    vpipe.flush_filter();
    vpipe.drain_filtered(&mut octx)?;
    vpipe.encoder.send_eof().ok();
    vpipe.drain_encoded(&mut octx)?;

    if let Some(mut a) = audio {
        a.finish(&mut octx)?;
    }

    octx.write_trailer().map_err(|e| anyhow!("write trailer: {e}"))?;
    log(format!("rendered -> {}", out_path.display()));
    Ok(out_path.to_path_buf())
}

// --- Audio pipeline ----------------------------------------------------------
struct AudioPipeline {
    stream_index_in: usize,
    stream_index_out: usize,
    decoder: codec::decoder::Audio,
    encoder: codec::encoder::Audio,
    filter: ffmpeg::filter::Graph,
    in_time_base: Rational,
    out_time_base: Rational,
    start: f64,
    end: f64,
    rate: u32,
    sample_count: i64,
}

/// Set up audio: reuse the already-seeked demuxer, decode audio, resample & buffer
/// to fixed 1024 sample chunks (required by AAC), and encode to AAC.
fn setup_audio(
    octx: &mut format::context::Output,
    ictx: &mut format::context::Input,
    global_header: bool,
    start: f64,
    dur: f64,
) -> Option<AudioPipeline> {
    let astream = ictx.streams().best(media::Type::Audio)?;
    let index = astream.index();
    let in_time_base = astream.time_base();
    let decoder = codec::context::Context::from_parameters(astream.parameters())
        .ok()?
        .decoder()
        .audio()
        .ok()?;

    let rate = decoder.rate();
    let layout = if decoder.channel_layout().is_empty() {
        ChannelLayout::STEREO
    } else {
        decoder.channel_layout()
    };
    let sample_fmt = ffmpeg::format::Sample::F32(ffmpeg::format::sample::Type::Planar);

    let (encoder, out_index) = open_audio_encoder(octx, rate, layout, sample_fmt, global_header).ok()?;
    let out_time_base = Rational(1, rate as i32);

    let mut filter = ffmpeg::filter::Graph::new();
    let args = format!(
        "time_base={}:sample_rate={}:sample_fmt={}:channel_layout=0x{:x}",
        in_time_base,
        rate,
        decoder.format().name(),
        layout.bits()
    );
    filter.add(&ffmpeg::filter::find("abuffer")?, "in", &args).ok()?;
    filter.add(&ffmpeg::filter::find("abuffersink")?, "out", "").ok()?;
    {
        let mut out = filter.get("out")?;
        out.set_sample_format(sample_fmt);
        out.set_channel_layout(layout);
        out.set_sample_rate(rate);
    }
    filter.output("in", 0).ok()?.input("out", 0).ok()?.parse("aresample,asetnsamples=n=1024:p=1").ok()?;
    filter.validate().ok()?;

    Some(AudioPipeline {
        stream_index_in: index,
        stream_index_out: out_index,
        decoder,
        encoder,
        filter,
        in_time_base,
        out_time_base,
        start,
        end: start + dur,
        rate,
        sample_count: 0,
    })
}

impl AudioPipeline {
    fn receive_decoded(&mut self, octx: &mut format::context::Output) -> Result<()> {
        let mut decoded = frame::Audio::empty();
        while self.decoder.receive_frame(&mut decoded).is_ok() {
            let t_sec = match decoded.timestamp() {
                Some(ts) => ts as f64 * f64::from(self.in_time_base),
                None => continue,
            };
            let frame_dur = decoded.samples() as f64 / self.rate as f64;
            if t_sec + frame_dur < self.start {
                continue;
            }
            if t_sec > self.end {
                break;
            }
            self.filter
                .get("in")
                .unwrap()
                .source()
                .add(&decoded)
                .map_err(|e| anyhow!("audio filter add: {e}"))?;
            self.drain_filtered(octx)?;
        }
        Ok(())
    }

    fn drain_filtered(&mut self, octx: &mut format::context::Output) -> Result<()> {
        let mut filtered = frame::Audio::empty();
        while self
            .filter
            .get("out")
            .unwrap()
            .sink()
            .frame(&mut filtered)
            .is_ok()
        {
            filtered.set_pts(Some(self.sample_count));
            self.sample_count += filtered.samples() as i64;
            self.encoder
                .send_frame(&filtered)
                .map_err(|e| anyhow!("audio encoder send: {e}"))?;
            self.drain_encoded(octx)?;
        }
        Ok(())
    }

    fn drain_encoded(&mut self, octx: &mut format::context::Output) -> Result<()> {
        let mut pkt = ffmpeg::Packet::empty();
        while self.encoder.receive_packet(&mut pkt).is_ok() {
            pkt.set_stream(self.stream_index_out);
            pkt.rescale_ts(
                self.out_time_base,
                octx.stream(self.stream_index_out).unwrap().time_base(),
            );
            pkt.write_interleaved(octx)
                .map_err(|e| anyhow!("write audio packet: {e}"))?;
        }
        Ok(())
    }

    fn finish(&mut self, octx: &mut format::context::Output) -> Result<()> {
        self.decoder.send_eof().ok();
        self.receive_decoded(octx)?;
        self.filter.get("in").unwrap().source().flush().ok();
        self.drain_filtered(octx)?;
        self.encoder.send_eof().ok();
        self.drain_encoded(octx)?;
        Ok(())
    }
}
