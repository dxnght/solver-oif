use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;
use tokio::fs::{self, File, OpenOptions};
use tokio::io::{AsyncWriteExt, BufWriter};
use tracing::info;

/// Append-only JSONL writer. One line per serialized record (a
/// `NormalizedIntent` from ingest, or a live `DecisionRecord`).
///
/// Chosen over Parquet/SQLite because:
///   - dead-simple to inspect with `jq` / `head`
///   - append-only matches the streaming nature of ingestion
///   - 20-70k records (Phase 0 target) fit in memory at backtest time
///
/// Rotation: a fresh file per UTC day per source — caller passes the path.
pub struct JsonlSink {
    path: PathBuf,
    writer: BufWriter<File>,
    written: usize,
}

impl JsonlSink {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await.with_context(|| {
                format!("creating parent dir for sink {}", path.display())
            })?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .with_context(|| format!("opening sink {}", path.display()))?;
        Ok(Self {
            path,
            writer: BufWriter::with_capacity(64 * 1024, file),
            written: 0,
        })
    }

    /// Append one record as a JSON line. Generic over `T: Serialize` so the same
    /// sink serves historical intents and the live decisions log (LIV-4).
    pub async fn write<T: Serialize>(&mut self, record: &T) -> Result<()> {
        let mut line = serde_json::to_vec(record).context("serializing record")?;
        line.push(b'\n');
        self.writer.write_all(&line).await.context("writing record line")?;
        self.written += 1;
        Ok(())
    }

    /// Flush and log at INFO — use at end of a batch ingest run.
    pub async fn flush(&mut self) -> Result<()> {
        self.writer.flush().await.context("flushing sink")?;
        info!(path = %self.path.display(), written = self.written, "flushed sink");
        Ok(())
    }

    /// Flush the underlying buffer without logging — for live mode where
    /// flush is called after every write and an INFO per intent would be noisy.
    pub async fn flush_silent(&mut self) -> Result<()> {
        self.writer.flush().await.context("flushing sink")
    }

    pub fn count(&self) -> usize {
        self.written
    }
}
