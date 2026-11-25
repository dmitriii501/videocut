//! Download stream sources (YouTube/Twitch) via an external `yt-dlp` binary.
//!
//! We shell out to yt-dlp rather than reimplement its site logic. The binary is
//! resolved from `STREAMREELS_YTDLP` or PATH.

use anyhow::{bail, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config;
use crate::utils::{duration, ensure_dir, log, retry, run_out, short_hash, video_size};

fn ytdlp_bin() -> String {
    std::env::var("STREAMREELS_YTDLP").unwrap_or_else(|_| "yt-dlp".to_string())
}

fn common_args() -> Vec<String> {
    let mut args: Vec<String> = vec![
        "--no-playlist",
        "--no-progress",
        "--retries",
        "10",
        "--fragment-retries",
        "10",
        "--socket-timeout",
        "30",
        "--merge-output-format",
        "mp4",
        "--remux-video",
        "mp4",
    ]
    .into_iter()
    .map(String::from)
    .collect();

    if let Ok(cookies) = std::env::var("STREAMREELS_COOKIES") {
        if !cookies.is_empty() {
            args.push("--cookies".into());
            args.push(cookies);
        }
    }
    if let Ok(browser) = std::env::var("STREAMREELS_COOKIES_FROM_BROWSER") {
        if !browser.is_empty() {
            args.push("--cookies-from-browser".into());
            args.push(browser);
        }
    }
    if let Ok(js) = std::env::var("STREAMREELS_JS_RUNTIME") {
        if !js.is_empty() {
            args.push("--js-runtimes".into());
            args.push(js);
        }
    }
    if std::env::var("STREAMREELS_REMOTE_COMPONENTS").is_ok() {
        args.push("--remote-components".into());
        args.push("ejs:github".into());
    }
    args
}

fn download(url: &str, outdir: &Path, max_height: u32) -> Result<PathBuf> {
    ensure_dir(outdir)?;

    let attempt = || -> Result<PathBuf> {
        let before: std::collections::HashSet<PathBuf> = std::fs::read_dir(outdir)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().map(|x| x == "mp4").unwrap_or(false))
            .collect();

        let mut cmd = Command::new(ytdlp_bin());
        cmd.arg("-S")
            .arg(format!("res:{max_height},ext:mp4:m4a"))
            .arg("--format-sort-force")
            .args(common_args())
            .arg("--output")
            .arg(outdir.join("%(title).80s-%(id)s.%(ext)s"))
            .arg("--print")
            .arg("after_move:filepath")
            .arg(url);

        let out = run_out(&mut cmd)?;
        if let Some(line) = out
            .lines()
            .map(str::trim)
            .rfind(|l| l.ends_with(".mp4"))
        {
            return Ok(PathBuf::from(line));
        }
        // Fall back to whatever new mp4 appeared, else newest overall.
        let mut after: Vec<PathBuf> = std::fs::read_dir(outdir)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().map(|x| x == "mp4").unwrap_or(false))
            .filter(|p| !before.contains(p))
            .collect();
        after.sort_by_key(|p| p.metadata().and_then(|m| m.modified()).ok());
        if let Some(p) = after.pop() {
            return Ok(p);
        }
        let mut all: Vec<PathBuf> = std::fs::read_dir(outdir)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().map(|x| x == "mp4").unwrap_or(false))
            .collect();
        all.sort_by_key(|p| p.metadata().and_then(|m| m.modified()).ok());
        all.pop()
            .ok_or_else(|| anyhow::anyhow!("yt-dlp produced no mp4 for {url:?}"))
    };

    match retry(attempt, 5, 3.0, &format!("download {}", &url[..url.len().min(50)])) {
        Ok(p) => Ok(p),
        Err(exc) => {
            let msg = exc.to_string();
            if msg.contains("not a bot") || msg.contains("403") || msg.contains("Sign in") {
                bail!(
                    "YouTube blocked the download (bot-check / 403).\n  \
                     Set STREAMREELS_COOKIES=/path/cookies.txt or \
                     STREAMREELS_COOKIES_FROM_BROWSER=firefox, or pass a local file."
                );
            }
            Err(exc)
        }
    }
}

/// Download (or reuse a cached copy of) the source VOD.
pub fn fetch_source(url: &str, _work: &Path) -> Result<PathBuf> {
    let key = short_hash(url);
    let outdir = ensure_dir(&config::cache().join("sources"))?;
    if let Ok(entries) = std::fs::read_dir(&outdir) {
        for entry in entries.flatten() {
            let p = entry.path();
            let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if name.contains(&key) && p.stat_size() > 0 {
                log(format!("reusing cached source: {}", p.display()));
                return Ok(p);
            }
        }
    }
    log(format!("downloading source: {url}"));
    let path = download(url, &outdir, config::max_source_height())?;
    let dur = duration(&path);
    let size = video_size(&path).map(|(w, h)| format!("{w}x{h}")).unwrap_or_default();
    log(format!("source ready: {} ({dur:.0}s, {size})", path.display()));
    Ok(path)
}

/// Small helper so we can call `.stat_size()` on a Path.
trait PathSize {
    fn stat_size(&self) -> u64;
}

impl PathSize for PathBuf {
    fn stat_size(&self) -> u64 {
        self.metadata().map(|m| m.len()).unwrap_or(0)
    }
}
