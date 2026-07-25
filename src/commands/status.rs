use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use phoxal::bus::{Subscribe, Subscriber, Topic};
use phoxal::raw::Bus;
use phoxal_api::v0_1 as api;
use tokio::time::timeout;

use crate::AppContext;
use crate::commands::bus_target::BusTargetArgs;

#[derive(Debug, Args)]
pub struct Status {
    #[command(subcommand)]
    pub command: StatusSubcommand,
}

// There is no software emergency-stop command. Every emergency stop is a
// manifest-declared component under the ordinary communication rules (#952
// section A), so the CLI has no privileged latch to engage: a robot with
// e-stop hardware declared is stopped through that hardware, and a robot
// without it has no robot-wide software stop by design.
#[derive(Debug, Subcommand)]
pub enum StatusSubcommand {
    #[command(about = "Inspect the latest domain-native safety constraints.")]
    Safety(BusTargetArgs),
    #[command(about = "Inspect the latest domain-native motion arbitration state.")]
    Motion(BusTargetArgs),
    #[command(about = "Inspect the latest domain-native localization estimate.")]
    Localization(BusTargetArgs),
}

impl Status {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        run_action(&self.command, app).await
    }
}

/// Open an observer session on the addressed run.
async fn inspect_bus(app: &AppContext, target: &BusTargetArgs, participant: &str) -> Result<Bus> {
    target.open(app, participant).await
}

async fn run_action(command: &StatusSubcommand, app: &AppContext) -> Result<()> {
    match command {
        StatusSubcommand::Safety(target) => inspect_safety(app, target).await,
        StatusSubcommand::Motion(target) => inspect_motion(app, target).await,
        StatusSubcommand::Localization(target) => inspect_localization(app, target).await,
    }
}

async fn inspect_localization(app: &AppContext, target: &BusTargetArgs) -> Result<()> {
    let bus = inspect_bus(app, target, "phoxal-cli-localization-inspect").await?;
    let topic = Topic::<Subscribe<api::localize::LocalizationState>>::new_static(
        <api::localize::LocalizationState as phoxal::bus::ContractBody>::TOPIC,
    );
    let subscriber = Subscriber::new(&bus, &topic, 32).await?;
    let state = timeout(Duration::from_secs(3), subscriber.recv())
        .await
        .context("localize did not publish domain state")??
        .body;
    println!(
        "localization: ({:.3}, {:.3}) yaw={:.3} confidence={:.3}",
        state.x_m, state.y_m, state.yaw_rad, state.confidence
    );
    bus.close().await?;
    Ok(())
}

async fn inspect_motion(app: &AppContext, target: &BusTargetArgs) -> Result<()> {
    let bus = inspect_bus(app, target, "phoxal-cli-motion-inspect").await?;
    let topic = Topic::<Subscribe<api::motion::State>>::new_static(
        <api::motion::State as phoxal::bus::ContractBody>::TOPIC,
    );
    let subscriber = Subscriber::new(&bus, &topic, 32).await?;
    let state = timeout(Duration::from_secs(3), subscriber.recv())
        .await
        .context("motion did not publish domain state")??
        .body;
    println!(
        "motion: source={:?} target=({:.3} m/s, {:.3} rad/s) zero={:?}",
        state.selected_source,
        state.final_target.linear_x_mps,
        state.final_target.angular_z_radps,
        state.zero_reason
    );
    println!(
        "  component_estop_blocked={} safety_constraints={}",
        state.component_estop_blocked,
        state.active_safety_constraints.len()
    );
    bus.close().await?;
    Ok(())
}

async fn inspect_safety(app: &AppContext, target: &BusTargetArgs) -> Result<()> {
    let bus = inspect_bus(app, target, "phoxal-cli-safety-inspect").await?;
    let topic = Topic::<Subscribe<api::safety::State>>::new_static(
        <api::safety::State as phoxal::bus::ContractBody>::TOPIC,
    );
    let subscriber = Subscriber::new(&bus, &topic, 32).await?;
    let state = timeout(Duration::from_secs(3), subscriber.recv())
        .await
        .context("safety did not publish domain state")??
        .body;
    println!(
        "safety: {} (sequence {}, expires at {})",
        if state.clear { "clear" } else { "constrained" },
        state.motion.sequence,
        state.motion.expires_at
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
    bus.close().await?;
    Ok(())
}
