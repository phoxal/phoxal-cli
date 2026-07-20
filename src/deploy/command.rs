//! Deployment command entry point.

use super::{
    DRY_RUN_REMOTE_USER, Deploy, DeployOptions, DeployReport, DeployTransport, SshTransport,
    deploy_with_transport, local_tty_available, prepare_deploy, report, report_from_payload,
    validate_deploy_options,
};
use crate::AppContext;
use anyhow::Context;
use anyhow::Result;
use phoxal_cli_core::deploy::target_from_selector;
use phoxal_cli_core::deploy::target_from_uname_arch;
use std::path::Path;
use std::time::Duration;

impl Deploy {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        let options = DeployOptions {
            host: self.host.clone(),
            dry_run: self.dry_run,
            target: self.target.clone(),
            overlays: self.env.clone(),
            catalog_source: app.catalog_source.clone(),
            health_timeout: Duration::from_secs(self.health_timeout_sec),
        };
        let project_root = app.project.root().to_path_buf();
        let ui = app.ui;
        let result = tokio::task::spawn_blocking(move || run(&project_root, options, &ui))
            .await
            .context("deploy worker failed")??;
        eprintln!(
            "warning: v0 is pre-stable: artifacts built at different times may not interoperate"
        );
        report(result)
    }
}

pub(crate) fn run(
    project_start: &Path,
    options: DeployOptions,
    ui: &crate::Ui,
) -> Result<DeployReport> {
    validate_deploy_options(&options)?;
    if options.dry_run {
        // A dry-run against a host probes that host's arch (and reachability,
        // remote user) read-only - no mutation - so it validates against the
        // real machine instead of a hand-specified triple. `--target` overrides
        // the probe, and is required only for a hostless render (CI / offline).
        let (target, remote_user) = match options.host.as_deref() {
            Some(host) => {
                let mut transport = SshTransport::new(host.to_string(), *ui);
                let probe = transport.probe().context("failed to probe deploy host")?;
                let target = match options.target.as_deref() {
                    Some(selector) => target_from_selector(selector)?,
                    None => target_from_uname_arch(&probe.arch)?,
                };
                (target, probe.remote_user)
            }
            None => {
                let target = target_from_selector(
                    options
                        .target
                        .as_deref()
                        .context("--dry-run without a host requires --target <arch>")?,
                )?;
                (target, DRY_RUN_REMOTE_USER.to_string())
            }
        };
        let payload = prepare_deploy(project_start, &options, target, false, &remote_user, ui)?;
        return Ok(report_from_payload("dry-run", payload, None));
    }

    let host = options
        .host
        .as_deref()
        .context("deploy requires <user@host> unless --dry-run is set")?;
    let mut transport = SshTransport::new(host.to_string(), *ui);
    deploy_with_transport(
        project_start,
        &options,
        &mut transport,
        local_tty_available(),
        ui,
    )
}
