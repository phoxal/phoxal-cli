//! Editor-schema generation for authored YAML.
//!
//! The schemas describe the exact authored-manifest parser linked into this
//! CLI, so they follow a CLI upgrade rather than the robot's locked framework
//! train. They are an editor aid only: `phoxal validate` remains authoritative
//! for semantic, cross-file, and runtime constraints.
//!
//! Output lives in `<project>/.phoxal/schemas/`, a sibling of the staged
//! runtime layout rather than a part of it, so a `build.phoxal` - which
//! archives only the staged layout - can never carry a generated schema.
//!
//! Each file is published through a temporary sibling plus rename. The rename
//! itself is not fsynced: these files are regenerable and idempotent, so a
//! post-crash rerun is a cheaper repair than a directory sync on every write.

use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use phoxal_manifest::schema::{DocumentKind, generate};

/// The project-relative directory the generated editor schemas are written to.
pub(crate) const SCHEMA_DIR_RELATIVE: &str = ".phoxal/schemas";

/// The suffix identifying a file in the schema directory as CLI-owned. Any
/// other file there is left alone; a stale one of ours is removed.
const SCHEMA_FILE_SUFFIX: &str = ".schema.json";

const DRAFT_2020_12: &str = "https://json-schema.org/draft/2020-12/schema";

/// One complete, verified schema set, held entirely in memory before any
/// filesystem replacement is attempted.
#[derive(Debug)]
struct GeneratedSchemas {
    schema_dir: PathBuf,
    files: Vec<(PathBuf, Vec<u8>)>,
}

/// What one generation did, for the command to report.
#[derive(Debug)]
pub(crate) struct SchemaReport {
    pub(crate) schema_dir: PathBuf,
    pub(crate) written: usize,
    /// Schemas this CLI no longer delivers, removed from its own directory.
    pub(crate) removed: Vec<String>,
    /// Retired schemas that could not be removed. The schemas themselves are
    /// written by then, so this is reported rather than failed.
    pub(crate) unremovable: Vec<String>,
}

/// Generate every authored document schema for the project containing `start`
/// and publish the set into `<project>/.phoxal/schemas/`.
pub(crate) fn generate_command(start: &Path) -> Result<SchemaReport> {
    let generated = generate_schemas(start)?;
    write_schemas(&generated)?;
    // Pruning must follow the writes, not precede them: it recognizes a
    // delivered file by what its path resolves to, and a path that does not
    // exist yet cannot resolve. Prune first and the delivered set is empty on
    // the first run, so a perfectly good schema from the previous run is
    // deleted and recreated instead of atomically replaced.
    let (removed, unremovable) = remove_retired_schemas(&generated);
    Ok(SchemaReport {
        schema_dir: generated.schema_dir,
        written: generated.files.len(),
        removed,
        unremovable,
    })
}

/// Generate and serialize the complete set, verifying each document before it
/// can reach the filesystem. This function never writes.
fn generate_schemas(start: &Path) -> Result<GeneratedSchemas> {
    let robot = phoxal_cli_core::project::resolver::discover_robot_yaml(start)
        .with_context(|| format!("failed to find robot.yaml from {}", start.display()))?;
    let root = robot
        .parent()
        .context("robot.yaml did not have a parent directory")?;
    let schema_dir = root.join(SCHEMA_DIR_RELATIVE);
    let mut names = BTreeSet::new();
    let mut files = Vec::with_capacity(DocumentKind::ALL.len());
    for kind in DocumentKind::ALL {
        let name = kind.file_name();
        // The owner's file name must be one plain path segment, so a schema
        // can never be written outside the project-owned directory.
        let mut segments = Path::new(name).components();
        ensure!(
            matches!(segments.next(), Some(Component::Normal(_))) && segments.next().is_none(),
            "manifest schema filename `{name}` is not a normal file name"
        );
        ensure!(
            name.ends_with(SCHEMA_FILE_SUFFIX),
            "manifest schema filename `{name}` does not end with `{SCHEMA_FILE_SUFFIX}`"
        );
        ensure!(
            names.insert(name),
            "manifest schema filename `{name}` is duplicated"
        );
        let value = generate(kind);
        ensure!(
            value.get("$schema").and_then(serde_json::Value::as_str) == Some(DRAFT_2020_12),
            "manifest schema `{name}` is not a Draft 2020-12 root"
        );
        let mut bytes = serde_json::to_vec_pretty(&value)
            .with_context(|| format!("failed to serialize manifest schema {name}"))?;
        bytes.push(b'\n');
        files.push((schema_dir.join(name), bytes));
    }
    Ok(GeneratedSchemas { schema_dir, files })
}

/// Replace each schema file through a temporary sibling plus rename. A failure
/// between per-file replacements leaves every earlier file valid; rerunning
/// repairs the set.
fn write_schemas(generated: &GeneratedSchemas) -> Result<()> {
    for directory in [
        generated
            .schema_dir
            .parent()
            .context("schema output has no project state parent")?,
        generated.schema_dir.as_path(),
    ] {
        if fs::symlink_metadata(directory).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            bail!(
                "schema output directory must not be a symlink: {}",
                directory.display()
            );
        }
    }
    fs::create_dir_all(&generated.schema_dir)
        .with_context(|| format!("failed to create {}", generated.schema_dir.display()))?;
    for (path, bytes) in &generated.files {
        let mut temp =
            tempfile::NamedTempFile::new_in(&generated.schema_dir).with_context(|| {
                format!(
                    "failed to create temporary schema beside {}",
                    path.display()
                )
            })?;
        // A temporary file is private to its creator; these are ordinary
        // generated project files an editor reads.
        #[cfg(unix)]
        fs::set_permissions(
            temp.path(),
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o644),
        )
        .with_context(|| format!("failed to write {}", path.display()))?;
        temp.write_all(bytes)
            .with_context(|| format!("failed to write {}", path.display()))?;
        temp.as_file_mut()
            .sync_all()
            .with_context(|| format!("failed to write {}", path.display()))?;
        temp.persist(path).map_err(|error| {
            anyhow::anyhow!("failed to write {}: {}", path.display(), error.error)
        })?;
    }
    Ok(())
}

/// Remove schemas this CLI once delivered but no longer does, so an authored
/// `# $schema:` comment can never keep resolving to a retired document.
///
/// Best effort by design. The schemas are already written when this runs, so
/// the command has met its contract; an unremovable file is reported, never a
/// failure. Returns the names removed and the names that resisted.
fn remove_retired_schemas(generated: &GeneratedSchemas) -> (Vec<String>, Vec<String>) {
    // A delivered file is identified by what it resolves to, not by how its
    // name is spelled: a case-insensitive volume keeps the directory entry's
    // stored casing across a rename, so `Robot.schema.json` can name the
    // `robot.schema.json` this run just wrote.
    let delivered = generated
        .files
        .iter()
        .filter_map(|(path, _)| fs::canonicalize(path).ok())
        .collect::<BTreeSet<_>>();
    let (mut removed, mut unremovable) = (Vec::new(), Vec::new());
    let Ok(entries) = fs::read_dir(&generated.schema_dir) else {
        return (removed, unremovable);
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(SCHEMA_FILE_SUFFIX)
            || !entry.file_type().is_ok_and(|kind| kind.is_file())
        {
            continue;
        }
        let path = entry.path();
        if fs::canonicalize(&path).is_ok_and(|resolved| delivered.contains(&resolved)) {
            continue;
        }
        match fs::remove_file(&path) {
            Ok(()) => removed.push(name),
            // Another run of this command reached the same goal state first.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => unremovable.push(name),
        }
    }
    removed.sort();
    unremovable.sort();
    (removed, unremovable)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROBOT_YAML: &str = "schema: phoxal/robot/v0\nname: rover\n";

    /// A project with a root `robot.yaml` and one component directory.
    fn project() -> Result<tempfile::TempDir> {
        let project = tempfile::tempdir()?;
        fs::write(project.path().join("robot.yaml"), ROBOT_YAML)?;
        fs::create_dir_all(project.path().join("components/wheel"))?;
        Ok(project)
    }

    /// Whether `directory` lives on a volume that folds case, which decides
    /// whether the delivered-name variant this project can produce exists.
    fn folds_case(directory: &Path) -> Result<bool> {
        let probe = directory.join("CaseProbe");
        fs::write(&probe, b"")?;
        let folded = directory.join("caseprobe").exists();
        fs::remove_file(&probe)?;
        Ok(folded)
    }

    fn entries(directory: &Path) -> Result<Vec<String>> {
        let mut names = fs::read_dir(directory)?
            .map(|entry| Ok(entry?.file_name().to_string_lossy().into_owned()))
            .collect::<Result<Vec<_>>>()?;
        names.sort();
        Ok(names)
    }

    #[test]
    fn the_delivered_inventory_is_the_manifest_owner_s() {
        // The owner names its own schema files. This locks the set the CLI
        // ships, so a pin bump that renames or adds one fails here, in CI,
        // rather than surprising a user at generation time.
        assert_eq!(
            DocumentKind::ALL.map(DocumentKind::file_name),
            [
                "robot.schema.json",
                "component.schema.json",
                "simulation.schema.json"
            ]
        );
    }

    #[test]
    fn discovery_resolves_the_root_from_the_root_a_component_and_the_robot_file() -> Result<()> {
        let project = project()?;
        let nested = project.path().join("components/wheel/nested/deeper");
        fs::create_dir_all(&nested)?;
        let expected = project.path().join(SCHEMA_DIR_RELATIVE);
        for start in [
            project.path().to_path_buf(),
            project.path().join("components/wheel"),
            nested,
            project.path().join("robot.yaml"),
        ] {
            let generated = generate_schemas(&start)?;
            assert_eq!(generated.schema_dir, expected, "from {}", start.display());
        }
        Ok(())
    }

    #[test]
    fn generation_writes_the_delivered_files_and_nothing_else() -> Result<()> {
        let project = project()?;
        let report = generate_command(project.path())?;
        let mut expected = DocumentKind::ALL
            .map(DocumentKind::file_name)
            .map(str::to_owned)
            .to_vec();
        expected.sort();
        assert_eq!(entries(&report.schema_dir)?, expected);
        assert!(report.removed.is_empty());
        assert!(report.unremovable.is_empty());
        Ok(())
    }

    #[test]
    fn every_delivered_file_is_a_titled_draft_2020_12_schema_in_readable_pretty_json() -> Result<()>
    {
        let project = project()?;
        let report = generate_command(project.path())?;
        for kind in DocumentKind::ALL {
            let name = kind.file_name();
            let path = report.schema_dir.join(name);
            let text = fs::read_to_string(&path)?;
            assert!(text.ends_with("}\n"), "{name} must end with one newline");
            assert!(!text.ends_with("\n\n"), "{name} must end with one newline");
            assert!(
                text.contains("\n  \""),
                "{name} must be pretty-printed, not compact"
            );
            let value = serde_json::from_str::<serde_json::Value>(&text)?;
            assert_eq!(
                value.get("$schema").and_then(serde_json::Value::as_str),
                Some(DRAFT_2020_12)
            );
            // An editor shows the title when it reports which schema it chose,
            // so each file must carry its own document's title, not a sibling's.
            let title = value
                .get("title")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            let document = name.trim_end_matches(SCHEMA_FILE_SUFFIX);
            assert!(
                title.starts_with("Phoxal ") && title.contains(document),
                "{name} title `{title}` must name its own authored document"
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = fs::metadata(&path)?.permissions().mode();
                assert_eq!(mode & 0o777, 0o644, "{name} must be readable");
            }
        }
        Ok(())
    }

    #[test]
    fn generation_is_deterministic_and_replaces_a_previous_schema_set() -> Result<()> {
        let project = project()?;
        let first = generate_schemas(project.path())?;
        write_schemas(&first)?;

        // A stale set from an older CLI is fully replaced, not merged.
        for (path, _) in &first.files {
            fs::write(path, b"{\"stale\": true}\n")?;
        }
        let second = generate_schemas(project.path())?;
        assert_eq!(
            first.files.iter().map(|(_, b)| b).collect::<Vec<_>>(),
            second.files.iter().map(|(_, b)| b).collect::<Vec<_>>(),
            "generation must be deterministic across runs"
        );
        write_schemas(&second)?;
        for (path, expected) in &second.files {
            assert_eq!(&fs::read(path)?, expected);
        }
        Ok(())
    }

    #[test]
    fn a_retired_schema_is_removed_while_anything_else_is_left_alone() -> Result<()> {
        let project = project()?;
        let report = generate_command(project.path())?;
        // A schema kind this CLI no longer delivers must not keep resolving.
        let retired = report.schema_dir.join("retired.schema.json");
        fs::write(&retired, b"{}\n")?;
        // Not ours: the directory is CLI-owned, but this is not a cleaner.
        let foreign = report.schema_dir.join("notes.md");
        fs::write(&foreign, b"mine\n")?;
        // Never followed, never unlinked - a link is not a schema we wrote.
        #[cfg(unix)]
        let linked = {
            let linked = report.schema_dir.join("linked.schema.json");
            std::os::unix::fs::symlink(&foreign, &linked)?;
            linked
        };

        let second = generate_command(project.path())?;
        assert_eq!(second.removed, vec!["retired.schema.json".to_string()]);
        assert!(second.unremovable.is_empty());
        assert!(!retired.exists(), "a retired schema must be removed");
        assert!(foreign.exists(), "a foreign file must be left alone");
        #[cfg(unix)]
        assert!(
            fs::symlink_metadata(&linked).is_ok(),
            "a symlink must be left alone"
        );
        for kind in DocumentKind::ALL {
            assert!(report.schema_dir.join(kind.file_name()).is_file());
        }
        Ok(())
    }

    #[test]
    fn a_delivered_schema_is_never_pruned_because_its_name_is_spelled_differently() -> Result<()> {
        let project = project()?;
        let schema_dir = project.path().join(SCHEMA_DIR_RELATIVE);
        fs::create_dir_all(&schema_dir)?;
        if !folds_case(&schema_dir)? {
            // On a case-sensitive volume the two names are genuinely different
            // files, and pruning the variant is the intended behavior.
            return Ok(());
        }
        // A rename onto an existing entry keeps that entry's stored casing, so
        // after generation this name *is* the delivered robot schema.
        fs::write(schema_dir.join("Robot.schema.json"), b"{}\n")?;

        let report = generate_command(project.path())?;
        assert!(
            report.removed.is_empty(),
            "a delivered schema must never be removed: {:?}",
            report.removed
        );
        assert_eq!(entries(&report.schema_dir)?.len(), DocumentKind::ALL.len());
        for kind in DocumentKind::ALL {
            let path = report.schema_dir.join(kind.file_name());
            let value = serde_json::from_slice::<serde_json::Value>(&fs::read(&path)?)?;
            assert_eq!(
                value.get("$schema").and_then(serde_json::Value::as_str),
                Some(DRAFT_2020_12),
                "{} must hold this run's schema",
                kind.file_name()
            );
        }
        Ok(())
    }

    #[test]
    fn concurrent_runs_all_succeed_while_pruning_the_same_retired_schemas() -> Result<()> {
        let project = project()?;
        let report = generate_command(project.path())?;
        for index in 0..12 {
            fs::write(
                report
                    .schema_dir
                    .join(format!("retired{index}.schema.json")),
                b"{}\n",
            )?;
        }
        let root = project.path().to_path_buf();
        let outcomes = std::thread::scope(|scope| {
            let handles = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        generate_command(&root)
                            .map(|_| ())
                            .map_err(|error| format!("{error:#}"))
                    })
                })
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .filter_map(|handle| handle.join().unwrap_or(Ok(())).err())
                .collect::<Vec<_>>()
        });
        // Losing a prune race is another run reaching the same goal state, not
        // a failure of work that already succeeded.
        assert!(outcomes.is_empty(), "every run must succeed: {outcomes:?}");
        let mut expected = DocumentKind::ALL
            .map(DocumentKind::file_name)
            .map(str::to_owned)
            .to_vec();
        expected.sort();
        assert_eq!(entries(&report.schema_dir)?, expected);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn an_unremovable_retired_schema_is_reported_rather_than_failing_a_finished_run() -> Result<()>
    {
        use std::os::unix::fs::PermissionsExt;

        let project = project()?;
        let generated = generate_schemas(project.path())?;
        write_schemas(&generated)?;
        let retired = generated.schema_dir.join("retired.schema.json");
        fs::write(&retired, b"{}\n")?;
        // A directory without write permission refuses to unlink its entries.
        fs::set_permissions(&generated.schema_dir, PermissionsExt::from_mode(0o555))?;
        let (removed, unremovable) = remove_retired_schemas(&generated);
        fs::set_permissions(&generated.schema_dir, PermissionsExt::from_mode(0o755))?;

        assert!(removed.is_empty());
        assert_eq!(unremovable, vec!["retired.schema.json".to_string()]);
        // The command's actual contract - the delivered schemas - still holds.
        for (path, bytes) in &generated.files {
            assert_eq!(&fs::read(path)?, bytes);
        }
        Ok(())
    }

    #[test]
    fn a_discovery_failure_writes_nothing() -> Result<()> {
        // `generate_schemas` has no write path at all, so no partial state can
        // exist before a document is verified; discovery is its first failure.
        let empty = tempfile::tempdir()?;
        let error = generate_schemas(empty.path()).expect_err("no robot.yaml exists");
        assert!(format!("{error:#}").contains("robot.yaml"));
        assert!(!empty.path().join(".phoxal").exists());
        Ok(())
    }

    #[test]
    fn an_unusable_schema_directory_names_the_exact_path_and_writes_no_schema() -> Result<()> {
        let project = project()?;
        let generated = generate_schemas(project.path())?;
        fs::create_dir_all(project.path().join(".phoxal"))?;
        fs::write(&generated.schema_dir, b"not a directory")?;
        let error = write_schemas(&generated).expect_err("schema directory is a file");
        assert!(
            format!("{error:#}").contains(&generated.schema_dir.display().to_string()),
            "error must name the exact path: {error:#}"
        );
        assert!(generated.schema_dir.is_file(), "nothing may be written");
        Ok(())
    }

    #[test]
    fn a_failure_midway_keeps_earlier_files_valid_and_leaves_no_temporary() -> Result<()> {
        let project = project()?;
        let generated = generate_schemas(project.path())?;
        fs::create_dir_all(&generated.schema_dir)?;
        // The last delivered file cannot be replaced by a rename.
        let blocked = generated
            .files
            .last()
            .expect("the set is not empty")
            .0
            .clone();
        fs::create_dir(&blocked)?;

        let error = write_schemas(&generated).expect_err("one destination is a directory");
        assert!(format!("{error:#}").contains(&blocked.display().to_string()));
        for (path, bytes) in &generated.files {
            if path == &blocked {
                continue;
            }
            assert_eq!(&fs::read(path)?, bytes, "earlier files must be complete");
        }
        let leftovers = entries(&generated.schema_dir)?
            .into_iter()
            .filter(|name| !name.ends_with(SCHEMA_FILE_SUFFIX))
            .collect::<Vec<_>>();
        assert!(
            leftovers.is_empty(),
            "no temporary may survive: {leftovers:?}"
        );

        // Rerunning after clearing the blocker repairs the set.
        fs::remove_dir(&blocked)?;
        generate_command(project.path())?;
        for (path, bytes) in &generated.files {
            assert_eq!(&fs::read(path)?, bytes);
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_schema_directory_is_refused() -> Result<()> {
        let project = project()?;
        let elsewhere = tempfile::tempdir()?;
        let generated = generate_schemas(project.path())?;
        fs::create_dir_all(project.path().join(".phoxal"))?;
        std::os::unix::fs::symlink(elsewhere.path(), &generated.schema_dir)?;
        let error = write_schemas(&generated).expect_err("symlinked output is refused");
        assert!(format!("{error:#}").contains("must not be a symlink"));
        assert!(!elsewhere.path().join("robot.schema.json").exists());
        Ok(())
    }

    #[test]
    fn generation_never_edits_authored_yaml_or_an_existing_association() -> Result<()> {
        let project = project()?;
        // A pre-existing association - even a wrong one - is the user's to fix.
        let stale_association = "# $schema: ./old/robot.json\n";
        let robot = project.path().join("robot.yaml");
        fs::write(&robot, format!("{stale_association}{ROBOT_YAML}"))?;
        let component = project.path().join("components/wheel/component.yaml");
        fs::write(&component, "schema: phoxal/component/v0\n")?;

        generate_command(project.path())?;
        assert_eq!(
            fs::read_to_string(&robot)?,
            format!("{stale_association}{ROBOT_YAML}")
        );
        assert_eq!(
            fs::read_to_string(&component)?,
            "schema: phoxal/component/v0\n"
        );
        assert!(!project.path().join(".idea").exists());
        assert!(!project.path().join(".vscode").exists());
        Ok(())
    }

    #[test]
    fn generation_requires_no_cargo_registry_resident_or_build_lock_state() -> Result<()> {
        let project = project()?;
        generate_command(project.path())?;
        assert!(!project.path().join("target").exists());
        // The schema directory is the only project state this command creates,
        // which also rules out a lock, a socket, and a package cache.
        let state = phoxal_cli_core::runtime::paths::RuntimePaths::for_root(project.path());
        assert_eq!(entries(&state.state_root)?, vec!["schemas".to_string()]);
        Ok(())
    }

    #[test]
    fn the_schema_directory_is_a_sibling_of_the_staged_bundle_root() -> Result<()> {
        // `build.phoxal` archives the staged layout root and nothing above it
        // (`crates/project/src/bundle/archive.rs`), so keeping the schema
        // directory beside that root - never inside it - is what keeps
        // generated schemas out of every build archive.
        let project = project()?;
        let generated = generate_schemas(project.path())?;
        let staged = project
            .path()
            .join(phoxal_cli_core::project::launch_plan::RUNTIME_BUNDLE_ROOT_RELATIVE);
        assert_eq!(generated.schema_dir.parent(), staged.parent());
        assert_ne!(generated.schema_dir, staged);
        Ok(())
    }
}
