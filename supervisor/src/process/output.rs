//! Captured child-output routing and reader task cleanup.

use super::{MAX_CAPTURED_LINE_BYTES, SupervisorState};
use phoxal_cli_core::runtime::ProcessKey;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::task::JoinHandle;
use tokio::time::timeout;

pub(crate) fn spawn_output_reader<R>(
    board: SupervisorState,
    id: impl Into<ProcessKey>,
    stream: &'static str,
    reader: R,
) -> JoinHandle<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let id = id.into();
    tokio::spawn(async move {
        let mut reader = reader;
        let mut chunk = [0_u8; 4_096];
        let mut line = Vec::with_capacity(MAX_CAPTURED_LINE_BYTES);
        let mut truncated = false;
        loop {
            match reader.read(&mut chunk).await {
                Ok(0) => {
                    if !line.is_empty() || truncated {
                        route_captured_line(&board, &id, stream, &line, truncated);
                    }
                    break;
                }
                Ok(read) => {
                    for byte in &chunk[..read] {
                        if *byte == b'\n' {
                            if line.last() == Some(&b'\r') {
                                line.pop();
                            }
                            route_captured_line(&board, &id, stream, &line, truncated);
                            line.clear();
                            truncated = false;
                        } else if line.len() < MAX_CAPTURED_LINE_BYTES {
                            line.push(*byte);
                        } else {
                            truncated = true;
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(process = %id, stream, %error, "failed to read captured child output");
                    break;
                }
            }
        }
    })
}

pub(crate) fn route_captured_line(
    board: &SupervisorState,
    id: &ProcessKey,
    stream: &str,
    bytes: &[u8],
    truncated: bool,
) {
    let mut text = String::from_utf8_lossy(bytes).into_owned();
    if truncated {
        text.push('…');
    }
    if stream == "stderr" {
        board.record_captured_stderr(id, &text);
    }
    tracing::info!(process = %id, stream, message = %text, "captured child output");
}

pub(crate) const READER_JOIN_BUDGET: Duration = Duration::from_millis(250);

pub(crate) async fn join_reader(task: Option<JoinHandle<()>>) {
    if let Some(task) = task {
        let _ = timeout(READER_JOIN_BUDGET, task).await;
    }
}
