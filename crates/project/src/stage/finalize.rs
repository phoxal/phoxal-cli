//! Turning an authored robot project into the one finalized document a bundle
//! persists.
//!
//! Finalization is where the run intent stops being an option and becomes
//! manifest data:
//!
//! 1. read the exact source manifest, composing `extends` deterministically;
//! 2. validate it as a current `robot/v0` document;
//! 3. require the composed `extends` to be empty;
//! 4. rewrite every referenced path to its deterministic bundle-relative
//!    location below `assets/`;
//! 5. write `clock:` explicitly - `real` by default, `simulated` for a
//!    simulation build - and strip the `driver:` blocks the intent excludes;
//! 6. serialize the same `robot/v0` DTO.
//!
//! Nothing downstream carries a mode flag or a driver flag: the finalized
//! document alone answers "what runs and on which clock", and the one
//! requirement derivation reads a document that no longer mentions an excluded
//! driver at all.

use anyhow::{Context, Result, bail, ensure};
use phoxal_cli_core::project::intent::RunIntent;
use phoxal_manifest::source::robot::v0::Manifest;

/// The deterministic bundle-relative location of the robot structure, below
/// `assets/`.
pub(crate) const ROBOT_STRUCTURE_ASSET: &str = "robot/structure.urdf";

/// Apply `intent` to the composed authored document and return the finalized
/// one.
pub(crate) fn finalize_manifest(source: &Manifest, intent: &RunIntent) -> Result<Manifest> {
    ensure!(
        source.extends.is_empty(),
        "the composed robot document still declares `extends`; a bundle carries one fully \
         resolved document"
    );
    let mut finalized = source.clone();
    finalized.clock = intent.clock;
    finalized.robot.structure = ROBOT_STRUCTURE_ASSET.into();
    if finalized.router.config.is_some() {
        finalized.router.config = Some(
            phoxal_cli_core::project::layout::ROUTER_CONFIG_ASSET
                .parse()
                .context("router config asset path is not a path")?,
        );
    }
    for (instance, component) in &mut finalized.robot.components {
        if !intent.drivers.includes_instance(instance) {
            component.driver = None;
        }
    }
    finalized
        .validate()
        .map_err(|errors| finalization_error(&errors))?;
    Ok(finalized)
}

/// Render the validation failures of a finalized document. `clock: simulated`
/// combined with a remaining `driver:` block is one of them, so the rejection
/// the plan requires at finalization is the document grammar's own.
fn finalization_error(
    errors: &[phoxal_manifest::source::robot::v0::ValidationError],
) -> anyhow::Error {
    anyhow::anyhow!(
        "the finalized robot document is invalid:{}",
        errors
            .iter()
            .map(|error| format!("\n  - {error:?}"))
            .collect::<String>()
    )
}

/// Serialize a finalized document as the tagged `robot/v0` YAML a bundle root
/// carries.
pub(crate) fn render_finalized(manifest: &Manifest) -> Result<String> {
    let document = phoxal_manifest::source::robot::Manifest::V0(manifest.clone());
    let text = serde_yaml::to_string(&document)
        .context("failed to serialize the finalized robot document")?;
    // A finalized document must survive its own reader: parse it back before it
    // is ever written, so a serialization defect fails here rather than at the
    // daemon.
    let phoxal_manifest::source::robot::Manifest::V0(round_tripped) =
        phoxal_manifest::source::robot::parse_from_string(&text)
            .context("the finalized robot document does not parse back")?;
    if &round_tripped != manifest {
        bail!("the finalized robot document does not round-trip through its own reader");
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoxal_cli_core::project::intent::DriverSelection;
    use phoxal_manifest::source::robot::v0::Clock;

    const AUTHORED: &str = r#"schema: robot/v0
robot:
  id: testbot
  namespace: dev
  structure: model/structure.urdf
  motion_limits:
    max_linear_speed_mps: 0.6
    max_angular_speed_radps: 2.0
  kinematic:
    kind: omnidirectional
    actuators:
      - left_drive.motor
    encoders: []
  components:
    left_drive:
      component: ddsm115
      mount_link: base
      driver:
        connection:
          type: serial
          port: /dev/ttyUSB0
          baud: 115200
    right_drive:
      component: ddsm115
      mount_link: base
      driver:
        connection:
          type: serial
          port: /dev/ttyUSB1
          baud: 115200
"#;

    fn authored() -> Manifest {
        let phoxal_manifest::source::robot::Manifest::V0(manifest) =
            phoxal_manifest::source::robot::parse_from_string(AUTHORED).expect("fixture parses");
        manifest
    }

    fn drivers(manifest: &Manifest) -> Vec<&str> {
        manifest
            .robot
            .components
            .iter()
            .filter(|(_, component)| component.driver.is_some())
            .map(|(instance, _)| instance.as_str())
            .collect()
    }

    /// An authored document omits `clock:` and gets `real`; the finalized one
    /// always carries it explicitly, so the bundle answers the question on its
    /// own.
    #[test]
    fn an_authored_document_defaults_to_real_and_a_finalized_one_is_always_explicit() -> Result<()>
    {
        assert_eq!(authored().clock, Clock::Real);
        let finalized = finalize_manifest(&authored(), &RunIntent::default())?;
        assert_eq!(finalized.clock, Clock::Real);
        assert!(render_finalized(&finalized)?.contains("clock: real"));
        Ok(())
    }

    #[test]
    fn the_run_intent_strips_exactly_the_excluded_driver_blocks() -> Result<()> {
        let all = finalize_manifest(&authored(), &RunIntent::real(DriverSelection::All))?;
        assert_eq!(drivers(&all), vec!["left_drive", "right_drive"]);

        let subset = finalize_manifest(
            &authored(),
            &RunIntent::real(DriverSelection::Only(
                ["left_drive".to_string()].into_iter().collect(),
            )),
        )?;
        assert_eq!(drivers(&subset), vec!["left_drive"]);

        let off = finalize_manifest(&authored(), &RunIntent::real(DriverSelection::None))?;
        assert!(drivers(&off).is_empty());
        Ok(())
    }

    /// A simulation build carries `clock: simulated` with every driver block
    /// stripped, and a hand-made document that keeps one is rejected here
    /// rather than at the daemon.
    #[test]
    fn a_simulated_clock_rejects_a_remaining_driver_block() -> Result<()> {
        let simulated = finalize_manifest(&authored(), &RunIntent::simulated())?;
        assert_eq!(simulated.clock, Clock::Simulated);
        assert!(drivers(&simulated).is_empty());

        let error = finalize_manifest(
            &authored(),
            &phoxal_cli_core::project::intent::RunIntent {
                clock: Clock::Simulated,
                drivers: DriverSelection::All,
            },
        )
        .expect_err("`clock: simulated` plus a driver block must be rejected");
        assert!(
            error.to_string().contains("DriverUnderSimulatedClock"),
            "{error}"
        );
        Ok(())
    }

    /// Every referenced path in the finalized document is bundle-relative, so
    /// the authored source layout never controls the bundle.
    #[test]
    fn referenced_paths_are_rewritten_bundle_relative() -> Result<()> {
        let mut authored = authored();
        authored.router.config = Some("config/zenoh.json5".into());
        let finalized = finalize_manifest(&authored, &RunIntent::default())?;
        assert_eq!(
            finalized.robot.structure.to_string_lossy(),
            "robot/structure.urdf"
        );
        assert_eq!(
            finalized
                .router
                .config
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
            Some("router/config.json5".to_string())
        );
        Ok(())
    }
}
