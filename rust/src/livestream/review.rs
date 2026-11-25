//! Review log: persist every detected hype moment for manual quality scoring.
//!
//! When `--review-dir` is set the live loop saves the raw clip for *every*
//! detection — both the ones the moment filter keeps and the ones it drops —
//! plus one row per detection in `index.csv`.  You open the clips, label the
//! `label` column (`hit` / `miss`), and `tools/score_review.py` turns that into
//! a precision number plus tuning hints.
//!
//! Saving dropped clips too is deliberate: it lets you spot false *negatives*
//! of the moment filter (a clip it threw away that was actually a good moment)
//! and false positives of detection, not just what survived.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};

use crate::utils::{ensure_dir, log};

/// Fixed column order; tools/score_review.py and your spreadsheet rely on it.
pub const COLUMNS: &[&str] = &[
    "idx",
    "time",
    "channel",
    "content_center",
    "score",
    "audio_ev",
    "chat_ev",
    "audio_peak",
    "audio_base",
    "audio_sigma",
    "chat_rate",
    "chat_base",
    "filter_keep",
    "filter_reason",
    "mean_conf",
    "low_conf_frac",
    "garbage_frac",
    "clip",
    "label",
];

/// Append-only CSV + per-clip sidecars under `root`.
pub struct ReviewLog {
    root: PathBuf,
    index: PathBuf,
}

impl ReviewLog {
    /// The directory this log writes clips and `index.csv` into, for display.
    pub fn location(&self) -> String {
        self.root.display().to_string()
    }

    pub fn new(root: &Path) -> Result<Self> {
        let root = ensure_dir(root)?;
        let index = root.join("index.csv");
        let log = ReviewLog {
            root,
            index,
        };
        if !log.index.exists() {
            let mut wtr = csv::Writer::from_path(&log.index)
                .with_context(|| format!("create {}", log.index.display()))?;
            wtr.write_record(COLUMNS)?;
            wtr.flush()?;
        } else {
            log.migrate_header();
        }
        Ok(log)
    }

    /// Rewrite `index.csv` if its header is missing newer COLUMNS.
    fn migrate_header(&self) {
        if let Err(exc) = self.try_migrate_header() {
            log(format!("review: could not migrate index.csv: {exc}"));
        }
    }

    fn try_migrate_header(&self) -> Result<()> {
        let mut rdr = csv::Reader::from_path(&self.index)?;
        let header: Vec<String> = rdr.headers()?.iter().map(|s| s.to_string()).collect();
        if COLUMNS.iter().all(|c| header.iter().any(|h| h == c)) {
            return Ok(());
        }
        let mut rows: Vec<HashMap<String, String>> = Vec::new();
        for rec in rdr.records() {
            let rec = rec?;
            let mut map = HashMap::new();
            for (h, v) in header.iter().zip(rec.iter()) {
                map.insert(h.clone(), v.to_string());
            }
            rows.push(map);
        }
        drop(rdr);

        let mut wtr = csv::Writer::from_path(&self.index)?;
        wtr.write_record(COLUMNS)?;
        for r in &rows {
            let record: Vec<String> = COLUMNS
                .iter()
                .map(|c| r.get(*c).cloned().unwrap_or_default())
                .collect();
            wtr.write_record(&record)?;
        }
        wtr.flush()?;
        log(format!(
            "review: migrated index.csv to {} columns",
            COLUMNS.len()
        ));
        Ok(())
    }

    /// Copy `clip` into the review dir and append one row to `index.csv`.
    ///
    /// `row` is a plain map; missing columns are written empty.  Any error is
    /// swallowed and logged — review bookkeeping must never break the live
    /// loop.
    pub fn record(&self, clip: &Path, row: &HashMap<String, String>) {
        let idx: i64 = row
            .get("idx")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let channel = row
            .get("channel")
            .cloned()
            .unwrap_or_else(|| "stream".to_string());
        let name = format!("{channel}-{idx:03}.mp4");
        let dest = self.root.join(&name);
        if let Err(exc) = fs::copy(clip, &dest) {
            log(format!("review: could not save clip {name}: {exc}"));
            return;
        }

        let mut row = row.clone();
        row.insert("clip".into(), name.clone());
        row.entry("label".into()).or_default();

        match self.append_row(&row) {
            Ok(()) => {}
            Err(exc) => {
                log(format!("review: could not write index.csv: {exc}"));
                return;
            }
        }
        Self::write_sidecar(&dest.with_extension("txt"), &row);
    }

    fn append_row(&self, row: &HashMap<String, String>) -> Result<()> {
        let mut wtr = csv::WriterBuilder::new()
            .has_headers(false)
            .from_writer(OpenOptions::new().append(true).open(&self.index)?);
        let record: Vec<String> = COLUMNS
            .iter()
            .map(|c| row.get(*c).cloned().unwrap_or_default())
            .collect();
        wtr.write_record(&record)?;
        wtr.flush()?;
        Ok(())
    }

    fn write_sidecar(path: &Path, row: &HashMap<String, String>) {
        let get = |k: &str| row.get(k).map(|s| s.as_str()).unwrap_or("");
        let text = format!(
            "clip:    {}\nchannel: {}\ntime:    {}\nscore:   {} (audio {}, chat {})\ncenter:  {}s (content time)\nfilter:  keep={} {}\n\nWatch the clip and label it in index.csv:\n  hit  = good moment, keep this reel\n  miss = bad/empty/mistimed, should not be a reel\n",
            get("clip"),
            get("channel"),
            get("time"),
            get("score"),
            get("audio_ev"),
            get("chat_ev"),
            get("content_center"),
            get("filter_keep"),
            get("filter_reason"),
        );
        let _ = fs::write(path, text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_header_and_appends_row() {
        let dir = std::env::temp_dir().join(format!(
            "streamreels-review-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        let log = ReviewLog::new(&dir).unwrap();
        // A fake clip to copy.
        let clip = dir.join("src.mp4");
        fs::write(&clip, b"fakevideo").unwrap();
        let mut row = HashMap::new();
        row.insert("idx".to_string(), "1".to_string());
        row.insert("channel".to_string(), "chan".to_string());
        row.insert("score".to_string(), "1.20".to_string());
        log.record(&clip, &row);

        let content = fs::read_to_string(dir.join("index.csv")).unwrap();
        assert!(content.starts_with("idx,time,channel"));
        assert!(content.contains("chan-001.mp4"));
        assert!(dir.join("chan-001.mp4").exists());
        assert!(dir.join("chan-001.txt").exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
