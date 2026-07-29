use std::path::{Path, PathBuf};
use std::{error::Error as StdError, fmt};

use anyhow::{Result, bail};
use phoxal_cli_protocol::codec::async_io::{read_frame, write_frame};
use phoxal_cli_protocol::limits::{
    FRAME_READ_TIMEOUT, FRAME_WRITE_TIMEOUT, MAX_HANDSHAKE_FRAME_BYTES,
};
use phoxal_cli_protocol::{
    CommandSessionId, ConnectionRole, HandshakeReply, HandshakeRequest, SUPERVISOR_PROTOCOL_VERSION,
};
use tokio::net::UnixStream;

pub(crate) const EARLY_CLOSE_RECOVERY: &str = concat!(
    "the resident rejected or disconnected during the supervisor handshake.\n\n",
    "It may have been started by an older Phoxal CLI, or it may be unhealthy.\n\n",
    "Stop it without using the supervisor protocol:\n\n",
    "    phoxal stop --force <target>\n\n",
    "Then start or attach again."
);

#[derive(Debug)]
pub(crate) struct ConnectionUnavailable {
    path: PathBuf,
    source: std::io::Error,
}

impl fmt::Display for ConnectionUnavailable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "connect to unavailable supervisor {}: {}",
            self.path.display(),
            self.source
        )
    }
}

impl StdError for ConnectionUnavailable {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        Some(&self.source)
    }
}

#[must_use]
pub fn is_connection_unavailable(error: &anyhow::Error) -> bool {
    error.downcast_ref::<ConnectionUnavailable>().is_some()
}

pub(crate) async fn connect_role(
    path: &Path,
    role: ConnectionRole,
    resume_command_session: Option<CommandSessionId>,
) -> Result<(UnixStream, HandshakeReply)> {
    let mut stream = UnixStream::connect(path)
        .await
        .map_err(|source| ConnectionUnavailable {
            path: path.to_path_buf(),
            source,
        })?;
    write_frame(
        &mut stream,
        &HandshakeRequest {
            protocol_version: SUPERVISOR_PROTOCOL_VERSION,
            role,
            resume_command_session,
        },
        MAX_HANDSHAKE_FRAME_BYTES,
        FRAME_WRITE_TIMEOUT,
    )
    .await?;
    let reply: HandshakeReply =
        read_frame(&mut stream, MAX_HANDSHAKE_FRAME_BYTES, FRAME_READ_TIMEOUT)
            .await
            .map_err(|_| anyhow::anyhow!(EARLY_CLOSE_RECOVERY))?;
    if reply.protocol_version != SUPERVISOR_PROTOCOL_VERSION {
        bail!(
            concat!(
                "the running resident uses supervisor protocol {}, but this CLI requires protocol {}.\n\n",
                "Stop the resident without using its protocol:\n\n",
                "    phoxal stop --force <target>\n\n",
                "Then start or attach again."
            ),
            reply.protocol_version,
            SUPERVISOR_PROTOCOL_VERSION
        );
    }
    Ok((stream, reply))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn nonexistent_socket_is_classified_as_retryable_unavailable() {
        let directory = tempfile::tempdir().unwrap();
        let error = connect_role(
            &directory.path().join("missing.sock"),
            ConnectionRole::Snapshots,
            None,
        )
        .await
        .unwrap_err();
        assert!(is_connection_unavailable(&error), "{error:#}");
    }
}
