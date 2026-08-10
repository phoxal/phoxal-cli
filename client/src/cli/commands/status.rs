//! `phoxal status` - render the supervisor's authoritative snapshot.

use std::path::PathBuf;

use anyhow::Result;
use clap::Args;

use crate::cli::context::AppContext;

#[derive(Debug, Args)]
pub struct Status {
    #[arg(value_name = "PROJECT_OR_ENTRY")]
    pub target: Option<PathBuf>,
    #[arg(
        long,
        value_name = "ZENOH_ENDPOINT",
        help = "Report the execution at an explicit endpoint instead of this project's."
    )]
    endpoint: Option<String>,
}

impl Status {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        crate::application::lifecycle::status_command(
            app,
            self.target.as_deref(),
            self.endpoint.clone(),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use crate::cli::args::{Cli, RootCommand};
    use clap::Parser;

    /// `status` reports one execution's authoritative snapshot.
    #[test]
    fn status_takes_a_target_or_an_endpoint_and_no_domain_subcommand() {
        assert!(matches!(
            Cli::try_parse_from(["phoxal", "status"])
                .expect("bare status parses")
                .command,
            RootCommand::Status(_)
        ));
        assert!(matches!(
            Cli::try_parse_from(["phoxal", "status", "--endpoint", "tcp/robot:7447"])
                .expect("an explicit endpoint parses")
                .command,
            RootCommand::Status(_)
        ));
        // What follows `status` is a project, never a domain to query.
        let RootCommand::Status(status) = Cli::try_parse_from(["phoxal", "status", "safety"])
            .unwrap()
            .command
        else {
            panic!("expected status")
        };
        assert_eq!(
            status.target.as_deref(),
            Some(std::path::Path::new("safety"))
        );
        for removed in [
            vec!["phoxal", "status", "safety", "--connect", "tcp/robot:7447"],
            vec!["phoxal", "status", "motion", "--execution", "2b"],
        ] {
            assert!(
                Cli::try_parse_from(removed.clone()).is_err(),
                "removed status surface unexpectedly parsed: {removed:?}"
            );
        }
    }
}
