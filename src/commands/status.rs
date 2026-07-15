use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use phoxal::bus::{LogicalTime, Publish, Publisher, Subscribe, Subscriber, Topic};
use phoxal::raw::{Bus, BusConfig};
use phoxal_api::v1 as api;
use tokio::time::MissedTickBehavior;
use tokio::time::timeout;

use crate::AppContext;
use crate::commands::{MessageFormat, print_message};
use crate::launch_plan::DEFAULT_ROUTER_CONNECT;
use crate::resolver::{discover_robot_yaml, load_robot_with_extras};
use crate::supervisor::{
    SupervisorActionRequest, read_supervisor_state, request_supervisor_action,
    supervisor_state_path,
};

#[derive(Debug, Args)]
pub struct Status {
    #[arg(long, help = "Keep printing the supervisor status snapshot.")]
    pub watch: bool,
    #[arg(long, value_enum, default_value_t = MessageFormat::Human)]
    pub message_format: MessageFormat,
    #[command(subcommand)]
    pub command: Option<StatusSubcommand>,
}

#[derive(Debug, Subcommand)]
pub enum StatusSubcommand {
    #[command(about = "Stop a managed child and mark it released for a manual run.")]
    Release(ParticipantArg),
    #[command(about = "Respawn a released participant under supervisor control.")]
    Resume(ParticipantArg),
    #[command(about = "Engage the robot-wide software emergency stop.")]
    EngageEstop(EmergencyStopArg),
    #[command(about = "Reset the robot-wide software emergency stop.")]
    ResetEstop(EmergencyStopArg),
    #[command(about = "Inspect the latest domain-native safety constraints.")]
    Safety(SafetyArg),
    #[command(about = "Inspect the latest domain-native motion arbitration state.")]
    Motion(SafetyArg),
    #[command(about = "Inspect the latest domain-native localization estimate.")]
    Localization(SafetyArg),
}

#[derive(Debug, Args)]
pub struct ParticipantArg {
    #[arg(value_name = "PARTICIPANT")]
    pub participant: String,
}

#[derive(Debug, Args)]
pub struct EmergencyStopArg {
    #[arg(
        long,
        value_name = "ENDPOINT",
        default_value = DEFAULT_ROUTER_CONNECT,
        help = "Router endpoint to connect to."
    )]
    pub connect: String,
}

#[derive(Debug, Args)]
pub struct SafetyArg {
    #[arg(
        long,
        value_name = "ENDPOINT",
        default_value = DEFAULT_ROUTER_CONNECT,
        help = "Router endpoint to connect to."
    )]
    pub connect: String,
}

impl Status {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        if let Some(command) = &self.command {
            return run_action(command, app, self.message_format).await;
        }
        let path = supervisor_state_path()?;
        if !self.watch {
            let snapshot = read_supervisor_state(&path)?;
            return print_message(
                &snapshot,
                || {
                    print!("{}", snapshot.render());
                    Ok(())
                },
                self.message_format,
            );
        }

        let mut ticker = tokio::time::interval(Duration::from_secs(1));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            match read_supervisor_state(&path) {
                Ok(snapshot) => {
                    print_message(
                        &snapshot,
                        || {
                            print!("{}", snapshot.render());
                            Ok(())
                        },
                        self.message_format,
                    )?;
                }
                Err(error) if self.message_format == MessageFormat::Human => {
                    eprintln!("{error:#}");
                }
                Err(_) => {}
            }
        }
    }
}

async fn run_action(
    command: &StatusSubcommand,
    app: &AppContext,
    message_format: MessageFormat,
) -> Result<()> {
    match command {
        StatusSubcommand::EngageEstop(arg) => {
            return publish_estop(app, arg, true).await;
        }
        StatusSubcommand::ResetEstop(arg) => {
            return publish_estop(app, arg, false).await;
        }
        StatusSubcommand::Safety(arg) => {
            return inspect_safety(app, arg, message_format).await;
        }
        StatusSubcommand::Motion(arg) => {
            return inspect_motion(app, arg, message_format).await;
        }
        StatusSubcommand::Localization(arg) => {
            return inspect_localization(app, arg, message_format).await;
        }
        StatusSubcommand::Release(_) | StatusSubcommand::Resume(_) => {}
    }
    let (participant, action, request) = match command {
        StatusSubcommand::Release(arg) => (
            arg.participant.as_str(),
            "release",
            SupervisorActionRequest::Release {
                participant: arg.participant.clone(),
            },
        ),
        StatusSubcommand::Resume(arg) => (
            arg.participant.as_str(),
            "resume",
            SupervisorActionRequest::Resume {
                participant: arg.participant.clone(),
            },
        ),
        StatusSubcommand::EngageEstop(_)
        | StatusSubcommand::ResetEstop(_)
        | StatusSubcommand::Safety(_)
        | StatusSubcommand::Motion(_)
        | StatusSubcommand::Localization(_) => unreachable!(),
    };
    request_supervisor_action(request)?;
    app.ui
        .info(format!("queued supervisor {action} for {participant}"));
    Ok(())
}

async fn inspect_localization(
    app: &AppContext,
    arg: &SafetyArg,
    message_format: MessageFormat,
) -> Result<()> {
    let robot_path = discover_robot_yaml(app.project.root())?;
    let loaded = load_robot_with_extras(&robot_path)?;
    let bus = Bus::open(BusConfig {
        namespace: loaded.robot.robot.namespace,
        robot_id: loaded.robot.robot.id,
        participant: "phoxal-cli-localization-inspect".to_string(),
        incarnation: 0,
        connect_endpoints: vec![arg.connect.clone()],
    })
    .await?;
    let topic = Topic::<Subscribe<api::localize::LocalizationState>>::new_static(
        <api::localize::LocalizationState as phoxal::bus::ContractBody>::TOPIC,
    );
    let subscriber = Subscriber::new(&bus, &topic, 32).await?;
    let state = timeout(Duration::from_secs(3), subscriber.recv())
        .await
        .context("localize did not publish domain state")??
        .body;
    print_message(
        &state,
        || {
            println!(
                "localization: ({:.3}, {:.3}) yaw={:.3} confidence={:.3}",
                state.x_m, state.y_m, state.yaw_rad, state.confidence
            );
            Ok(())
        },
        message_format,
    )?;
    bus.close().await?;
    Ok(())
}

async fn inspect_motion(
    app: &AppContext,
    arg: &SafetyArg,
    message_format: MessageFormat,
) -> Result<()> {
    let robot_path = discover_robot_yaml(app.project.root())?;
    let loaded = load_robot_with_extras(&robot_path)?;
    let bus = Bus::open(BusConfig {
        namespace: loaded.robot.robot.namespace,
        robot_id: loaded.robot.robot.id,
        participant: "phoxal-cli-motion-inspect".to_string(),
        incarnation: 0,
        connect_endpoints: vec![arg.connect.clone()],
    })
    .await?;
    let topic = Topic::<Subscribe<api::motion::State>>::new_static(
        <api::motion::State as phoxal::bus::ContractBody>::TOPIC,
    );
    let subscriber = Subscriber::new(&bus, &topic, 32).await?;
    let state = timeout(Duration::from_secs(3), subscriber.recv())
        .await
        .context("motion did not publish domain state")??
        .body;
    print_message(
        &state,
        || {
            println!(
                "motion: source={:?} target=({:.3} m/s, {:.3} rad/s) zero={:?}",
                state.selected_source,
                state.final_target.linear_x_mps,
                state.final_target.angular_z_radps,
                state.zero_reason
            );
            println!(
                "  software_estop={} component_estop_blocked={} safety_constraints={}",
                state.software_estop_engaged,
                state.component_estop_blocked,
                state.active_safety_constraints.len()
            );
            Ok(())
        },
        message_format,
    )?;
    bus.close().await?;
    Ok(())
}

async fn inspect_safety(
    app: &AppContext,
    arg: &SafetyArg,
    message_format: MessageFormat,
) -> Result<()> {
    let robot_path = discover_robot_yaml(app.project.root())?;
    let loaded = load_robot_with_extras(&robot_path)?;
    let bus = Bus::open(BusConfig {
        namespace: loaded.robot.robot.namespace,
        robot_id: loaded.robot.robot.id,
        participant: "phoxal-cli-safety-inspect".to_string(),
        incarnation: 0,
        connect_endpoints: vec![arg.connect.clone()],
    })
    .await?;
    let topic = Topic::<Subscribe<api::safety::State>>::new_static(
        <api::safety::State as phoxal::bus::ContractBody>::TOPIC,
    );
    let subscriber = Subscriber::new(&bus, &topic, 32).await?;
    let state = timeout(Duration::from_secs(3), subscriber.recv())
        .await
        .context("safety did not publish domain state")??
        .body;
    print_message(
        &state,
        || {
            println!(
                "safety: {} (sequence {}, expires at {}ns)",
                if state.clear { "clear" } else { "constrained" },
                state.motion.sequence,
                state.motion.expires_at_ns
            );
            for constraint in &state.motion.constraints {
                println!(
                    "  {:?} from {} stop={} linear_limit={:?} observed={:?}",
                    constraint.reason,
                    constraint.source.participant_id,
                    constraint.stop,
                    constraint.max_linear_speed_mps,
                    constraint.observed_value
                );
            }
            Ok(())
        },
        message_format,
    )?;
    bus.close().await?;
    Ok(())
}

async fn publish_estop(app: &AppContext, arg: &EmergencyStopArg, engaged: bool) -> Result<()> {
    let robot_path = discover_robot_yaml(app.project.root()).with_context(|| {
        format!(
            "failed to find robot.yaml from {}",
            app.project.root().display()
        )
    })?;
    let loaded = load_robot_with_extras(&robot_path)?;
    let bus = Bus::open(BusConfig {
        namespace: loaded.robot.robot.namespace,
        robot_id: loaded.robot.robot.id,
        participant: "phoxal-cli-estop".to_string(),
        incarnation: 0,
        connect_endpoints: vec![arg.connect.clone()],
    })
    .await?;
    let topic = Topic::<Publish<api::motion::EmergencyStopRequest>>::new_owned(
        api::topic::new().motion().estop().publish_key()?.to_owned(),
    );
    let state_topic = Topic::<Subscribe<api::motion::State>>::new_owned(
        api::topic::new().motion().state().key().to_owned(),
    );
    let states = Subscriber::new(&bus, &state_topic, 32).await?;
    let publisher = Publisher::new(bus.clone(), &topic)?;
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    publisher
        .publish_at(
            LogicalTime::new(0, u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX)),
            api::motion::EmergencyStopRequest { engaged },
        )
        .await?;
    timeout(Duration::from_secs(3), async {
        loop {
            let state = states.recv().await?;
            if state.body.software_estop_engaged == engaged {
                return Ok::<(), phoxal::bus::BusError>(());
            }
        }
    })
    .await
    .with_context(|| {
        if engaged {
            "motion did not acknowledge the software emergency stop"
        } else {
            "motion did not acknowledge the software emergency-stop reset"
        }
    })??;
    bus.close().await?;
    app.ui.info(if engaged {
        "software emergency stop engaged"
    } else {
        "software emergency stop reset"
    });
    Ok(())
}
