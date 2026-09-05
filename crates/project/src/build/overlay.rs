//! The development override for unpublished framework trains.
//!
//! Every official package - the services, the component drivers, and the
//! supervisor - is normally materialized from the `phoxal` registry at the
//! exact train the project locked. That is unavailable for a train that is
//! still being written, and a robot cannot be field-tested against a train
//! nobody can install.
//!
//! `PHOXAL_FRAMEWORK_PATH` points at a framework checkout, and every official
//! package is then materialized with `cargo install --path <checkout>/<crate>`
//! instead. The exact-train Webots adapter packages live in that same
//! framework checkout under `simulators/webots`.
//!
//! This changes *where the source comes from* and nothing else: the same
//! install, the same install root, the same caching, the same staging. It is a
//! development aid, and `phoxal doctor` reports it whenever it is set so an
//! operator can never be confused about which binaries they are running.

use std::path::PathBuf;

use phoxal_cli_catalog::ArtifactKind;

use crate::build::materialise::MaterializeSource;

/// The framework checkout every official package is built from when set.
pub const FRAMEWORK_PATH_VAR: &str = "PHOXAL_FRAMEWORK_PATH";

/// The framework checkout the operator directed official packages at.
#[must_use]
pub fn framework_path() -> Option<PathBuf> {
    non_empty(FRAMEWORK_PATH_VAR)
}

fn non_empty(variable: &str) -> Option<PathBuf> {
    let value = std::env::var_os(variable)?;
    if value.is_empty() {
        return None;
    }
    Some(PathBuf::from(value))
}

/// Where one official runtime's package is built from.
#[must_use]
pub fn official_source(kind: ArtifactKind, short_name: &str) -> MaterializeSource {
    framework_path().map_or(MaterializeSource::Registry, |checkout| {
        MaterializeSource::Path(checkout.join(official_crate_dir(kind, short_name)))
    })
}

/// Where the framework supervisor's package is built from.
#[must_use]
pub fn supervisor_source() -> MaterializeSource {
    framework_path().map_or(MaterializeSource::Registry, |checkout| {
        MaterializeSource::Path(checkout.join(SUPERVISOR_CRATE_DIR))
    })
}

/// Where one exact-train Webots adapter package is built from.
#[must_use]
pub fn webots_adapter_source(package_directory: &str) -> MaterializeSource {
    framework_path().map_or(MaterializeSource::Registry, |checkout| {
        MaterializeSource::Path(
            checkout
                .join("simulators")
                .join("webots")
                .join(package_directory),
        )
    })
}

/// The framework repository's own directory layout for its official packages.
const SUPERVISOR_CRATE_DIR: &str = "supervisor";

fn official_crate_dir(kind: ArtifactKind, short_name: &str) -> PathBuf {
    let family = match kind {
        ArtifactKind::Service => "services",
        ArtifactKind::ComponentDriver => "components",
    };
    PathBuf::from(family).join(short_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The crate directories are the framework repository's real layout, so an
    /// override resolves to a package that exists rather than to a path the
    /// install only discovers is wrong.
    #[test]
    fn an_override_resolves_the_framework_repositorys_own_layout() {
        assert_eq!(
            official_crate_dir(ArtifactKind::Service, "drive"),
            PathBuf::from("services/drive")
        );
        assert_eq!(
            official_crate_dir(ArtifactKind::ComponentDriver, "ddsm115"),
            PathBuf::from("components/ddsm115")
        );
        assert_eq!(
            PathBuf::from(SUPERVISOR_CRATE_DIR),
            PathBuf::from("supervisor")
        );
    }
}
