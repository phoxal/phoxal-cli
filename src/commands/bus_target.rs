//! How an ad hoc CLI client addresses a running robot's bus.
//!
//! Every bus root is `<namespace>/robots/<robot-id>/x<execution>` (#952 section
//! B), so an observer needs all three parts before it can hear anything. A
//! client that minted its own `ExecutionId` would subscribe to a root nobody
//! publishes on and silently time out, which is the failure this module exists
//! to make impossible.
//!
//! The defaults come from where the operator usually is - inside the project,
//! with a run active - so the common case stays a bare `phoxal status safety`.
//! Every part is also nameable explicitly, which is what lets a second checkout
//! or a second host address a run whose project root it cannot see.

use anyhow::{Context, Result};
use clap::Args;
use phoxal::raw::{Bus, BusConfig};
use phoxal_cli_core::identity::{ExecutionId, ProducerId};

use crate::AppContext;
use phoxal_cli_core::project::launch_plan::DEFAULT_ROUTER_CONNECT;
use phoxal_cli_core::project::resolver::{discover_robot_yaml, load_robot};

/// The address of one running robot's bus.
#[derive(Debug, Clone, Args)]
pub struct BusTargetArgs {
    #[arg(
        long,
        value_name = "ENDPOINT",
        default_value = DEFAULT_ROUTER_CONNECT,
        help = "Router endpoint to connect to."
    )]
    pub connect: String,
    #[arg(
        long,
        value_name = "NAMESPACE",
        help = "Robot namespace. Defaults to the project's robot.yaml."
    )]
    pub namespace: Option<String>,
    #[arg(
        long = "robot-id",
        value_name = "ID",
        help = "Robot id. Defaults to the project's robot.yaml."
    )]
    pub robot_id: Option<String>,
    #[arg(
        long,
        value_name = "EXECUTION",
        value_parser = parse_execution,
        help = "Execution to address. Defaults to the run active in this project."
    )]
    pub execution: Option<ExecutionId>,
}

fn parse_execution(value: &str) -> Result<ExecutionId, String> {
    ExecutionId::parse(value).map_err(|error| error.to_string())
}

impl BusTargetArgs {
    /// Open an observer/publisher session on the addressed run.
    ///
    /// Each invocation is a fresh process and therefore a fresh `ProducerId`,
    /// which is what makes repeated ad hoc invocations acceptable under strict
    /// per-producer sequence rejection (#952 section G).
    pub async fn open(&self, app: &AppContext, participant: &str) -> Result<Bus> {
        let config = self.resolve(app, participant)?;
        Ok(Bus::open(config).await?)
    }

    fn resolve(&self, app: &AppContext, participant: &str) -> Result<BusConfig> {
        let (namespace, robot_id) = self.identity(app)?;
        let execution = match self.execution {
            Some(execution) => execution,
            None => crate::supervisor::active_execution(app.project.root())?.context(
                "no phoxal run is active in this project; start one, or name the run with \
                 --execution",
            )?,
        };
        Ok(BusConfig {
            namespace,
            robot_id,
            execution,
            participant: participant.to_string(),
            producer: ProducerId::mint(),
            connect_endpoints: vec![self.connect.clone()],
        })
    }

    /// The namespace and robot id, from the flags or from the project.
    ///
    /// Naming both explicitly is what lets this run outside a project root -
    /// `robot.yaml` is only consulted when something is still missing.
    fn identity(&self, app: &AppContext) -> Result<(String, String)> {
        if let (Some(namespace), Some(robot_id)) = (&self.namespace, &self.robot_id) {
            return Ok((namespace.clone(), robot_id.clone()));
        }
        let root = app.project.root();
        let robot_path = discover_robot_yaml(root).with_context(|| {
            format!(
                "failed to find robot.yaml from {}; name the robot with --namespace and \
                 --robot-id to address a run from outside its project",
                root.display()
            )
        })?;
        let robot = load_robot(&robot_path)?;
        Ok((
            self.namespace.clone().unwrap_or(robot.robot.namespace),
            self.robot_id.clone().unwrap_or(robot.robot.id),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_execution_round_trips_through_the_flag() {
        let execution = ExecutionId::mint();
        assert_eq!(
            parse_execution(&execution.to_string()).expect("a rendered execution parses"),
            execution
        );
        assert!(
            parse_execution("not-an-execution").is_err(),
            "a malformed execution must be rejected at the flag, not at the bus"
        );
    }
}
