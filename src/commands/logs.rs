use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Args;
use phoxal::bus::{ContractBody, Subscribe, Subscriber, Topic};
use phoxal::raw::{Bus, BusConfig};
use phoxal_api::v1 as api;
use tokio::time::timeout;

use crate::AppContext;
use crate::supervisor::{logs_wildcard_topic_key, render_log_event};
use phoxal_cli_core::project::launch_plan::DEFAULT_ROUTER_CONNECT;
use phoxal_cli_core::project::resolver::{discover_robot_yaml, load_robot_with_extras};

#[derive(Debug, Args)]
pub struct Logs {
    #[arg(short = 'f', long, help = "Follow log events until interrupted.")]
    pub follow: bool,
    #[arg(
        value_name = "PARTICIPANT",
        help = "Participant id to stream. Omit for all participants."
    )]
    pub participant: Option<String>,
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
        help = "Robot namespace. Defaults to robot.yaml."
    )]
    pub namespace: Option<String>,
    #[arg(
        long = "robot-id",
        value_name = "ID",
        help = "Robot id. Defaults to robot.yaml."
    )]
    pub robot_id: Option<String>,
}

impl Logs {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        let (namespace, robot_id) = resolve_identity(
            app.project.root(),
            self.namespace.clone(),
            self.robot_id.clone(),
        )?;
        stream_logs(
            namespace,
            robot_id,
            self.connect.clone(),
            self.participant.clone(),
            self.follow,
        )
        .await
    }
}

fn resolve_identity(
    project_start: &Path,
    namespace: Option<String>,
    robot_id: Option<String>,
) -> Result<(String, String)> {
    match (namespace, robot_id) {
        (Some(namespace), Some(robot_id)) => Ok((namespace, robot_id)),
        (namespace, robot_id) => {
            let robot_path = discover_robot_yaml(project_start).with_context(|| {
                format!("failed to find robot.yaml from {}", project_start.display())
            })?;
            let loaded = load_robot_with_extras(&robot_path)?;
            Ok((
                namespace.unwrap_or(loaded.robot.robot.namespace),
                robot_id.unwrap_or(loaded.robot.robot.id),
            ))
        }
    }
}

async fn stream_logs(
    namespace: String,
    robot_id: String,
    connect: String,
    participant: Option<String>,
    follow: bool,
) -> Result<()> {
    let bus = Bus::open(BusConfig {
        namespace,
        robot_id,
        participant: "phoxal-cli-logs".to_string(),
        incarnation: 0,
        connect_endpoints: vec![connect],
    })
    .await?;
    let topic_key = match participant {
        Some(participant) => {
            <api::logs::Event as ContractBody>::TOPIC.replace("{participant_id}", &participant)
        }
        None => logs_wildcard_topic_key(),
    };
    let topic = Topic::<Subscribe<api::logs::Event>>::new_owned(topic_key);
    let subscriber = Subscriber::<api::logs::Event>::new(&bus, &topic, 256).await?;

    if follow {
        loop {
            print_received(subscriber.recv().await?);
        }
    }

    match timeout(Duration::from_millis(750), subscriber.recv()).await {
        Ok(received) => print_received(received?),
        Err(_) => eprintln!("no log events received"),
    }
    Ok(())
}

fn print_received(received: phoxal::bus::Received<api::logs::Event>) {
    println!(
        "[{}] {}",
        received.metadata.source.participant,
        render_log_event(&received.body)
    );
}
