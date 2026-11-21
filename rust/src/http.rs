//! Shared blocking HTTP helpers with retry, built on reqwest.
//!
//! Used by webcam (TypeSafe), translate (MyMemory), describe (LLM) and
//! telegram. Keeps one place for timeouts, headers and error shaping.

use anyhow::{anyhow, Context, Result};
use std::time::Duration;

use crate::utils::{log, retry};

fn client(timeout_secs: u64) -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .connect_timeout(Duration::from_secs(20))
        .user_agent("streamreels/0.1")
        .build()
        .context("build http client")
}

/// POST a JSON body, return parsed JSON. Retries on transport errors and 5xx.
pub fn post_json(
    url: &str,
    headers: &[(&str, &str)],
    body: &serde_json::Value,
    timeout_secs: u64,
) -> Result<serde_json::Value> {
    let attempt = || -> Result<serde_json::Value> {
        let cl = client(timeout_secs)?;
        let mut req = cl.post(url).json(body);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let resp = req.send().context("http POST")?;
        let status = resp.status();
        let text = resp.text().context("read response body")?;
        if !status.is_success() {
            return Err(anyhow!("HTTP {status}: {}", truncate(&text, 300)));
        }
        serde_json::from_str(&text).with_context(|| format!("parse JSON: {}", truncate(&text, 200)))
    };
    retry(attempt, 5, 2.0, &format!("POST {url}"))
}

/// GET a URL, return the body as text. Retries on transport errors and 5xx.
pub fn get_text(url: &str, headers: &[(&str, &str)], timeout_secs: u64) -> Result<String> {
    let attempt = || -> Result<String> {
        let cl = client(timeout_secs)?;
        let mut req = cl.get(url);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let resp = req.send().context("http GET")?;
        let status = resp.status();
        let text = resp.text().context("read response body")?;
        if !status.is_success() {
            return Err(anyhow!("HTTP {status}: {}", truncate(&text, 300)));
        }
        Ok(text)
    };
    retry(attempt, 5, 2.0, &format!("GET {url}"))
}

/// GET a URL and parse JSON.
pub fn get_json(url: &str, headers: &[(&str, &str)], timeout_secs: u64) -> Result<serde_json::Value> {
    let text = get_text(url, headers, timeout_secs)?;
    serde_json::from_str(&text).with_context(|| format!("parse JSON: {}", truncate(&text, 200)))
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let t: String = s.chars().take(n).collect();
        log(format!("… (truncated {})", s.len()));
        format!("{t}…")
    }
}
