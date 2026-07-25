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
const ALLOWED_RAW_MODULE_IMPORTS: &[&str] = &[
    "src/commands/behavior.rs",
    "src/commands/logs.rs",
    "src/commands/status.rs",
    "src/supervisor/bus.rs",
    "src/telemetry.rs",
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
            .collect::<Vec<_>>();
        assert_eq!(
            path_dependencies, expected_path_dependencies,
            "{package_name} has forbidden workspace dependency edges; keep UI -> core -> external libraries and root -> UI/core as documented in ARCHITECTURE.md"
        );
    }
}

#[test]
fn privileged_raw_bus_imports_do_not_spread_silently() {
    assert_sorted_unique("ALLOWED_RAW_MODULE_IMPORTS", ALLOWED_RAW_MODULE_IMPORTS);
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut rust_files = Vec::new();
    collect_rust_files(&root.join("src"), &mut rust_files);
    collect_rust_files(&root.join("crates/core/src"), &mut rust_files);
    collect_rust_files(&root.join("crates/ui/src"), &mut rust_files);
    // Production source is the trust boundary. Test fixtures are excluded
    // intentionally because this audit itself contains bypass probes and
    // tests may construct raw bus fixtures without shipping that authority.
    let mut actual = rust_files
        .into_iter()
        .filter_map(|path| {
            let source = std::fs::read_to_string(&path).ok()?;
            assert!(
                !contains_public_raw_reexport(&source),
                "{} publicly re-exports privileged raw bus authority",
                path.display()
            );
            contains_raw_bus_access(&source).then_some(path)
        })
        .map(|path| {
            path.strip_prefix(root)
                .expect("source path should be inside the crate")
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect::<Vec<_>>();
    actual.sort();
    assert_eq!(actual, ALLOWED_RAW_MODULE_IMPORTS);
}

fn contains_raw_bus_access(source: &str) -> bool {
    let compact = source
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();
    if compact.contains("phoxal::raw") {
        return true;
    }
    let mut remainder = compact.as_str();
    while let Some(start) = remainder.find("phoxal::{") {
        remainder = &remainder[start + "phoxal::{".len()..];
        let Some(end) = remainder.find('}') else {
            break;
        };
        if remainder[..end]
            .split(',')
            .any(|item| item == "raw" || item.starts_with("raw::") || item.starts_with("rawas"))
        {
            return true;
        }
        remainder = &remainder[end + 1..];
    }
    let mut remainder = compact.as_str();
    while let Some(start) = remainder.find("usephoxalas") {
        remainder = &remainder[start + "usephoxalas".len()..];
        let Some(end) = remainder.find(';') else {
            break;
        };
        let alias = &remainder[..end];
        if !alias.is_empty() && compact.contains(&format!("{alias}::raw")) {
            return true;
        }
        remainder = &remainder[end + 1..];
    }
    false
}

fn contains_public_raw_reexport(source: &str) -> bool {
    let compact = source
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();
    compact
        .split(';')
        .any(|statement| statement.contains("pubusephoxal") && statement.contains("raw"))
}

#[test]
fn raw_bus_audit_recognizes_grouped_alias_and_reexport_bypasses() {
    assert!(contains_raw_bus_access("use phoxal::{raw::Bus, model};"));
    assert!(contains_raw_bus_access(
        "use phoxal as fw; use fw::raw::Bus;"
    ));
    assert!(contains_public_raw_reexport("pub use phoxal::raw::Bus;"));
}

fn collect_rust_files(directory: &Path, files: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(directory).expect("source directory should be readable") {
        let path = entry.expect("source entry should be readable").path();
        if path.is_dir() {
            collect_rust_files(&path, files);
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("rs") {
            files.push(path);
        }
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
