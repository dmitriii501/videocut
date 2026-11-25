//! Anonymous Twitch IRC chat monitor with activity-spike detection.
//!
//! We join the channel anonymously (Twitch allows a read-only connection with
//! the `justinfan` nickname and no OAuth), count messages in short windows, and
//! emit an event when the recent rate spikes well above the recent baseline.
//! Chat usually reacts *before* the funny moment is over, which is why the
//! caller adds a pre-roll when cutting the clip.
//!
//! No external deps: plain sockets.

use anyhow::{Context, Result};
use regex::Regex;
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

type Windows = (Vec<(Instant, f64)>, Vec<(Instant, f64)>);

use crate::utils::log;

const IRC_HOST: &str = "irc.chat.twitch.tv";
const IRC_PORT: u16 = 6667;

/// Cap on continuous evidence fed into the fused hype score (mirror of live.rs).
const MAX_EVIDENCE: f64 = 4.0;

/// Emotes that signal a genuinely funny / hyped moment.
const HYPE_EMOTES: &[&str] = &[
    "kekw", "kekwa", "lul", "lulw", "omegalul", "omegalulw", "pog", "pogchamp", "pogu", "poggers",
    "poggg", "kappa", "monkas", "pepelaugh", "pepehands", "pepega", "icant", "ez", "clap", "4head",
    "hahaa", "sadge", "biblethump", "catjam", "weirdchamp", "jebaited", "residentsleeper", "hypers",
    "lmaooo", "xdd", "lulul",
];

fn laugh_regex() -> &'static Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)(?:ха){2,}|(?:ах){2,}|(?:h[ae]){2,}|lol|lmao|kek|x[dд]{1,}|:d\b")
            .expect("valid laugh regex")
    })
}

/// Weight of a single chat message as a hype signal (>=1.0).
///
/// Plain message = 1.0.  Each hype emote adds +0.5 (capped), laughter +1.0,
/// all-caps shouting +0.5.  Total capped at 4.0.
fn weight(text: &str) -> f64 {
    if text.is_empty() {
        return 1.0;
    }
    let mut w = 1.0f64;
    let mut hype = 0;
    for tok in text.split_whitespace() {
        let clean = tok
            .trim_matches(|c: char| "!?.,;:()[]\"'".contains(c))
            .to_lowercase();
        if HYPE_EMOTES.contains(&clean.as_str()) {
            hype += 1;
        }
    }
    if hype > 0 {
        w += hype.min(4) as f64 * 0.5;
    }
    if laugh_regex().is_match(text) {
        w += 1.0;
    }
    let letters: Vec<char> = text.chars().filter(|c| c.is_alphabetic()).collect();
    if letters.len() >= 4 && letters.iter().all(|c| c.is_uppercase()) {
        w += 0.5;
    }
    w.min(4.0)
}

struct State {
    weighted: VecDeque<(Instant, f64)>,
}

struct Shared {
    state: Mutex<State>,
    stop: AtomicBool,
}

/// Read-only Twitch IRC client that detects chat activity spikes.
pub struct ChatMonitor {
    channel: String,
    spike_ratio: f64,
    min_rate: f64,
    short_secs: f64,
    baseline_secs: f64,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl ChatMonitor {
    pub fn new(
        channel: &str,
        spike_ratio: f64,
        min_rate: f64,
        short_secs: f64,
        baseline_secs: f64,
    ) -> Self {
        ChatMonitor {
            channel: channel.to_lowercase().trim_start_matches('#').to_string(),
            spike_ratio,
            min_rate,
            short_secs,
            baseline_secs,
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    weighted: VecDeque::with_capacity(20000),
                }),
                stop: AtomicBool::new(false),
            }),
            thread: None,
        }
    }

    pub fn start(&mut self) {
        let shared = self.shared.clone();
        let channel = self.channel.clone();
        self.thread = Some(thread::spawn(move || run(shared, channel)));
    }

    pub fn stop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
        log("chat: stopped");
    }

    fn windows(&self, now: Instant) -> Windows {
        let state = self.shared.state.lock().unwrap();
        let short = Duration::from_secs_f64(self.short_secs);
        let baseline = Duration::from_secs_f64(self.baseline_secs);
        let recent: Vec<(Instant, f64)> = state
            .weighted
            .iter()
            .filter(|(t, _)| now.duration_since(*t) <= short)
            .cloned()
            .collect();
        let base: Vec<(Instant, f64)> = state
            .weighted
            .iter()
            .filter(|(t, _)| {
                let age = now.duration_since(*t);
                age >= short && age <= baseline
            })
            .cloned()
            .collect();
        (recent, base)
    }

    /// Continuous chat evidence normalised to the spike threshold.
    ///
    /// Returns `(evidence, rate, baseline)`.  Evidence measures the excess over
    /// the baseline, scaled so 0.0 == calm chatter, 1.0 == exactly the hard
    /// spike level, >1 hotter.  Pure read: no cooldown side effects.
    pub fn evidence(&self) -> (f64, f64, f64) {
        let now = Instant::now();
        let (recent, base) = self.windows(now);
        let rate = recent.iter().map(|(_, w)| w).sum::<f64>() / self.short_secs;
        let baseline = if base.is_empty() {
            0.0
        } else {
            base.iter().map(|(_, w)| w).sum::<f64>() / (self.baseline_secs - self.short_secs)
        };
        let trigger = self.min_rate.max(baseline * self.spike_ratio);
        let span = trigger - baseline;
        let evidence = if span > 0.0 {
            (rate - baseline) / span
        } else if trigger > 0.0 {
            rate / trigger
        } else {
            0.0
        };
        (evidence.clamp(0.0, MAX_EVIDENCE), rate, baseline)
    }
}

fn connect(shared: &Arc<Shared>, channel: &str) -> Result<TcpStream> {
    let mut stream = TcpStream::connect((IRC_HOST, IRC_PORT)).context("connect IRC")?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let nick = format!("justinfan{}", 10000 + (nanos % 90000));
    stream.set_read_timeout(Some(Duration::from_secs(60)))?;
    stream.write_all(b"PASS SCHMOOPIIE\r\n")?;
    stream.write_all(format!("NICK {nick}\r\n").as_bytes())?;
    stream.write_all(b"CAP REQ :twitch.tv/tags twitch.tv/commands\r\n")?;
    stream.write_all(format!("JOIN #{channel}\r\n").as_bytes())?;
    log(format!("chat: joined #{channel} as {nick}"));
    let _ = shared;
    Ok(stream)
}

fn handle_line(shared: &Arc<Shared>, line: &str) -> Option<String> {
    if line.starts_with("PING") {
        return Some("PONG :tmi.twitch.tv\r\n".to_string());
    }
    if let Some(pos) = line.find(" PRIVMSG ") {
        let rest = &line[pos + " PRIVMSG ".len()..];
        let text = rest.split_once(" :").map(|(_, t)| t).unwrap_or("").to_string();
        let now = Instant::now();
        let mut state = shared.state.lock().unwrap();
        state.weighted.push_back((now, weight(&text)));
        if state.weighted.len() > 20000 {
            state.weighted.pop_front();
        }
    }
    None
}

fn run(shared: Arc<Shared>, channel: String) {
    let mut backoff = 2.0f64;
    while !shared.stop.load(Ordering::SeqCst) {
        let stream = match connect(&shared, &channel) {
            Ok(s) => s,
            Err(exc) => {
                if shared.stop.load(Ordering::SeqCst) {
                    break;
                }
                log(format!(
                    "chat: connection error ({exc}); reconnecting in {backoff:.0}s"
                ));
                thread::sleep(Duration::from_secs_f64(backoff));
                backoff = (backoff * 2.0).min(30.0);
                continue;
            }
        };
        backoff = 2.0;
        let reader_stream = match stream.try_clone() {
            Ok(s) => s,
            Err(_) => break,
        };
        let mut writer = stream;
        let reader = BufReader::new(reader_stream);
        let mut buf = String::new();
        // We drive reads line by line; a read timeout surfaces as an Err and we
        // send a keepalive PING.
        let mut lines = reader.lines();
        loop {
            if shared.stop.load(Ordering::SeqCst) {
                break;
            }
            match lines.next() {
                Some(Ok(line)) => {
                    if let Some(reply) = handle_line(&shared, &line) {
                        let _ = writer.write_all(reply.as_bytes());
                    }
                }
                Some(Err(_)) => {
                    // Timeout: keep the connection alive.
                    if writer.write_all(b"PING :tmi.twitch.tv\r\n").is_err() {
                        break;
                    }
                }
                None => break,
            }
        }
        buf.clear();
        if shared.stop.load(Ordering::SeqCst) {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weight_plain_is_one() {
        assert!((weight("hello there") - 1.0).abs() < 1e-9);
    }

    #[test]
    fn weight_hype_emote_and_laugh() {
        assert!(weight("KEKW") > 1.0);
        assert!(weight("hahaha") > 1.0);
        assert!(weight("LETS GOOOOO") > 1.0);
    }

    #[test]
    fn evidence_is_zero_without_messages() {
        let m = ChatMonitor::new("chan", 2.5, 1.5, 5.0, 60.0);
        let (ev, rate, base) = m.evidence();
        assert_eq!(ev, 0.0);
        assert_eq!(rate, 0.0);
        assert_eq!(base, 0.0);
    }
}
