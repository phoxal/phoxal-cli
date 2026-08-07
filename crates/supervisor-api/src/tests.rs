//! The protocol's own tests: keys, side branding, and schema tagging.
//!
//! These prove the contract, not a daemon. The one place a real bus appears is
//! the key-composition test, which opens an in-process session (no listener, no
//! scouting) purely to read back the root it composes.

use phoxal_bus::{AskQuery, BusConfig, ContractBody, Publish, ServeQuery, Subscribe, Topic};
use phoxal_runtime_contract::ExecutionId;

use crate::model::{ExecutionMode, RobotIdentity};
use crate::schemas::{RobotApi, SupervisorSchemas};
use crate::text::Name;
use crate::{IDENTITY_KEY, supervisor};

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
    assert_eq!(
        bus.full_key(IDENTITY_KEY),
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

fn snapshot(revision: u64) -> crate::model::Snapshot {
    crate::model::Snapshot {
        revision,
        robot: RobotIdentity {
            id: Name::new("rover"),
            namespace: Name::new("demo"),
        },
        mode: ExecutionMode::Simulated,
        lifecycle: crate::model::Lifecycle::Ready,
        startup: Vec::new(),
        processes: Vec::new(),
        failure: None,
    }
}

fn connect_reply() -> supervisor::connect::Reply {
    supervisor::connect::Reply::V0 {
        robot: RobotIdentity {
            id: Name::new("rover"),
            namespace: Name::new("demo"),
        },
        api: RobotApi::V0_1,
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
    assert_eq!(json["schema"], "phoxal/supervisor/connect/reply/v0");
    assert_eq!(json["api"], RobotApi::V0_1.as_str());
    assert_eq!(
        json["schemas"]["snapshot"],
        crate::SnapshotSchema::CURRENT.as_str()
    );
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
    foreign["schema"] = serde_json::json!("phoxal/supervisor/connect/reply/v9");
    assert!(serde_json::from_value::<supervisor::connect::Reply>(foreign).is_err());

    let untagged = serde_json::json!({});
    assert!(serde_json::from_value::<supervisor::connect::Request>(untagged).is_err());
}

/// The snapshot stream and the `current` query are two *documents* over one
/// *payload*: distinct tags, because they are distinct endpoints, but the same
/// [`crate::model::Snapshot`] inside - so a client decodes one model and the
/// keep-highest rule compares revisions from either source.
#[test]
fn the_two_snapshot_carriers_share_one_payload_under_distinct_tags() {
    let payload = snapshot(7);
    let update = supervisor::snapshot::Update::V0(payload.clone());
    let current = supervisor::snapshot::Current::V0(payload.clone());

    let update_json = serde_json::to_value(&update).unwrap();
    let current_json = serde_json::to_value(&current).unwrap();

    // Distinct tags: the routing they mirror is distinct.
    assert_eq!(update_json["schema"], "phoxal/supervisor/snapshot/v0");
    assert_eq!(
        current_json["schema"],
        "phoxal/supervisor/snapshot/current/v0"
    );
    assert_ne!(update_json["schema"], current_json["schema"]);

    // One payload: everything except the tag is the same document, and both
    // destructure to the identical model value.
    let strip = |mut value: serde_json::Value| {
        value.as_object_mut().unwrap().remove("schema");
        value
    };
    assert_eq!(strip(update_json), strip(current_json));

    let supervisor::snapshot::Update::V0(from_stream) = update.clone();
    let supervisor::snapshot::Current::V0(from_query) = current;
    assert_eq!(from_stream, payload);
    assert_eq!(from_query, payload);

    let bytes = rmp_serde::to_vec_named(&update).unwrap();
    assert_eq!(
        rmp_serde::from_slice::<supervisor::snapshot::Update>(&bytes).unwrap(),
        update
    );
}

/// Which structure a tag's role suffix names.
///
/// The primary document of a topic - the stream it pushes, or the reply of a
/// query-only topic - carries no suffix, because it is what the topic *is*.
#[derive(Clone, Copy)]
enum Role {
    /// The topic's primary document: no suffix.
    Primary,
    Request,
    Reply,
}

impl Role {
    const fn suffix(self) -> &'static str {
        match self {
            Self::Primary => "",
            Self::Request => "/request",
            Self::Reply => "/reply",
        }
    }
}

/// Every document this protocol declares, with the tag it actually serializes
/// and the tag its topic path says it should.
///
/// The expected tag is *derived* from `ContractBody::TOPIC` - the same constant
/// the bus builds keys from - so a retagged document, a moved node, or a
/// renamed leaf all fail here. Nothing in this table restates a tag literal.
pub(crate) fn document_tags() -> Vec<(&'static str, String)> {
    table().into_iter().map(|(pair, _)| pair).collect()
}

#[allow(clippy::type_complexity)]
fn table() -> Vec<((&'static str, String), String)> {
    fn entry<B: ContractBody + serde::Serialize>(
        name: &'static str,
        document: B,
        role: Role,
    ) -> ((&'static str, String), String) {
        let tag = serde_json::to_value(&document).unwrap()["schema"]
            .as_str()
            .expect("every supervisor document is schema-tagged")
            .to_string();
        let derived = format!("phoxal/{}{}/v0", B::TOPIC, role.suffix());
        ((name, tag), derived)
    }

    vec![
        entry(
            "connect::Request",
            supervisor::connect::Request::V0 {},
            Role::Request,
        ),
        entry("connect::Reply", connect_reply(), Role::Reply),
        entry(
            "snapshot::Update",
            supervisor::snapshot::Update::V0(snapshot(1)),
            Role::Primary,
        ),
        entry(
            "snapshot::CurrentRequest",
            supervisor::snapshot::CurrentRequest::V0 {},
            Role::Request,
        ),
        entry(
            "snapshot::Current",
            supervisor::snapshot::Current::V0(snapshot(1)),
            Role::Primary,
        ),
        entry(
            "command::Request",
            supervisor::command::Request::V0 {
                command: crate::model::Command::Stop,
            },
            Role::Request,
        ),
        entry(
            "command::Reply",
            supervisor::command::Reply::V0 {
                outcome: crate::model::CommandOutcome::Accepted,
            },
            Role::Reply,
        ),
        entry(
            "bundle::GetRequest",
            supervisor::bundle::GetRequest::V0 {
                path: crate::text::BundlePath::new("robot.yaml"),
            },
            Role::Request,
        ),
        entry(
            "bundle::GetReply",
            supervisor::bundle::GetReply::V0 {
                outcome: crate::model::BundleGetOutcome::Missing,
            },
            Role::Reply,
        ),
        entry(
            "logs::SnapshotRequest",
            supervisor::logs::SnapshotRequest::V0 {
                participant: None,
                limit: 0,
                before_sequence: None,
            },
            Role::Request,
        ),
        entry(
            "logs::Snapshot",
            supervisor::logs::Snapshot::V0 {
                cursor: cursor(),
                ingest_dropped: 0,
                records: Vec::new(),
                next_before_sequence: None,
            },
            Role::Primary,
        ),
        entry(
            "logs::Follow",
            supervisor::logs::Follow::V0 {
                cursor: cursor(),
                ingest_dropped: 0,
                record: log_record(),
            },
            Role::Primary,
        ),
        entry(
            "telemetry::SnapshotRequest",
            supervisor::telemetry::SnapshotRequest::V0 {
                participant: None,
                limit: 0,
                before_sequence: None,
            },
            Role::Request,
        ),
        entry(
            "telemetry::Snapshot",
            supervisor::telemetry::Snapshot::V0 {
                cursor: cursor(),
                records: Vec::new(),
                capacity_evictions: 0,
                next_before_sequence: None,
            },
            Role::Primary,
        ),
        entry(
            "telemetry::Follow",
            supervisor::telemetry::Follow::V0 {
                cursor: cursor(),
                record: telemetry_record(),
            },
            Role::Primary,
        ),
    ]
}

/// Every tag mirrors its own topic's relative path. Derived, never restated:
/// the expected value is built from `ContractBody::TOPIC`, which is the same
/// constant the bus composes the key from, so a tag can never describe a route
/// the document does not actually travel.
#[test]
fn every_documents_tag_mirrors_its_topic_path() {
    for ((name, tag), derived) in table() {
        assert_eq!(
            tag, derived,
            "{name} is tagged `{tag}` but routes as `{derived}`"
        );
    }
}

/// A tag identifies one structure. Two documents sharing a tag would make the
/// tag descriptive rather than identifying - and would quietly reintroduce the
/// ambiguity typed versions exist to remove.
#[test]
fn no_two_documents_share_a_tag() {
    let documents = document_tags();
    let mut seen: std::collections::BTreeMap<String, &'static str> =
        std::collections::BTreeMap::new();
    for (name, tag) in &documents {
        if let Some(previous) = seen.insert(tag.clone(), name) {
            panic!("`{tag}` is carried by both {previous} and {name}");
        }
    }
    assert_eq!(
        seen.len(),
        documents.len(),
        "every document must have its own tag"
    );

    // Every tag is namespaced and versioned, so none can be mistaken for a bus
    // key or for another project's document.
    for (name, tag) in &documents {
        assert!(tag.starts_with("phoxal/supervisor/"), "{name}: {tag}");
        assert!(tag.ends_with("/v0"), "{name}: {tag}");
    }
}

fn cursor() -> crate::model::Cursor {
    crate::model::Cursor {
        generation: Name::new("g1"),
        sequence: 0,
    }
}

fn log_record() -> crate::model::LogRecord {
    crate::model::LogRecord {
        sequence: 1,
        participant: Name::new("brain"),
        source_sequence: 1,
        time: crate::model::WallTime {
            unix_seconds: 0,
            nanos: 0,
        },
        level: crate::model::LogLevel::Info,
        target: crate::text::LogText::new("phoxal"),
        message: crate::text::LogText::new("started"),
        fields: std::collections::BTreeMap::new(),
        dropped: 0,
        truncated: 0,
    }
}

fn telemetry_record() -> crate::model::TelemetryRecord {
    crate::model::TelemetryRecord {
        sequence: 1,
        participant: Name::new("brain"),
        truncated: 0,
        window_ns: 1_000_000,
        step: None,
        topics: Vec::new(),
        overflow: None,
    }
}

/// Prints the table `no_two_documents_share_a_tag` walks. Not an assertion -
/// run with `--ignored --nocapture` when a reviewer wants the contract on
/// screen.
#[test]
#[ignore = "diagnostic: prints the tag table"]
fn print_the_tag_table() {
    for (name, tag) in document_tags() {
        println!("{name:30} {tag}");
    }
}
