use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

const ALLOWED_BASELINE: &[&str] = &[
    "phoxal",
    "phoxal-api",
    "phoxal-cli-client",
    "phoxal-cli-core",
    "phoxal-cli-ui",
];
#[test]
fn phoxal_cli_path_dependencies_do_not_grow_past_the_snapshot() {
    assert_sorted_unique("ALLOWED_BASELINE", ALLOWED_BASELINE);

    let metadata = cargo_metadata();
    let workspace_root = metadata["workspace_root"]
        .as_str()
        .expect("cargo metadata did not include workspace_root");
    let current_deps = phoxal_cli_path_dependencies(&metadata);
    let snapshot =
        read_snapshot(Path::new(workspace_root).join("tests/dependency_audit_snapshot.txt"));
    let allowed_baseline = ALLOWED_BASELINE
        .iter()
        .map(|name| (*name).to_owned())
        .collect::<BTreeSet<_>>();

    let still_violating = current_deps
        .intersection(&snapshot)
        .cloned()
        .collect::<BTreeSet<_>>();
    let unexpected = current_deps
        .difference(&snapshot)
        .cloned()
        .collect::<BTreeSet<_>>()
        .difference(&allowed_baseline)
        .cloned()
        .collect::<BTreeSet<_>>();
    let vestigial = snapshot
        .difference(&current_deps)
        .cloned()
        .collect::<BTreeSet<_>>();

    if unexpected.is_empty() && vestigial.is_empty() {
        return;
    }

    let mut message = String::from("phoxal-cli dependency audit failed\n");
    if !unexpected.is_empty() {
        message.push_str("\nUnexpected path dependencies:\n");
        for name in &unexpected {
            message.push_str(&format!(
                "- {name}: new dependency from phoxal-cli. If the dependency is intentional and not implementation-layer, add it to ALLOWED_BASELINE in dependency_audit.rs. If it is implementation-layer, add it to dependency_audit_snapshot.txt only as a transitional baseline - then plan to remove it.\n"
            ));
        }
    }
    if !vestigial.is_empty() {
        message.push_str("\nVestigial snapshot entries:\n");
        for name in &vestigial {
            message.push_str(&format!(
                "- {name}: no longer a dependency. Remove it from dependency_audit_snapshot.txt to ratchet the audit tighter.\n"
            ));
        }
    }
    if !still_violating.is_empty() {
        message.push_str("\nStill covered by the transitional snapshot:\n");
        for name in &still_violating {
            message.push_str(&format!("- {name}\n"));
        }
    }

    panic!("{message}");
}

#[test]
fn extracted_crates_follow_the_one_way_dependency_rule() {
    let metadata = cargo_metadata();
    let packages = metadata["packages"]
        .as_array()
        .expect("cargo metadata packages field was not an array");
    for (package_name, expected_path_dependencies) in [
        ("phoxal-cli-client", &["phoxal-cli-core"][..]),
        ("phoxal-cli-core", &[][..]),
        ("phoxal-cli-ui", &["phoxal-cli-core"][..]),
    ] {
        let package = packages
            .iter()
            .find(|package| package["name"].as_str() == Some(package_name))
            .unwrap_or_else(|| panic!("cargo metadata did not include {package_name}"));
        let path_dependencies = package["dependencies"]
            .as_array()
            .expect("package dependencies field was not an array")
            .iter()
            .filter(|dependency| dependency["source"].is_null())
            .filter_map(|dependency| dependency["name"].as_str())
            // The framework crates are path-pinned while the #952 train is
            // unreleased. That is a version pin, not a workspace edge - the
            // rule this test guards is about the CLI's own crates.
            .filter(|name| !matches!(*name, "phoxal" | "phoxal-api"))
            .collect::<Vec<_>>();
        assert_eq!(
            path_dependencies, expected_path_dependencies,
            "{package_name} has forbidden workspace dependency edges; keep UI -> core -> external libraries and root -> UI/core as documented in ARCHITECTURE.md"
        );
    }
}

fn cargo_metadata() -> Value {
    let output = Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("failed to run cargo metadata");

    if !output.status.success() {
        panic!(
            "cargo metadata failed with status {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    serde_json::from_slice(&output.stdout).expect("cargo metadata produced invalid JSON")
}

fn phoxal_cli_path_dependencies(metadata: &Value) -> BTreeSet<String> {
    let package = metadata["packages"]
        .as_array()
        .expect("cargo metadata packages field was not an array")
        .iter()
        .find(|package| package["name"].as_str() == Some("phoxal-cli"))
        .expect("cargo metadata did not include the phoxal-cli package");

    package["dependencies"]
        .as_array()
        .expect("phoxal-cli dependencies field was not an array")
        .iter()
        .filter(|dependency| dependency["source"].is_null())
        .map(|dependency| {
            dependency["name"]
                .as_str()
                .expect("phoxal-cli dependency did not include a string name")
                .to_owned()
        })
        .collect()
}

fn read_snapshot(path: PathBuf) -> BTreeSet<String> {
    let snapshot = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
    let names = snapshot
        .lines()
        .enumerate()
        .map(|(index, raw_line)| {
            let line = raw_line.trim_end();
            assert!(
                !line.trim().is_empty(),
                "{}:{} is empty or whitespace-only",
                path.display(),
                index + 1
            );
            line.to_owned()
        })
        .collect::<Vec<_>>();

    assert_sorted_unique_vec(&path, &names);

    names.into_iter().collect()
}

fn assert_sorted_unique(label: &str, names: &[&str]) {
    for pair in names.windows(2) {
        assert!(
            pair[0] < pair[1],
            "{label} must be sorted and contain unique crate names; `{}` should sort before `{}`",
            pair[1],
            pair[0]
        );
    }
}

fn assert_sorted_unique_vec(path: &Path, names: &[String]) {
    for pair in names.windows(2) {
        assert!(
            pair[0] < pair[1],
            "{} must be sorted and contain unique crate names; `{}` should sort before `{}`",
            path.display(),
            pair[1],
            pair[0]
        );
    }
}
