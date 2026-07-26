//! Opt-in live proof for the manual-command path. Run only against an already
//! running robot-v1 Webots session:
//!
//! `cargo test --test live_manual_webots -- --ignored --nocapture`

use std::time::Duration;

use phoxal::bus::{
    CommandPublisher, ContractBody, StatePublisher, StepToken, Subscribe, Subscriber, Topic,
};
use phoxal::raw::{Bus, BusConfig};
use phoxal_api::v0_1 as api;
use tokio::time::timeout;

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[ignore = "requires a running robot-v1 Webots session for PHOXAL_PROJECT_ROOT"]
async fn manual_command_moves_then_stops_after_ttl() {
    let project_root = std::env::var_os("PHOXAL_PROJECT_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().expect("current project directory"));
    let connect = format!(
        "unixsock-stream/{}",
        project_root.join(".phoxal/zenoh.sock").display()
    );
    let bus = Bus::open(BusConfig {
        namespace: "dev".to_string(),
        robot_id: "robot-v1".to_string(),
        execution: phoxal_cli::active_execution(&project_root)
            .expect("read the running project lock")
            .expect("a live simulation must be running"),
        participant: "live-manual-webots-test".to_string(),
        producer: phoxal::bus::ProducerId::mint(),
        connect_endpoints: vec![connect],
    })
    .await
    .expect("connect to the live simulation router");
    let manual_topic = api::topic::client().motion().manual();
    let manual = CommandPublisher::new(bus.clone(), &manual_topic).expect("manual publisher");
    let component_estop_topic = api::topic::owner()
        .component("simulation_estop")
        .emergency_stop("emergency_stop")
        .state();
    let component_estop = StatePublisher::new(bus.clone(), &component_estop_topic)
        .expect("component e-stop publisher");
    let clock = Subscriber::new(
        &bus,
        &Topic::<Subscribe<api::simulation::Clock>>::new_static(
            <api::simulation::Clock as ContractBody>::TOPIC,
        ),
        32,
    )
    .await
    .expect("clock subscriber");
    let localization = Subscriber::new(
        &bus,
        &Topic::<Subscribe<api::localize::LocalizationState>>::new_static(
            <api::localize::LocalizationState as ContractBody>::TOPIC,
        ),
        32,
    )
    .await
    .expect("localization subscriber");
    let motion = Subscriber::new(
        &bus,
        &Topic::<Subscribe<api::motion::State>>::new_static(
            <api::motion::State as ContractBody>::TOPIC,
        ),
        32,
    )
    .await
    .expect("motion subscriber");
    let safety = Subscriber::new(
        &bus,
        &Topic::<Subscribe<api::safety::State>>::new_static(
            <api::safety::State as ContractBody>::TOPIC,
        ),
        32,
    )
    .await
    .expect("safety subscriber");

    let first_clock = timeout(Duration::from_secs(3), clock.recv())
        .await
        .expect("initial clock timeout")
        .expect("initial clock sample");
    // The world authority stamps the clock's instant in the envelope, so this
    // is the exact instant the world reached - not a value the test invents.
    let first_time = first_clock
        .metadata
        .produced_exactly_at()
        .expect("the simulation clock carries an exact world instant");
    component_estop
        .publish(
            &StepToken::__mint(first_time),
            api::component::emergency_stop::State { engaged: false },
        )
        .expect("release component e-stop");

    let safety_state = timeout(Duration::from_secs(3), safety.recv())
        .await
        .expect("safety state timeout")
        .expect("safety state sample")
        .body;
    assert_eq!(
        safety_state
            .motion
            .expires_at
            .checked_cmp(first_time)
            .expect("the safety product must be on the world's own timeline"),
        std::cmp::Ordering::Greater
    );

    // No software e-stop reset is sent before this command. A fresh manual
    // input must be enough to select motion on a robot whose configured
    // component e-stop is freshly released.
    for _ in 0..10 {
        // Pace off the world clock; the command itself carries no robot time.
        timeout(Duration::from_secs(1), clock.recv())
            .await
            .expect("startup manual clock timeout")
            .expect("startup manual clock sample");
        manual
            .send(api::motion::ManualCommand {
                linear_x_mps: 0.05,
                angular_z_radps: 0.0,
            })
            .expect("publish startup manual command");
    }
    timeout(Duration::from_secs(3), async {
        loop {
            let state = motion.recv().await?.body;
            if state.selected_source == Some(api::motion::Source::Manual)
                && state.final_target.linear_x_mps > 0.0
            {
                return Ok::<(), phoxal::bus::BusError>(());
            }
        }
    })
    .await
    .expect("startup manual command was not selected")
    .expect("motion stream");

    for _ in 0..20 {
        let sample = timeout(Duration::from_secs(1), clock.recv())
            .await
            .expect("component e-stop clock timeout")
            .expect("component e-stop clock sample");
        let at = sample
            .metadata
            .produced_exactly_at()
            .expect("the simulation clock carries an exact world instant");
        component_estop
            .publish(
                &StepToken::__mint(at),
                api::component::emergency_stop::State { engaged: true },
            )
            .expect("engage component e-stop");
    }
    timeout(Duration::from_secs(3), async {
        loop {
            let state = motion.recv().await?.body;
            if state.component_estop_blocked {
                assert_eq!(state.final_target.linear_x_mps, 0.0);
                return Ok::<(), phoxal::bus::BusError>(());
            }
        }
    })
    .await
    .expect("component e-stop was not observed")
    .expect("motion stream");
    component_estop
        .publish(
            &StepToken::__mint(first_time),
            api::component::emergency_stop::State { engaged: false },
        )
        .expect("release component e-stop");

    let baseline = timeout(Duration::from_secs(3), localization.recv())
        .await
        .expect("localization timeout")
        .expect("localization sample")
        .body;
    for _ in 0..100 {
        timeout(Duration::from_secs(1), clock.recv())
            .await
            .expect("simulation clock timeout")
            .expect("simulation clock sample");
        manual
            .send(api::motion::ManualCommand {
                linear_x_mps: 0.1,
                angular_z_radps: 0.0,
            })
            .expect("publish manual command");
    }

    let moved = timeout(Duration::from_secs(5), async {
        loop {
            let pose = localization.recv().await?.body;
            if (pose.x_m - baseline.x_m).abs() >= 0.02 {
                return Ok::<_, phoxal::bus::BusError>(pose);
            }
        }
    })
    .await
    .expect("robot did not move 2 cm")
    .expect("localization stream");
    assert!((moved.x_m - baseline.x_m).abs() >= 0.02);

    timeout(Duration::from_secs(3), async {
        loop {
            let state = motion.recv().await?.body;
            if state.zero_reason == Some(api::motion::ZeroReason::NoCandidate) {
                assert_eq!(state.final_target.linear_x_mps, 0.0);
                assert_eq!(state.final_target.angular_z_radps, 0.0);
                return Ok::<(), phoxal::bus::BusError>(());
            }
        }
    })
    .await
    .expect("motion did not stop after the manual TTL")
    .expect("motion stream");
    bus.close().await.expect("close test bus");
}
