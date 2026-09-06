//! Backend-neutral world compilation and exact-train adapter materialization.
//!
//! The CLI compiles one explicit authored world through `phoxal`, then
//! launches a separately installed adapter host against that closed bundle.
//! Native scene generation, controller staging, and Webots process ownership
//! stay inside the adapter packages.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use phoxal::bundle::RuntimeBundle;
use phoxal::bundle::WorldBundle;
use phoxal::model::simulation::FullSimulationPlan;
use phoxal::model::world::WorldDigest;
use phoxal::version::FrameworkVersion;

use crate::Reporter;
use crate::build::materialise::{
    MaterializationDestination, MaterializeProfile, MaterializeSpec, cargo_install_batch,
};
use crate::build::overlay;

const PACKAGE_DIRECTORIES: [(&str, &str); 3] = [
    (phoxal_cli_catalog::WEBOTS_HOST_PACKAGE, "host"),
    (
        phoxal_cli_catalog::WEBOTS_WORLD_CONTROLLER_PACKAGE,
        "world-controller",
    ),
    (
        phoxal_cli_catalog::WEBOTS_ROBOT_CONTROLLER_PACKAGE,
        "robot-controller",
    ),
];

/// Proof that one exact compiled bundle passed backend-neutral full-simulation
/// admission.
///
/// This value is intentionally neither clonable nor constructible by callers.
/// The CLI moves it together with the prepared release into the launch phase,
/// so execution cannot be launched from a model different from the one that
/// passed the preflight.
#[derive(Debug)]
pub struct FullSimulationAdmission {
    bundle: PathBuf,
    _plan: FullSimulationPlan,
}

impl FullSimulationAdmission {
    /// The exact compiled bundle this admission proves complete.
    #[must_use]
    pub fn bundle(&self) -> &Path {
        &self.bundle
    }
}

/// Validate one closed runtime bundle for backend-neutral full simulation.
///
/// This performs no build, process launch, session-record creation, or native
/// simulator work. Adapter-specific admission still runs against the frozen
/// supervisor model before native mutation.
pub fn validate_full_simulation_bundle(bundle_root: &Path) -> Result<FullSimulationAdmission> {
    let bundle = RuntimeBundle::open(bundle_root).with_context(|| {
        format!(
            "failed to open compiled robot bundle {} for simulation preflight",
            bundle_root.display()
        )
    })?;
    let plan = FullSimulationPlan::derive(bundle.robot())
        .context("compiled robot is incomplete for full simulation")?;

    let mut asset_lengths = std::collections::BTreeMap::new();
    for asset in plan.required_assets() {
        let bytes = bundle.asset(asset).with_context(|| {
            format!(
                "required simulation asset '{}' is unavailable in compiled bundle {}",
                asset,
                bundle_root.display()
            )
        })?;
        asset_lengths.insert(asset.clone(), bytes.len());
    }
    plan.validate_assets(|asset| asset_lengths.get(asset).copied())
        .context("compiled robot has incomplete simulation assets")?;

    Ok(FullSimulationAdmission {
        bundle: bundle_root.to_path_buf(),
        _plan: plan,
    })
}

/// One closed compiled world staged for a host process.
#[derive(Debug)]
pub struct CompiledWorld {
    bundle: WorldBundle,
    path: PathBuf,
}

impl CompiledWorld {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub const fn bundle(&self) -> &WorldBundle {
        &self.bundle
    }

    #[must_use]
    pub const fn digest(&self) -> WorldDigest {
        self.bundle.digest()
    }
}

/// Compile one explicit `world.yaml` and atomically write its inspectable
/// bundle into a new caller-owned directory.
pub fn compile_world(source: &Path, destination: &Path) -> Result<CompiledWorld> {
    let bundle = phoxal::authoring::world::compile(source)
        .with_context(|| format!("failed to compile world source {}", source.display()))?;
    bundle
        .write(destination)
        .with_context(|| format!("failed to stage world bundle {}", destination.display()))?;
    Ok(CompiledWorld {
        bundle,
        path: destination.to_path_buf(),
    })
}

/// The exact-train Webots adapter tools installed beside each other.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaterializedWebotsTools {
    root: PathBuf,
    host: PathBuf,
}

impl MaterializedWebotsTools {
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn host(&self) -> &Path {
        &self.host
    }
}

/// Materialize all three Webots adapter roles on the CLI host from one exact
/// framework train. The CLI launches only the host; the adjacent controller
/// executables are discovered and owned by that host.
pub fn materialize_webots_tools(
    framework: FrameworkVersion,
    offline: bool,
    reporter: &dyn Reporter,
) -> Result<MaterializedWebotsTools> {
    let cache = dirs::cache_dir()
        .context("the host has no per-user cache directory")?
        .join("phoxal")
        .join("simulation-tools")
        .join(if overlay::framework_path().is_some() {
            "development".to_string()
        } else {
            framework.to_string()
        });
    secure_tool_root(&cache)?;

    let target_dir = cache.join("target");
    let mut specs = Vec::with_capacity(PACKAGE_DIRECTORIES.len());
    for (package, directory) in PACKAGE_DIRECTORIES {
        let mut spec = MaterializeSpec::new(package, framework.to_string())
            .with_profile(MaterializeProfile::Release)
            .with_target_dir(target_dir.clone())
            .with_source(overlay::webots_adapter_source(directory));
        spec.destination = MaterializationDestination::HostTools;
        specs.push(spec);
    }
    ensure!(
        specs
            .iter()
            .all(|spec| !spec.destination.enters_runtime_document()),
        "Webots host tools must never enter a robot runtime document"
    );
    cargo_install_batch(&cache, &specs, offline, reporter)
        .with_context(|| format!("failed to materialize Webots adapter train {framework}"))?;

    let host = cache
        .join("bin")
        .join(phoxal_cli_catalog::WEBOTS_HOST_PACKAGE);
    ensure!(
        host.is_file(),
        "Webots host materialization did not produce {}",
        host.display()
    );
    Ok(MaterializedWebotsTools { root: cache, host })
}

#[cfg(unix)]
fn secure_tool_root(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::create_dir_all(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn secure_tool_root(_path: &Path) -> Result<()> {
    anyhow::bail!("local world adapters require Unix process ownership")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use phoxal::bundle::BundleWriter;
    use phoxal::model::builder::RobotBuilder;
    use phoxal::model::manifest::ManifestDocument;
    use phoxal::model::simulation;

    use super::*;

    fn write_bundle(
        robot: phoxal::model::Robot,
        assets: BTreeMap<phoxal::model::AssetId, Vec<u8>>,
    ) -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().expect("a temporary bundle parent");
        let bundle = root.path().join("bundle");
        BundleWriter::write(
            &bundle,
            &ManifestDocument::new(robot),
            &assets,
            &BTreeMap::new(),
        )
        .expect("the fixture bundle writes");
        (root, bundle)
    }

    #[test]
    fn adapter_packages_are_exactly_the_three_framework_train_roles() {
        assert_eq!(
            PACKAGE_DIRECTORIES,
            [
                ("phoxal-simulator-webots-host", "host"),
                (
                    "phoxal-simulator-webots-world-controller",
                    "world-controller"
                ),
                (
                    "phoxal-simulator-webots-robot-controller",
                    "robot-controller"
                ),
            ]
        );
    }

    #[test]
    fn every_adapter_role_is_a_host_tool_not_a_runtime() {
        for (package, directory) in PACKAGE_DIRECTORIES {
            let mut spec = MaterializeSpec::new(package, "0.68.2")
                .with_source(overlay::webots_adapter_source(directory));
            spec.destination = MaterializationDestination::HostTools;
            assert!(!spec.destination.enters_runtime_document());
        }
    }

    #[test]
    fn invalid_full_simulation_bundles_are_refused_before_launch_eligibility() {
        let missing = RobotBuilder::new("missing")
            .component_type("wheel", |wheel| wheel.encoder("turns", "axle"))
            .component("left", "wheel")
            .build()
            .expect("the hardware model is valid");
        let (_root, bundle) = write_bundle(missing, BTreeMap::new());
        let error = validate_full_simulation_bundle(&bundle)
            .expect_err("missing simulation data cannot produce an admission token");
        assert!(
            format!("{error:#}").contains("has no compiled simulation data"),
            "{error:#}"
        );

        let partial = RobotBuilder::new("partial")
            .component_type("wheel", |wheel| {
                wheel
                    .motor("spin", "axle")
                    .encoder("turns", "axle")
                    .simulated(
                        "spin",
                        simulation::Capability::Motor(simulation::Motor::default()),
                    )
            })
            .component("left", "wheel")
            .build()
            .expect("the partial simulation remains a valid hardware model");
        let (_root, bundle) = write_bundle(partial, BTreeMap::new());
        let error = validate_full_simulation_bundle(&bundle)
            .expect_err("partial capability coverage cannot produce an admission token");
        assert!(format!("{error:#}").contains("left.turns"), "{error:#}");

        let valid_link = RobotBuilder::new("unknown-link")
            .component_type("wheel", |wheel| {
                wheel
                    .motor("spin", "axle")
                    .simulated(
                        "spin",
                        simulation::Capability::Motor(simulation::Motor::default()),
                    )
                    .contact_material("axle_link", "rubber")
            })
            .component("left", "wheel")
            .build()
            .expect("the simulation link names component structure");
        let (_root, bundle) = write_bundle(valid_link, BTreeMap::new());
        let manifest_path = bundle.join(phoxal::bundle::MANIFEST_FILE);
        let mut manifest: serde_json::Value = serde_json::from_slice(
            &std::fs::read(&manifest_path).expect("the valid manifest reads"),
        )
        .expect("the valid manifest is JSON");
        let links = manifest["component_types"]["wheel"]["simulation"]["links"]
            .as_object_mut()
            .expect("the valid simulation has a link map");
        let contact = links
            .remove("axle_link")
            .expect("the valid contact material names axle_link");
        links.insert("ghost".to_owned(), contact);
        std::fs::write(
            &manifest_path,
            serde_json::to_vec(&manifest).expect("the corrupted manifest encodes"),
        )
        .expect("the corrupted manifest writes");
        let error = validate_full_simulation_bundle(&bundle)
            .expect_err("an unknown simulated link cannot produce an admission token");
        assert!(format!("{error:#}").contains("ghost"), "{error:#}");

        let mask = "components/camera/meshes/noise-mask.png";
        let missing_asset = RobotBuilder::new("missing-asset")
            .component_type("camera", |camera| {
                camera.camera("image", "lens").simulated(
                    "image",
                    simulation::Capability::Camera(simulation::Camera {
                        noise_mask_url: Some(mask.to_owned()),
                        ..simulation::Camera::default()
                    }),
                )
            })
            .component("front", "camera")
            .build()
            .expect("the simulation model is complete apart from its bundle asset");
        let (_root, bundle) = write_bundle(missing_asset, BTreeMap::new());
        let error = validate_full_simulation_bundle(&bundle)
            .expect_err("a missing local simulation asset cannot produce an admission token");
        assert!(format!("{error:#}").contains(mask), "{error:#}");
    }
}
