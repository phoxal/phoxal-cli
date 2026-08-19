//! The framework this workspace is allowed to depend on, and how much of it.
//!
//! The framework is one library. Everything this CLI reads from it - the
//! attachment SDK, the contract families, the canonical model, the bundle, the
//! authored-source compiler, the launch encoder - is `phoxal`, plus the proc
//! macro crate `phoxal` itself needs. A second framework package appearing in
//! the resolved graph would mean some member had gone back to pinning an
//! internal library, which is the topology `0.66` removed.
//!
//! The second half is the profile. The CLI is a `session` + `authoring`
//! consumer: it attaches to executions and compiles authored source. It is not
//! a participant, not a simulator, and emphatically not the supervisor, whose
//! profile compiles the supervisor implementation itself. Cargo unifies
//! features across a workspace, so one member enabling one of those would
//! silently enable it for the whole build - which is exactly what this reads
//! back out of the resolved graph rather than out of the manifests.
//!
//! This is a manifest-level fact, so it is asked of `cargo metadata` rather
//! than of anything compiled: a Rust `use` that reached a retired package
//! would fail to build anyway, and what this catches is the dependency edge
//! that nothing happens to import yet.

// Every line of this crate is test code, so the workspace's production denials
// on panicking accessors are lifted here exactly as the `#[cfg(test)]` modules
// inside each package lift them: a malformed `cargo metadata` document has no
// recovery, and a panic naming the missing field is the report.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::todo,
    clippy::unimplemented
)]

use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;

use serde_json::Value;

/// The whole framework library surface a consumer may depend on.
const ALLOWED_FRAMEWORK_PACKAGES: [&str; 2] = ["phoxal", "phoxal-macros"];

/// The consumer profiles this workspace must never turn on.
///
/// `participant` is the robot-author profile, `simulator` the external world
/// adapter's, and `supervisor` the exact-train profile that compiles the
/// supervisor process itself. The CLI builds and launches all three kinds of
/// process; it is none of them.
const FORBIDDEN_PROFILES: [&str; 3] = ["participant", "simulator", "supervisor"];

/// The profiles the CLI does select, in the one place they are pinned.
const REQUIRED_PROFILES: [&str; 2] = ["authoring", "session"];

fn metadata() -> Value {
    let output = Command::new(env!("CARGO"))
        .args(["metadata", "--format-version", "1"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("cargo metadata runs");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("cargo metadata emits JSON")
}

fn strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Every resolved package, by the opaque id the resolve graph refers to it by.
///
/// A package id spells its source as well as its name, and it spells it
/// differently for a registry dependency and for a local path overlay, so a
/// caller that wants the *name* looks it up here rather than reading it out of
/// the id.
fn packages_by_id(metadata: &Value) -> BTreeMap<String, String> {
    metadata
        .get("packages")
        .and_then(Value::as_array)
        .expect("cargo metadata lists packages")
        .iter()
        .filter_map(|package| {
            let id = package.get("id").and_then(Value::as_str)?;
            let name = package.get("name").and_then(Value::as_str)?;
            Some((id.to_string(), name.to_string()))
        })
        .collect()
}

/// Every `phoxal`-named package in the resolved graph that this workspace does
/// not itself contain: that is the framework's footprint here.
fn framework_packages(metadata: &Value) -> BTreeSet<String> {
    let members = strings(metadata.get("workspace_members"))
        .into_iter()
        .collect::<BTreeSet<_>>();
    packages_by_id(metadata)
        .into_iter()
        .filter(|(id, _)| !members.contains(id))
        .map(|(_, name)| name)
        .filter(|name| name == "phoxal" || name.starts_with("phoxal-"))
        .collect()
}

/// The one framework library is the only framework package here.
#[test]
fn the_workspace_consumes_the_framework_as_one_library() {
    let packages = framework_packages(&metadata());
    assert_eq!(
        packages,
        ALLOWED_FRAMEWORK_PACKAGES
            .iter()
            .map(|name| (*name).to_string())
            .collect::<BTreeSet<_>>(),
        "the resolved graph must contain `phoxal` and its proc-macro crate and \
         no other framework package"
    );
}

/// The unified feature set is exactly the two consumer profiles this workspace
/// declares, with no host or participant profile pulled in behind them.
#[test]
fn the_workspace_selects_only_the_session_and_authoring_profiles() {
    let metadata = metadata();
    let names = packages_by_id(&metadata);
    let node = metadata
        .get("resolve")
        .and_then(|resolve| resolve.get("nodes"))
        .and_then(Value::as_array)
        .expect("cargo metadata resolves the graph")
        .iter()
        .find(|node| {
            node.get("id")
                .and_then(Value::as_str)
                .and_then(|id| names.get(id))
                .is_some_and(|name| name == "phoxal")
        })
        .cloned()
        .expect("the resolved graph contains `phoxal`");

    let features = strings(node.get("features"))
        .into_iter()
        .collect::<BTreeSet<_>>();
    for profile in REQUIRED_PROFILES {
        assert!(
            features.contains(profile),
            "the CLI needs the `{profile}` profile; resolved features are {features:?}"
        );
    }
    for profile in FORBIDDEN_PROFILES {
        assert!(
            !features.contains(profile),
            "no member of this workspace may enable the `{profile}` profile; \
             resolved features are {features:?}"
        );
    }
}
