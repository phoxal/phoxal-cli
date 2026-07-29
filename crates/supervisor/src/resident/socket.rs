use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::net::UnixListener;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{SupervisorAction, SupervisorState};

use super::server::{ServerState, accept_loop};

pub struct ResidentSocket {
    path: PathBuf,
    stop: CancellationToken,
    accept_task: Option<tokio::task::JoinHandle<()>>,
}

impl ResidentSocket {
    /// Bind while the caller holds the project lock. Only this authority may
    /// remove a stale pathname.
    pub fn bind(
        project: &Path,
        board: SupervisorState,
        actions: mpsc::Sender<SupervisorAction>,
        supervisor_token: CancellationToken,
    ) -> Result<Self> {
        let path = supervisor_socket_path(project)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to remove stale {}", path.display()));
            }
        }
        let listener = UnixListener::bind(&path)
            .with_context(|| format!("failed to bind {}", path.display()))?;
        let stop = CancellationToken::new();
        let state = ServerState::new(board, actions, supervisor_token);
        let accept_task = tokio::spawn(accept_loop(listener, state, stop.clone()));
        Ok(Self {
            path,
            stop,
            accept_task: Some(accept_task),
        })
    }

    /// Give existing writers one final bounded scheduling opportunity, then
    /// stop accepting and remove the pathname while lock authority is held.
    pub async fn close(mut self) {
        tokio::time::sleep(
            phoxal_cli_protocol::limits::FRAME_WRITE_TIMEOUT.min(Duration::from_millis(250)),
        )
        .await;
        self.stop.cancel();
        if let Some(accept_task) = self.accept_task.take() {
            let _ = accept_task.await;
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Drop for ResidentSocket {
    fn drop(&mut self) {
        // Created only after project-lock acquisition and dropped before that
        // lock guard, including error unwinds.
        self.stop.cancel();
        let _ = std::fs::remove_file(&self.path);
    }
}

pub fn supervisor_socket_path(project: &Path) -> Result<PathBuf> {
    let path = phoxal_cli_core::runtime::paths::RuntimePaths::for_root(project).supervisor_socket();
    let absolute = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    };
    let bytes = OsStr::new(&absolute).as_bytes();
    let maximum =
        std::mem::size_of::<libc::sockaddr_un>() - std::mem::size_of::<libc::sa_family_t>() - 1;
    if bytes.len() > maximum {
        bail!(
            "project supervisor socket path is {} bytes but this platform supports at most {maximum}: {}; move the project to a shorter path",
            bytes.len(),
            absolute.display()
        );
    }
    Ok(absolute)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_path_is_project_local_and_rejects_overlong_paths() {
        let project = Path::new("/tmp/phoxal-project");
        assert_eq!(
            supervisor_socket_path(project).unwrap(),
            project.join(".phoxal/supervisor.sock")
        );
        let long = PathBuf::from(format!("/tmp/{}", "x".repeat(256)));
        assert!(
            supervisor_socket_path(&long)
                .unwrap_err()
                .to_string()
                .contains("shorter")
        );
    }
}
