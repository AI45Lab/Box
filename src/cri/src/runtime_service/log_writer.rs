//! CRI container log writer.
//!
//! Buffers workload stdout/stderr into line-oriented CRI log records
//! (`<timestamp> <stream> F <line>`) for [`super::supervisor`].

use tokio::io::AsyncWriteExt;

/// Cap on a per-stream partial-line buffer. A workload that never emits a `\n`
/// would otherwise grow it without bound (a per-container OOM vector in the
/// shared CRI daemon); past this the buffer is flushed as a forced partial.
const MAX_PARTIAL_BYTES: usize = 64 * 1024;

pub(super) struct CriLogWriter {
    file: tokio::fs::File,
    path: String,
    stdout_partial: Vec<u8>,
    stderr_partial: Vec<u8>,
}

impl CriLogWriter {
    pub(super) async fn open(log_path: &str) -> std::io::Result<Option<Self>> {
        if log_path.is_empty() {
            return Ok(None);
        }

        let path = std::path::Path::new(log_path);
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            tokio::fs::create_dir_all(parent).await?;
        }

        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await?;

        Ok(Some(Self {
            file,
            path: log_path.to_string(),
            stdout_partial: Vec::new(),
            stderr_partial: Vec::new(),
        }))
    }

    /// Reopen the log file at its path (CRI `ReopenContainerLog`). The kubelet
    /// rotates by renaming the current file, then calls this; we flush, drop the
    /// old handle, and open a fresh file at the original path so subsequent
    /// output lands where the kubelet now expects it.
    pub(super) async fn reopen(&mut self) -> std::io::Result<()> {
        self.flush_partials().await?;
        if let Some(parent) = std::path::Path::new(&self.path)
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            tokio::fs::create_dir_all(parent).await?;
        }
        self.file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .await?;
        Ok(())
    }

    pub(super) async fn write_chunk(
        &mut self,
        stream: a3s_box_core::exec::StreamType,
        data: &[u8],
    ) -> std::io::Result<()> {
        let partial = match stream {
            a3s_box_core::exec::StreamType::Stdout => &mut self.stdout_partial,
            a3s_box_core::exec::StreamType::Stderr => &mut self.stderr_partial,
        };

        partial.extend_from_slice(data);
        let mut complete_lines = Vec::new();
        while let Some(newline) = partial.iter().position(|byte| *byte == b'\n') {
            let mut line: Vec<u8> = partial.drain(..=newline).collect();
            line.pop();
            complete_lines.push(line);
        }

        // Bound memory: a workload emitting a huge newline-less blob (a multi-GB
        // single line, a binary dump, a `\r`-only progress bar, or hostile output
        // that never sends `\n`) would grow `partial` without limit in the shared
        // CRI daemon — a per-container OOM vector. Once it exceeds the cap with no
        // newline, flush what we have as a forced partial ('P') record and clear
        // it, so memory stays bounded regardless of output shape.
        let overflow = if partial.len() > MAX_PARTIAL_BYTES {
            Some(std::mem::take(partial))
        } else {
            None
        };

        for line in complete_lines {
            self.write_record(stream, &line, true).await?;
        }
        if let Some(blob) = overflow {
            self.write_record(stream, &blob, false).await?;
        }

        Ok(())
    }

    pub(super) async fn flush_partials(&mut self) -> std::io::Result<()> {
        if !self.stdout_partial.is_empty() {
            let line = std::mem::take(&mut self.stdout_partial);
            self.write_record(a3s_box_core::exec::StreamType::Stdout, &line, true)
                .await?;
        }
        if !self.stderr_partial.is_empty() {
            let line = std::mem::take(&mut self.stderr_partial);
            self.write_record(a3s_box_core::exec::StreamType::Stderr, &line, true)
                .await?;
        }

        self.file.flush().await
    }

    /// Write one CRI log record. `full` selects the `F` (complete line) vs `P`
    /// (partial line, e.g. a forced flush of an over-cap newline-less buffer) tag.
    async fn write_record(
        &mut self,
        stream: a3s_box_core::exec::StreamType,
        line: &[u8],
        full: bool,
    ) -> std::io::Result<()> {
        let timestamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let stream = match stream {
            a3s_box_core::exec::StreamType::Stdout => "stdout",
            a3s_box_core::exec::StreamType::Stderr => "stderr",
        };

        self.file.write_all(timestamp.as_bytes()).await?;
        self.file.write_all(b" ").await?;
        self.file.write_all(stream.as_bytes()).await?;
        self.file
            .write_all(if full { b" F " } else { b" P " })
            .await?;
        self.file.write_all(line).await?;
        self.file.write_all(b"\n").await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use a3s_box_core::exec::StreamType;

    #[tokio::test]
    async fn write_chunk_caps_unbounded_partial_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.log");
        let mut w = CriLogWriter::open(path.to_str().unwrap())
            .await
            .unwrap()
            .unwrap();

        // 256 KiB with NO newline — a hostile newline-less blob.
        let blob = vec![b'x'; 256 * 1024];
        w.write_chunk(StreamType::Stdout, &blob).await.unwrap();

        // The in-memory partial buffer must stay bounded, not grow to 256 KiB.
        assert!(
            w.stdout_partial.len() <= MAX_PARTIAL_BYTES,
            "partial buffer must be capped, was {} bytes",
            w.stdout_partial.len()
        );
        // The over-cap data is flushed as a partial ('P') record, so it is not lost.
        w.flush_partials().await.unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(
            contents.contains(" stdout P "),
            "over-cap newline-less data must be written as a 'P' record"
        );
    }

    #[tokio::test]
    async fn write_chunk_still_splits_complete_lines_as_full() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.log");
        let mut w = CriLogWriter::open(path.to_str().unwrap())
            .await
            .unwrap()
            .unwrap();

        w.write_chunk(StreamType::Stdout, b"hello\nworld\n")
            .await
            .unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents.matches(" stdout F ").count(), 2);
        assert!(contents.contains("hello") && contents.contains("world"));
        assert!(w.stdout_partial.is_empty());
    }
}
