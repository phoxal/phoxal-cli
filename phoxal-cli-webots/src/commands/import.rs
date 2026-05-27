use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;

use phoxal_cli_core::AppContext;

#[derive(Debug, Parser, Clone)]
pub struct Import {
    #[arg(help = "Path to the source .osm file.")]
    pub input: PathBuf,

    #[arg(
        long,
        help = "Optional output world path. Defaults to simulator/webots/worlds/<input-stem>.wbt."
    )]
    pub output: Option<PathBuf>,
}
impl Import {
    pub fn execute(&self, app: &AppContext) -> Result<()> {
        let input = if self.input.is_absolute() {
            self.input.clone()
        } else {
            app.project.root().join(&self.input)
        }
        .canonicalize()
        .with_context(|| format!("failed to resolve OSM input path {}", self.input.display()))?;
        if input.extension().and_then(|ext| ext.to_str()) != Some("osm") {
            bail!("OSM input must end with .osm: {}", input.display());
        }

        let file_name = input
            .file_name()
            .ok_or_else(|| anyhow!("OSM input is missing a file name: {}", input.display()))?;
        let world_stem = input
            .file_stem()
            .and_then(|stem| stem.to_str())
            .ok_or_else(|| anyhow!("OSM input has an invalid file stem: {}", input.display()))?;

        let openstreet_dir = app.project.webots_openstreet_dir();
        fs::create_dir_all(&openstreet_dir)?;
        let staged_osm = app.project.webots_openstreet_map(
            file_name
                .to_str()
                .ok_or_else(|| anyhow!("OSM file name is not valid UTF-8: {}", input.display()))?,
        );
        if input != staged_osm {
            fs::copy(&input, &staged_osm).with_context(|| {
                format!(
                    "failed to copy imported OSM {} to {}",
                    input.display(),
                    staged_osm.display()
                )
            })?;
        }

        let output = match &self.output {
            Some(path) if path.is_absolute() => path.clone(),
            Some(path) => app.project.root().join(path),
            None => app.project.webots_world_source(world_stem),
        };
        if output.extension().and_then(|ext| ext.to_str()) != Some("wbt") {
            bail!("import output must end with .wbt: {}", output.display());
        }
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)?;
        }

        let venv_dir = openstreet_dir.join(".venv");
        let venv_python = venv_dir.join("bin/python");
        let pip = venv_dir.join("bin/pip");
        let webots_home = detect_webots_home()?;
        let importer_dir = webots_home.join("Contents/Resources/osm_importer");
        let config_file = importer_dir.join("config.ini");

        ensure_python_venv(app, &venv_dir, &venv_python)?;
        ensure_importer_dependencies(app, &venv_python, &pip)?;

        let import_title = format!(
            "Importing OpenStreetMap {} to {}",
            staged_osm.display(),
            output.display()
        );
        app.ui.step(&import_title, || {
            if output.exists() {
                fs::remove_file(&output)
                    .with_context(|| format!("failed to remove {}", output.display()))?;
            }
            let mut command = ProcessCommand::new(&venv_python);
            command
                .current_dir(&importer_dir)
                .arg("importer.py")
                .arg(format!("--input={}", staged_osm.display()))
                .arg(format!("--config-file={}", config_file.display()))
                .arg(format!("--output={}", output.display()));
            let status = app.ui.command_status(&mut command)?;
            if !status.success() {
                bail!("OpenStreetMap importer failed with status {status}");
            }
            Ok(())
        })?;

        patch_imported_world_for_repo(&output)?;

        app.ui
            .info(format!("Imported OSM copied to {}", staged_osm.display()));
        app.ui
            .info(format!("Generated Webots world {}", output.display()));

        Ok(())
    }
}
fn detect_webots_home() -> Result<PathBuf> {
    if let Some(webots_home) = std::env::var_os("WEBOTS_HOME") {
        return Ok(PathBuf::from(webots_home));
    }

    let default = PathBuf::from("/Applications/Webots.app");
    if default.exists() {
        return Ok(default);
    }

    bail!(
        "WEBOTS_HOME is not set and /Applications/Webots.app was not found; install Webots or set WEBOTS_HOME"
    )
}

fn ensure_python_venv(app: &AppContext, venv_dir: &Path, venv_python: &Path) -> Result<()> {
    if venv_python.exists() {
        return Ok(());
    }

    if let Some(parent) = venv_dir.parent() {
        fs::create_dir_all(parent)?;
    }
    let title = format!("Creating importer virtualenv {}", venv_dir.display());
    app.ui.step(&title, || {
        let mut command = ProcessCommand::new("python3");
        command.arg("-m").arg("venv").arg(venv_dir);
        let status = app.ui.command_status(&mut command)?;
        if !status.success() {
            bail!("python3 -m venv failed with status {status}");
        }
        Ok(())
    })?;

    Ok(())
}

fn ensure_importer_dependencies(app: &AppContext, venv_python: &Path, pip: &Path) -> Result<()> {
    let status = ProcessCommand::new(venv_python)
        .arg("-c")
        .arg("import lxml, pyproj, shapely, webcolors")
        .status()
        .context("failed to probe Webots importer Python dependencies")?;
    if status.success() {
        return Ok(());
    }

    app.ui
        .step("Installing Webots importer Python dependencies", || {
            let mut command = ProcessCommand::new(pip);
            command
                .arg("install")
                .args(["pyproj", "shapely", "webcolors", "lxml"]);
            let status = app.ui.command_status(&mut command)?;
            if !status.success() {
                bail!("pip install failed with status {status}");
            }
            Ok(())
        })?;
    Ok(())
}

fn patch_imported_world_for_repo(world_path: &Path) -> Result<()> {
    let mut world = fs::read_to_string(world_path)
        .with_context(|| format!("failed to read imported world {}", world_path.display()))?;
    if !world.contains("contactProperties [") {
        let replacement = "  lineScale 2\n  contactProperties [\n    ContactProperties {\n      material1 \"rubber_wheel\"\n      material2 \"default\"\n      coulombFriction [ 3.0 ]\n      bounce 0\n      bounceVelocity 0.1\n      forceDependentSlip [ 0 0 ]\n      softERP 0.2\n      softCFM 1e-9\n    }\n    ContactProperties {\n      material1 \"rubber_wheel\"\n      material2 \"grass\"\n      coulombFriction [ 1.0 ]\n      bounce 0\n      bounceVelocity 0.1\n      forceDependentSlip [ 0.02 0.02 ]\n      softERP 0.2\n      softCFM 1e-8\n    }\n  ]";
        world = world.replacen("  lineScale 2", replacement, 1);
    }
    if world.contains("Floor {\n") && !world.contains("contactMaterial \"grass\"") {
        world = world.replacen(
            "Floor {\n  translation",
            "Floor {\n  contactMaterial \"grass\"\n  translation",
            1,
        );
    }
    fs::write(world_path, world)
        .with_context(|| format!("failed to patch imported world {}", world_path.display()))?;
    Ok(())
}
