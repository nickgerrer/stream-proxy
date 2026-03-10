// src/transcode.rs
use bytes::Bytes;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

/// Spawn an FFmpeg transcoding subprocess.
///
/// Reads chunks from the broadcast receiver, writes them to FFmpeg's stdin.
/// Returns the child process (with stdout still available) and the writer task handle.
///
/// The caller reads from `child.stdout` to get transcoded output.
/// `kill_on_drop(true)` ensures FFmpeg is killed when the Child is dropped.
pub fn spawn_ffmpeg(
    rx: broadcast::Receiver<Bytes>,
) -> Result<(Child, JoinHandle<()>), std::io::Error> {
    let mut child = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "warning",
            "-i",
            "pipe:0",
            "-c:v",
            "copy",
            "-bsf:v",
            "dump_extra",
            "-c:a",
            "ac3",
            "-b:a",
            "128k",
            "-f",
            "mpegts",
            "-fflags",
            "+genpts+discardcorrupt",
            "-output_ts_offset",
            "0",
            "pipe:1",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;

    let mut stdin = child.stdin.take().expect("stdin was configured as piped");
    let stderr = child.stderr.take().expect("stderr was configured as piped");

    // Log FFmpeg stderr at debug level
    tokio::spawn(async move {
        let reader = BufReader::new(stderr);
        let mut lines = reader.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            tracing::debug!("ffmpeg: {}", line);
        }
    });

    // Writer task: broadcast receiver -> FFmpeg stdin
    let writer_handle = tokio::spawn(async move {
        let mut rx = rx;
        loop {
            match rx.recv().await {
                Ok(chunk) => {
                    if stdin.write_all(&chunk).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("FFmpeg stdin writer lagged {} messages", n);
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    Ok((child, writer_handle))
}
