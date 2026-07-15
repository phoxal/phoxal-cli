//! Opt-in live proof for the manual-command path. Run only against an already
//! running robot-v1 Webots session:
//!
//! `cargo test --test live_manual_webots -- --ignored --nocapture`

use std::time::Duration;

use phoxal::bus::{ContractBody, LogicalTime, Publish, Publisher, Subscribe, Subscriber, Topic};
use phoxal::raw::{Bus, BusConfig};
use phoxal_api::{v1 as api, v2 as preview};
use tokio::time::timeout;

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[ignore = "requires a running robot-v1 Webots session on tcp/localhost:7447"]
async fn manual_command_moves_then_stops_after_ttl() {
    let bus = Bus::open(BusConfig {
        namespace: "dev".to_string(),
        robot_id: "robot-v1".to_string(),
        participant: "live-manual-webots-test".to_string(),
        incarnation: 0,
        connect_endpoints: vec!["tcp/localhost:7447".to_string()],
    })
    .await
    .expect("connect to the live simulation router");
    let manual_topic = Topic::<Publish<api::motion::ManualCommand>>::new_owned(
        api::topic::new()
            .motion()
            .manual()
            .publish_key()
            .expect("manual publish key")
            .to_owned(),
    );
    let manual = Publisher::new(bus.clone(), &manual_topic).expect("manual publisher");
    let software_estop = Publisher::new(
        bus.clone(),
        &Topic::<Publish<api::motion::EmergencyStopRequest>>::new_static(
            <api::motion::EmergencyStopRequest as ContractBody>::TOPIC,
        ),
    )
    .expect("software e-stop publisher");
    let component_estop_topic = Topic::<Publish<api::component::emergency_stop::State>>::new_owned(
        api::topic::new()
            .component("simulation_estop")
            .emergency_stop("emergency_stop")
            .state()
            .publish_key()
            .expect("component e-stop publish key")
            .to_owned(),
    );
    let component_estop =
        Publisher::new(bus.clone(), &component_estop_topic).expect("component e-stop publisher");
    let clock = Subscriber::new(
        &bus,
        &Topic::<Subscribe<preview::simulation::Clock>>::new_static(
            <preview::simulation::Clock as ContractBody>::TOPIC,
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
    let first_time = LogicalTime::new(first_clock.metadata.epoch, first_clock.body.now_ns);
    component_estop
        .publish_at(
            first_time,
            api::component::emergency_stop::State { engaged: false },
        )
        .await
        .expect("release component e-stop");

    let safety_state = timeout(Duration::from_secs(3), safety.recv())
        .await
        .expect("safety state timeout")
        .expect("safety state sample")
        .body;
    assert!(safety_state.motion.expires_at_ns > first_clock.body.now_ns);

    // No software e-stop reset is sent before this command. A fresh manual
    // input must be enough to select motion on a robot whose configured
    // component e-stop is freshly released.
    for _ in 0..10 {
        let sample = timeout(Duration::from_secs(1), clock.recv())
            .await
            .expect("startup manual clock timeout")
            .expect("startup manual clock sample");
        manual
            .publish_at(
                LogicalTime::new(sample.metadata.epoch, sample.body.now_ns),
                api::motion::ManualCommand {
                    linear_x_mps: 0.05,
                    angular_z_radps: 0.0,
                },
            )
            .await
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

    software_estop
        .publish_at(
            first_time,
            api::motion::EmergencyStopRequest { engaged: true },
        )
        .await
        .expect("engage software e-stop");
    timeout(Duration::from_secs(3), async {
        loop {
            let state = motion.recv().await?.body;
            if state.software_estop_engaged {
                assert_eq!(state.final_target.linear_x_mps, 0.0);
                return Ok::<(), phoxal::bus::BusError>(());
            }
        }
    })
    .await
    .expect("software e-stop was not observed")
    .expect("motion stream");
    software_estop
        .publish_at(
            first_time,
            api::motion::EmergencyStopRequest { engaged: false },
        )
        .await
        .expect("reset software e-stop");

    for _ in 0..20 {
        let sample = timeout(Duration::from_secs(1), clock.recv())
            .await
            .expect("component e-stop clock timeout")
            .expect("component e-stop clock sample");
        component_estop
            .publish_at(
                LogicalTime::new(sample.metadata.epoch, sample.body.now_ns),
                api::component::emergency_stop::State { engaged: true },
            )
            .await
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
        .publish_at(
            first_time,
            api::component::emergency_stop::State { engaged: false },
        )
        .await
        .expect("release component e-stop");

    let baseline = timeout(Duration::from_secs(3), localization.recv())
        .await
        .expect("localization timeout")
        .expect("localization sample")
        .body;
    for _ in 0..100 {
        let sample = timeout(Duration::from_secs(1), clock.recv())
            .await
            .expect("simulation clock timeout")
            .expect("simulation clock sample");
        manual
            .publish_at(
                LogicalTime::new(sample.metadata.epoch, sample.body.now_ns),
                api::motion::ManualCommand {
                    linear_x_mps: 0.1,
                    angular_z_radps: 0.0,
                },
            )
            .await
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
            if state.zero_reason == Some(api::motion::ZeroReason::ManualCandidateStale) {
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
