//! Command-line interface for streamreels (clap-based, argparse parity).

use std::path::PathBuf;

use anyhow::Result;
use clap::{ArgAction, Parser};

use crate::config;
use crate::utils::{check_tool, log, parse_segment};

#[derive(Parser, Debug)]
#[command(
    name = "streamreels",
    about = "Cut stream VODs into vertical reels with face tracking, subtitles and a distracting gameplay strip."
)]
pub struct Cli {
    /// YouTube or Twitch VOD URL (omit if using --local).
    pub url: Option<String>,

    /// Segment as 'start-end' or 'start+dur', e.g. 12:30-13:05 or 90+30. Repeatable.
    #[arg(short = 's', long = "segment", value_name = "START-END", action = ArgAction::Append)]
    pub segment: Vec<String>,

    /// Auto-detect N highlight moments and cut those instead of -s.
    #[arg(long, value_name = "N", default_value_t = 0)]
    pub auto: usize,

    /// Output directory.
    #[arg(short = 'o', long, default_value_os_t = config::output())]
    pub out: PathBuf,

    /// Use this local video file as the source (skips download).
    #[arg(long)]
    pub local: Option<PathBuf>,

    /// Use this local video instead of auto-downloading (accepted, unused).
    #[arg(long)]
    pub gameplay: Option<String>,

    /// YouTube search query for gameplay (repeatable, accepted, unused).
    #[arg(long = "gameplay-query", action = ArgAction::Append)]
    pub gameplay_query: Vec<String>,

    /// Stream crop zoom (accepted, unused; render uses a fixed layout).
    #[arg(long, default_value_t = 1.15)]
    pub zoom: f64,

    /// x264 CRF (sets REEL_CRF for this run).
    #[arg(long)]
    pub crf: Option<u32>,

    /// x264 preset (sets REEL_PRESET for this run).
    #[arg(long)]
    pub preset: Option<String>,

    /// Disable subtitles.
    #[arg(long = "no-subs")]
    pub no_subs: bool,

    /// Translate subtitles into this language, e.g. ru (uses MyMemory, cached).
    #[arg(long = "subtitle-lang", value_name = "LANG")]
    pub subtitle_lang: Option<String>,

    /// Send each finished reel to Telegram.
    #[arg(long)]
    pub telegram: bool,

    /// Telegram chat_id to send to (else auto-discover / env).
    #[arg(long = "tg-chat", value_name = "ID")]
    pub tg_chat: Option<i64>,

    /// Manual webcam panel box in relative 0..1 coords, e.g. 0.7,0.0,1.0,1.0.
    #[arg(long, value_name = "X0,Y0,X1,Y1")]
    pub webcam: Option<String>,

    /// TypeSafe API key for webcam auto-detection (or set TYPESAFE_API_KEY).
    #[arg(long = "typesafe-key", value_name = "KEY")]
    pub typesafe_key: Option<String>,

    /// Skip webcam auto-detection, use the default panel.
    #[arg(long = "no-webcam-detect")]
    pub no_webcam_detect: bool,

    /// Run the Telegram bot.
    #[arg(long)]
    pub bot: bool,

    // ----- live mode -----
    /// Watch a live Twitch stream, detect hype and cut reels in real time.
    #[arg(long, value_name = "TWITCH_URL")]
    pub live: Option<String>,

    #[arg(long = "pre-roll", value_name = "SEC", default_value_t = 12.0)]
    pub pre_roll: f64,

    #[arg(long = "post-roll", value_name = "SEC", default_value_t = 28.0)]
    pub post_roll: f64,

    #[arg(long = "buffer-secs", value_name = "SEC", default_value_t = 120.0)]
    pub buffer_secs: f64,

    #[arg(long = "chat-threshold", value_name = "RATIO", default_value_t = 2.5)]
    pub chat_threshold: f64,

    #[arg(long = "no-audio")]
    pub no_audio: bool,

    #[arg(long = "audio-fixed")]
    pub audio_fixed: bool,

    #[arg(long = "audio-k", value_name = "K", default_value_t = 3.0)]
    pub audio_k: f64,

    #[arg(long = "audio-ratio", value_name = "RATIO", default_value_t = 1.6)]
    pub audio_ratio: f64,

    #[arg(long = "audio-weight", value_name = "W", default_value_t = 1.0)]
    pub audio_weight: f64,

    #[arg(long = "chat-weight", value_name = "W", default_value_t = 0.4)]
    pub chat_weight: f64,

    #[arg(long = "score-threshold", value_name = "S", default_value_t = 0.7)]
    pub score_threshold: f64,

    #[arg(long = "max-reels", value_name = "N", default_value_t = 0)]
    pub max_reels: usize,

    #[arg(long = "no-moment-filter")]
    pub no_moment_filter: bool,

    #[arg(long = "min-words-after", value_name = "N", default_value_t = 3)]
    pub min_words_after: usize,

    #[arg(long = "min-speech-ratio", value_name = "R", default_value_t = 0.03)]
    pub min_speech_ratio: f64,

    #[arg(long = "min-mean-conf", value_name = "C", default_value_t = 0.55)]
    pub min_mean_conf: f64,

    #[arg(long = "max-low-conf-frac", value_name = "R", default_value_t = 0.35)]
    pub max_low_conf_frac: f64,

    #[arg(long = "review-dir", value_name = "DIR")]
    pub review_dir: Option<PathBuf>,

    // ----- download / anti-bot -----
    #[arg(long)]
    pub cookies: Option<String>,

    #[arg(long = "cookies-from-browser")]
    pub cookies_from_browser: Option<String>,

    #[arg(long = "js-runtime")]
    pub js_runtime: Option<String>,

    #[arg(long = "remote-components")]
    pub remote_components: bool,
}

impl Cli {
    /// Push download/anti-bot options into the environment for `fetch`.
    fn apply_env(&self) {
        if let Some(c) = &self.cookies {
            std::env::set_var("STREAMREELS_COOKIES", c);
        }
        if let Some(c) = &self.cookies_from_browser {
            std::env::set_var("STREAMREELS_COOKIES_FROM_BROWSER", c);
        }
        if let Some(c) = &self.js_runtime {
            std::env::set_var("STREAMREELS_JS_RUNTIME", c);
        }
        if self.remote_components {
            std::env::set_var("STREAMREELS_REMOTE_COMPONENTS", "1");
        }
        if let Some(crf) = self.crf {
            std::env::set_var("REEL_CRF", crf.to_string());
        }
        if let Some(preset) = &self.preset {
            std::env::set_var("REEL_PRESET", preset);
        }
        if let Some(key) = &self.typesafe_key {
            std::env::set_var("TYPESAFE_API_KEY", key);
        }
        if let Some(chat) = self.tg_chat {
            std::env::set_var("STREAMREELS_TG_CHAT_ID", chat.to_string());
        }
    }

    fn webcam_spec(&self) -> Option<String> {
        if self.webcam.is_some() {
            return self.webcam.clone();
        }
        if self.no_webcam_detect {
            let (a, b, c, d) = crate::webcam::DEFAULT_PANEL;
            return Some(format!("{a:.3},{b:.3},{c:.3},{d:.3}"));
        }
        None
    }
}

/// Parse args and dispatch. Returns a process exit code.
pub fn main() -> i32 {
    let cli = Cli::parse();
    cli.apply_env();
    if let Err(exc) = check_tool("ffmpeg") {
        log(format!("FAILED: {exc}"));
        return 1;
    }
    match run(cli) {
        Ok(code) => code,
        Err(exc) => {
            log(format!("FAILED: {exc}"));
            1
        }
    }
}

fn run(cli: Cli) -> Result<i32> {
    let webcam_spec = cli.webcam_spec();

    if cli.bot {
        log("starting Telegram bot (send it a URL)...");
        return Ok(crate::bot::main());
    }

    if let Some(url) = &cli.live {
        use crate::live_pipeline::{run_live, LiveJob};
        let lj = LiveJob {
            url: url.clone(),
            out_dir: cli.out.clone(),
            pre_roll: cli.pre_roll,
            post_roll: cli.post_roll,
            buffer_secs: cli.buffer_secs,
            chat_threshold: cli.chat_threshold,
            max_reels: cli.max_reels,
            use_audio: !cli.no_audio,
            audio_adaptive: !cli.audio_fixed,
            audio_k: cli.audio_k,
            audio_ratio: cli.audio_ratio,
            audio_weight: cli.audio_weight,
            chat_weight: cli.chat_weight,
            score_threshold: cli.score_threshold,
            moment_filter: !cli.no_moment_filter,
            min_words_after: cli.min_words_after,
            min_speech_ratio: cli.min_speech_ratio,
            min_mean_conf: cli.min_mean_conf,
            max_low_conf_frac: cli.max_low_conf_frac,
            review_dir: cli.review_dir.clone(),
            no_subs: cli.no_subs,
            subtitle_lang: cli.subtitle_lang.clone(),
            webcam_spec,
            send_telegram: cli.telegram,
            ..Default::default()
        };
        log(format!(
            "live mode: {url} (pre {}s / post {}s, threshold {}x)",
            cli.pre_roll, cli.post_roll, cli.chat_threshold
        ));
        let outs = run_live(&lj)?;
        log(format!("live: done, {} reel(s)", outs.len()));
        for o in outs {
            log(format!("  {}", o.display()));
        }
        return Ok(0);
    }

    if cli.url.is_none() && cli.local.is_none() {
        log("нужен либо URL источника, либо --local <файл>");
        return Ok(2);
    }
    if cli.auto == 0 && cli.segment.is_empty() {
        log("нужен либо -s START-END (можно несколько), либо --auto N");
        return Ok(2);
    }
    let mut segments = Vec::with_capacity(cli.segment.len());
    for s in &cli.segment {
        match parse_segment(s) {
            Ok(seg) => segments.push(seg),
            Err(exc) => {
                log(format!("bad --segment: {exc}"));
                return Ok(2);
            }
        }
    }

    use crate::pipeline::{run_job, ReelJob};
    let source = cli
        .url
        .clone()
        .or_else(|| cli.local.as_ref().map(|p| p.display().to_string()))
        .unwrap_or_default();
    let job = ReelJob {
        source_url: source.clone(),
        segments,
        out_dir: cli.out.clone(),
        no_subs: cli.no_subs,
        local_source: cli.local.clone(),
        auto_count: cli.auto,
        webcam_spec,
        channel: cli.url.clone().unwrap_or_default(),
        send_telegram: cli.telegram,
        words: None,
        subtitle_lang: cli.subtitle_lang.clone(),
    };
    let outs = run_job(&job)?;
    log("produced:");
    for o in outs {
        log(format!("  {}", o.display()));
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_segments_and_defaults() {
        let cli = Cli::try_parse_from([
            "streamreels",
            "https://youtu.be/x",
            "-s",
            "90+30",
            "-s",
            "12:30-13:05",
        ])
        .unwrap();
        assert_eq!(cli.segment.len(), 2);
        assert_eq!(cli.pre_roll, 12.0);
        assert_eq!(cli.score_threshold, 0.7);
    }
}
