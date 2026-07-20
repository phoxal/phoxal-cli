//! Deployment option validation and target-triple selection.

use super::DeployOptions;
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;

pub(crate) fn validate_deploy_options(options: &DeployOptions) -> Result<()> {
    if options.dry_run {
        if options.host.is_none() && options.target.is_none() {
            bail!(
                "--dry-run needs either <user@host> (its arch is probed read-only) or --target <arch> for a hostless render"
            );
        }
        if options
            .host
            .as_deref()
            .is_some_and(|host| host.trim().is_empty() || host.chars().any(char::is_whitespace))
        {
            bail!("deploy host must be a non-empty SSH destination without whitespace");
        }
    } else {
        let host = options
            .host
            .as_deref()
            .context("deploy requires <user@host> unless --dry-run is set")?;
        if host.trim().is_empty() || host.chars().any(char::is_whitespace) {
            bail!("deploy host must be a non-empty SSH destination without whitespace");
        }
        if let Some(target) = options.target.as_deref() {
            match target {
                "mender" | "rauc" => {
                    bail!("--target {target} is reserved for future OS-update adapters")
                }
                "compose" | "balena" => {
                    bail!("--target {target} is not supported; deploy renders native systemd only")
                }
                _ => bail!(
                    "live deploy probes the robot arch; --target is only valid with --dry-run"
                ),
            }
        }
    }
    Ok(())
}

pub(crate) fn parse_deploy_host(value: &str) -> Result<String, String> {
    match value {
        "build" | "push" => Err(
            "deploy has one verb; use `deploy <user@host>` or `deploy --dry-run --target <arch>`"
                .to_string(),
        ),
        value if value.trim().is_empty() || value.chars().any(char::is_whitespace) => {
            Err("deploy host must be a non-empty SSH destination without whitespace".to_string())
        }
        value => Ok(value.to_string()),
    }
}

/// Placeholder deploy-group enrollee for `--dry-run`, which renders no host
/// and so never probes a real remote user. The rendered fragment is
/// inspectable but is never installed anywhere, since dry-run never contacts
/// a host.
pub(crate) const DRY_RUN_REMOTE_USER: &str = "<deploy-user>";
