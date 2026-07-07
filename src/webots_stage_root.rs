//! The Webots simulation staging root (Part 4/5 follow-up): a single,
//! home-based, per-play scratch area under `~/.phoxal/run/simulation/webots`
//! (relocatable via `PHOXAL_HOME`, see [`crate::host_paths`]).
//!
//! This used to be `<robot-project-root>/dist/simulator/webots`
//! (`project::Project::staged_webots_root` up to the previous design), but a
//! project-scoped staging tree fought the fact that Webots runs exactly one
//! world per `simulate` invocation and the staged content (a synthesized
//! world + generated PROTOs + the resolved robot's controller/mesh assets) is
//! never meant to be a durable part of the project checkout. Moving it under
//! `~/.phoxal/run/` keeps it out of the project tree entirely (nothing to
//! `.gitignore`, nothing to accidentally commit) while staying discoverable
//! and host-local like the rest of `run_dir()`'s contents (the supervisor
//! lock/state/action files are siblings, not children, of `simulation/`).
//!
//! Every play wipes and recreates this root (see [`wipe_and_recreate`])
//! since a previous play's stale worlds/protos/meshes/controllers must never
//! linger, given there is exactly one Webots world per run, so a clean slate
//! is both correct and cheap. Within the root, `worlds/` and `protos/` still
//! hold real generated files (they are synthesized fresh every play, so
//! there is nothing in the cache to link to), while `controllers/<name>/<name>`
//! and `meshes/<component>` are symlinks into the cache/path-pinned source so
//! the cache stays the single source of truth (see `commands::simulate`).

use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::host_paths;

/// The staged Webots root itself: `~/.phoxal/run/simulation/webots`.
pub fn root() -> Result<PathBuf> {
    Ok(host_paths::run_dir()?.join("simulation").join("webots"))
}

/// Where generated robot/component PROTOs are written for this play.
pub fn protos_dir() -> Result<PathBuf> {
    Ok(root()?.join("protos"))
}

/// Where mesh assets are staged (as symlinks per component type; see the
/// module doc) for this play.
pub fn meshes_dir() -> Result<PathBuf> {
    Ok(root()?.join("meshes"))
}

/// Where the synthesized world file is written for this play.
pub fn worlds_dir() -> Result<PathBuf> {
    Ok(root()?.join("worlds"))
}

/// The staged path of the synthesized world named `world_name`.
pub fn world_path(world_name: &str) -> Result<PathBuf> {
    Ok(worlds_dir()?.join(format!("{world_name}.wbt")))
}

/// The staged controller directory for `controller_name`
/// (`controllers/<name>/`); the controller binary itself is a symlink named
/// exactly `<name>` inside it, per Webots' `controllers/<name>/<name>`
/// convention.
pub fn controller_dir(controller_name: &str) -> Result<PathBuf> {
    Ok(root()?.join("controllers").join(controller_name))
}

/// Wipe the staged root (if present) and recreate its standard
/// `worlds|protos|controllers|meshes` substructure, empty, ready for this
/// play to populate. Must run once at the start of staging a simulation
/// (the live `stage_simulation_for_robot` entry point) - never for a
/// dry-run, which computes the intended path but writes nothing.
pub fn wipe_and_recreate() -> Result<PathBuf> {
    let root = root()?;
    if root.exists() {
        std::fs::remove_dir_all(&root).with_context(|| {
            format!(
                "failed to wipe stale staged simulation at {}",
                root.display()
            )
        })?;
    }
    for subdir in ["worlds", "protos", "controllers", "meshes"] {
        let path = root.join(subdir);
        std::fs::create_dir_all(&path).with_context(|| {
            format!(
                "failed to create staged simulation directory {}",
                path.display()
            )
        })?;
    }
    Ok(root)
}
