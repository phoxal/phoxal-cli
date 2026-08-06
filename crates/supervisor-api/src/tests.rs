//! The protocol's own tests: keys, side branding, and schema tagging.
//!
//! These prove the contract, not a daemon. The one place a real bus appears is
//! the key-composition test, which opens an in-process session (no listener, no
//! scouting) purely to read back the root it composes.

use phoxal_bus::{AskQuery, BusConfig, ContractBody, Publish, ServeQuery, Subscribe, Topic};
use phoxal_runtime_contract::ExecutionId;

use crate::model::{ExecutionMode, RobotIdentity};
use crate::schemas::{SupervisorSchemas, current_api};
use crate::text::Name;
use crate::{IDENTITY_KEY, identity_key, identity_key_under, supervisor};

fn assert_publish<B: ContractBody>(_topic: Topic<Publish<B>>) {}
fn assert_subscribe<B: ContractBody>(_topic: Topic<Subscribe<B>>) {}
fn assert_ask<Req: ContractBody, Resp: ContractBody>(_topic: Topic<AskQuery<Req, Resp>>) {}
fn assert_serve<Req: ContractBody, Resp: ContractBody>(_topic: Topic<ServeQuery<Req, Resp>>) {}

/// Every key this protocol declares, relative to the execution root. This list
/// is the contract the daemon implements the server side of, so it is asserted
/// exactly rather than derived.
#[test]
fn the_protocol_declares_exactly_these_relative_keys() {
    let keys = [
        (
            <supervisor::connect::Request as ContractBody>::TOPIC,
            "supervisor/connect",
        ),
        (
            <supervisor::connect::Reply as ContractBody>::TOPIC,
            "supervisor/connect",
        ),
        (
            <supervisor::snapshot::Update as ContractBody>::TOPIC,
            "supervisor/snapshot",
        ),
        (
            <supervisor::snapshot::Current as ContractBody>::TOPIC,
            "supervisor/snapshot/current",
        ),
        (
            <supervisor::command::Request as ContractBody>::TOPIC,
            "supervisor/command",
        ),
        (
            <supervisor::bundle::GetRequest as ContractBody>::TOPIC,
            "supervisor/bundle/get",
        ),
        (
            <supervisor::logs::Snapshot as ContractBody>::TOPIC,
            "supervisor/logs/snapshot",
        ),
        (
            <supervisor::logs::Follow as ContractBody>::TOPIC,
            "supervisor/logs/follow",
        ),
        (
            <supervisor::telemetry::Snapshot as ContractBody>::TOPIC,
            "supervisor/telemetry/snapshot",
        ),
        (
            <supervisor::telemetry::Follow as ContractBody>::TOPIC,
            "supervisor/telemetry/follow",
        ),
    ];
    for (actual, expected) in keys {
        assert_eq!(actual, expected);
    }

    // A protocol key carries no `v0.1/` segment: the leading segment is the
    // protocol name, and the payload's version lives in its serde tag.
    for (key, _) in keys {
        assert!(
            !key.contains("v0."),
            "{key} carries an API revision segment"
        );
        assert!(key.starts_with("supervisor/"), "{key}");
    }
}

/// The relative keys compose under the execution-scoped bus root, which is the
/// whole reason protocol mode drops the version segment. Proven against a real
/// (in-process) session rather than by restating the format string.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn relative_keys_compose_under_the_execution_root() {
    let execution = ExecutionId::mint();
    let bus = phoxal_bus::Bus::open(BusConfig::in_process("test-client").in_execution(execution))
        .await
        .expect("an in-process session opens without a router");

    assert_eq!(bus.root(), format!("phoxal/{execution}"));
    assert_eq!(
        bus.full_key(<supervisor::snapshot::Update as ContractBody>::TOPIC),
        format!("phoxal/{execution}/supervisor/snapshot")
    );
    assert_eq!(
        bus.full_key(<supervisor::bundle::GetRequest as ContractBody>::TOPIC),
        format!("phoxal/{execution}/supervisor/bundle/get")
    );

    // The liveliness token is not a topic, so it is stated rather than
    // derived - this is where that statement is pinned to the live root.
    assert_eq!(identity_key(execution), bus.full_key(IDENTITY_KEY));
    assert_eq!(identity_key_under(bus.root()), identity_key(execution));
    assert_eq!(
        identity_key(execution),
        format!("phoxal/{execution}/supervisor/identity")
    );

    bus.close().await.expect("the session closes");
}

/// Taking the wrong side of a supervisor topic is a compile error, exactly as
/// in the robot API. These calls would not build if a brand flipped.
#[test]
fn both_side_brandings_are_generated() {
    assert_ask(supervisor::topic::client().connect().topic());
    assert_serve(supervisor::topic::owner().connect().topic());

    assert_subscribe(supervisor::topic::client().snapshot().topic());
    assert_publish(supervisor::topic::owner().snapshot().topic());
    assert_ask(supervisor::topic::client().snapshot().current());
    assert_serve(supervisor::topic::owner().snapshot().current());

    assert_ask(supervisor::topic::client().command().topic());
    assert_serve(supervisor::topic::owner().command().topic());

    assert_ask(supervisor::topic::client().bundle().get());
    assert_serve(supervisor::topic::owner().bundle().get());

    assert_ask(supervisor::topic::client().logs().snapshot());
    assert_subscribe(supervisor::topic::client().logs().follow());
    assert_publish(supervisor::topic::owner().logs().follow());

    assert_ask(supervisor::topic::client().telemetry().snapshot());
    assert_subscribe(supervisor::topic::client().telemetry().follow());
    assert_publish(supervisor::topic::owner().telemetry().follow());
}

fn connect_reply() -> supervisor::connect::Reply {
    supervisor::connect::Reply::V0 {
        robot: RobotIdentity {
            id: Name::new("rover"),
            namespace: Name::new("demo"),
        },
        api: current_api(),
        schemas: SupervisorSchemas::current(),
        mode: ExecutionMode::Real,
    }
}

/// The current connect exchange round-trips through the wire codec with its
/// tag, and an unknown schema is rejected at parse time rather than reaching a
/// runtime version comparison.
#[test]
fn the_current_connect_round_trips_and_an_unknown_schema_is_rejected() {
    let request = supervisor::connect::Request::V0 {};
    let bytes = rmp_serde::to_vec_named(&request).unwrap();
    assert_eq!(
        rmp_serde::from_slice::<supervisor::connect::Request>(&bytes).unwrap(),
        request
    );

    let reply = connect_reply();
    let bytes = rmp_serde::to_vec_named(&reply).unwrap();
    assert_eq!(
        rmp_serde::from_slice::<supervisor::connect::Reply>(&bytes).unwrap(),
        reply
    );

    let json = serde_json::to_value(&reply).unwrap();
    assert_eq!(json["schema"], crate::CONNECT_SCHEMA);
    assert_eq!(json["api"], "v0.1");
    assert_eq!(json["schemas"]["snapshot"], crate::SNAPSHOT_SCHEMA);
    assert_eq!(json["mode"], "real");
    // Nothing the issue removed leaks back in through a stray field.
    for absent in [
        "framework",
        "train",
        "router",
        "execution",
        "generation",
        "manifest",
        "participants",
        "catalog",
    ] {
        assert!(json.get(absent).is_none(), "connect reply carries {absent}");
    }

    let mut foreign = json.clone();
    foreign["schema"] = serde_json::json!("phoxal/supervisor-connect/v9");
    assert!(serde_json::from_value::<supervisor::connect::Reply>(foreign).is_err());

    let untagged = serde_json::json!({});
    assert!(serde_json::from_value::<supervisor::connect::Request>(untagged).is_err());
}

/// Both snapshot carriers name the same schema and the same payload, so a
/// client that validated `schemas.snapshot` has validated both.
#[test]
fn the_stream_and_the_current_query_carry_one_snapshot_schema() {
    let snapshot = crate::model::Snapshot {
        revision: 7,
        robot: RobotIdentity {
            id: Name::new("rover"),
            namespace: Name::new("demo"),
        },
        mode: ExecutionMode::Simulated,
        lifecycle: crate::model::Lifecycle::Ready,
        startup: Vec::new(),
        processes: Vec::new(),
        failure: None,
    };
    let update = supervisor::snapshot::Update::V0(snapshot.clone());
    let current = supervisor::snapshot::Current::V0(snapshot);

    let update_json = serde_json::to_value(&update).unwrap();
    let current_json = serde_json::to_value(&current).unwrap();
    assert_eq!(update_json["schema"], crate::SNAPSHOT_SCHEMA);
    assert_eq!(update_json, current_json);

    let bytes = rmp_serde::to_vec_named(&update).unwrap();
    assert_eq!(
        rmp_serde::from_slice::<supervisor::snapshot::Update>(&bytes).unwrap(),
        update
    );
}
