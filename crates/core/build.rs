//! Keep the CLI-owned catalog floor definitionally tied to the framework train.
//!
//! organization#965 originally named a `phoxal::VERSION` constant, but the CLI
//! deliberately does not depend on the `phoxal` authoring facade and the split
//! framework libraries expose no such constant. The root manifest instead pins
//! every split `phoxal-*` library it consumes. Because those libraries share one
//! workspace SemVer, requiring identical caret pins (`X.Y.Z`, i.e.
//! `>=X.Y.Z, <X.(Y+1).0` for a 0.x train) gives the catalog the same train
//! selected by a robot project's locked `phoxal` facade without restoring the
//! forbidden dependency edge or maintaining a second version literal. Patch
//! drift inside one train is allowed; the train itself is still hand-bumped,
//! and the committed `Cargo.lock` pins the exact versions actually built.

use std::{env, fs, path::PathBuf};

fn main() {
    let crate_root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let workspace_root = crate_root
        .parent()
        .and_then(std::path::Path::parent)
        .expect("phoxal-cli-core lives two directories below the workspace root");
    let workspace_manifest = workspace_root.join("Cargo.toml");
    println!("cargo:rerun-if-changed={}", workspace_manifest.display());

    let source = fs::read_to_string(&workspace_manifest).unwrap_or_else(|error| {
        panic!(
            "failed to read workspace manifest {}: {error}",
            workspace_manifest.display()
        )
    });
    let manifest = toml::from_str::<toml::Value>(&source)
        .unwrap_or_else(|error| panic!("failed to parse workspace manifest: {error}"));
    let dependencies = manifest
        .get("workspace")
        .and_then(|workspace| workspace.get("dependencies"))
        .and_then(toml::Value::as_table)
        .expect("workspace manifest has [workspace.dependencies]");

    let mut framework_libraries: Vec<_> = dependencies
        .iter()
        .filter(|(package, _)| {
            package.starts_with("phoxal-") && !package.starts_with("phoxal-cli-")
        })
        .collect();
    framework_libraries.sort_unstable_by_key(|(package, _)| package.as_str());
    assert!(
        !framework_libraries.is_empty(),
        "workspace manifest pins at least one split framework library"
    );

    let mut train = None;
    for (package, dependency) in framework_libraries {
        let requirement = dependency
            .as_str()
            .or_else(|| {
                dependency
                    .as_table()
                    .and_then(|table| table.get("version"))
                    .and_then(toml::Value::as_str)
            })
            .unwrap_or_else(|| panic!("workspace dependency {package} has no version"));
        let version = requirement;
        assert!(
            !version.starts_with(['=', '^', '~', '>', '<', '*']),
            "workspace dependency {package} must use a plain X.Y.Z caret train pin, found {requirement}"
        );
        semver::Version::parse(version)
            .unwrap_or_else(|error| panic!("workspace dependency {package} is invalid: {error}"));

        match train {
            Some(expected) if expected != version => {
                panic!("framework train pins disagree: {package} is {version}, expected {expected}")
            }
            None => train = Some(version),
            _ => {}
        }
    }

    println!(
        "cargo:rustc-env=PHOXAL_FRAMEWORK_TRAIN={}",
        train.expect("framework library pins established a train")
    );
}
