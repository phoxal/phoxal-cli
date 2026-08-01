//! Thin `phoxal schema` command adapter.

use anyhow::Result;
use clap::{Args, Subcommand};

use crate::cli::AppContext;

/// The exact comments a project commits, printed after generation. Each
/// association sits on its own line so it can be selected and pasted.
const EDITOR_COMMENTS: &str = "  robot.yaml\n    \
     # $schema: ./.phoxal/schemas/robot.schema.json\n  \
     components/<id>/component.yaml\n    \
     # $schema: ../../.phoxal/schemas/component.schema.json\n  \
     components/<id>/simulation.yaml\n    \
     # $schema: ../../.phoxal/schemas/simulation.schema.json";

#[derive(Debug, Args)]
pub struct Schema {
    #[command(subcommand)]
    command: SchemaSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum SchemaSubcommand {
    #[command(
        about = "Generate the authored YAML editor schemas.",
        long_about = "Write the robot, component, and simulation editor schemas to <project>/.phoxal/schemas/.\n\n\
                      The robot project is found from the current directory, or from the global --project-path.\n\n\
                      The schemas give portable YAML completion and inspection in JetBrains IDEs such as RustRover and in current yaml-language-server clients. Each authored file opts in with a comment you commit above its `schema:` key, for example in a root robot.yaml:\n\n  \
                      # $schema: ./.phoxal/schemas/robot.schema.json\n\n\
                      Generation prints the exact comment for every authored file. This command never edits authored YAML and never writes IDE settings. The schemas describe the parser compiled into this CLI, so rerun it after upgrading phoxal.\n\n\
                      Examples:\n  \
                      phoxal schema generate\n  \
                      phoxal --project-path <project> schema generate\n\n\
                      Editor schemas are structural assistance only; `phoxal validate` remains authoritative."
    )]
    Generate,
}

impl Schema {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        match self.command {
            SchemaSubcommand::Generate => {
                let report = crate::application::schema::generate_command(app.project.root())?;
                app.ui.success(format!(
                    "generated {} editor schemas in {}\n\n\
                     Add these comments above `schema:` in the authored files:\n\n\
                     {EDITOR_COMMENTS}\n\n\
                     JetBrains IDEs such as RustRover and current yaml-language-server clients read this comment. \
                     No authored YAML was edited; rerun this command after upgrading phoxal. \
                     Editor schemas are structural assistance only - `phoxal validate` remains authoritative.",
                    report.written,
                    report.schema_dir.display(),
                ));
                if !report.removed.is_empty() {
                    app.ui.info(format!(
                        "removed schemas this phoxal no longer generates: {}",
                        report.removed.join(", ")
                    ));
                }
                if !report.unremovable.is_empty() {
                    app.ui.warn(format!(
                        "could not remove retired schemas in {} - delete them so no `# $schema:` comment keeps resolving to them: {}",
                        report.schema_dir.display(),
                        report.unremovable.join(", ")
                    ));
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::args::{Cli, RootCommand};
    use clap::{CommandFactory, Parser};

    /// The association for a root `robot.yaml`. The `long_about` uses it as its
    /// worked example and [`EDITOR_COMMENTS`] prints it, so it is written twice
    /// in the source; this constant is what keeps the two from drifting.
    const ROBOT_ASSOCIATION: &str = "# $schema: ./.phoxal/schemas/robot.schema.json";

    /// The `long_about` alone - not clap's rendered page, whose auto-generated
    /// `Usage:` and `Options:` blocks would satisfy several of these strings on
    /// their own.
    fn generate_long_about() -> String {
        Cli::command()
            .get_subcommands()
            .find(|command| command.get_name() == "schema")
            .expect("`schema` must be a top-level command")
            .get_subcommands()
            .find(|command| command.get_name() == "generate")
            .expect("`schema generate` must exist")
            .get_long_about()
            .expect("`schema generate` must have long help")
            .to_string()
    }

    #[test]
    fn schema_generate_parses_as_a_subcommand_group() {
        let cli = Cli::try_parse_from(["phoxal", "schema", "generate"]).unwrap();
        let RootCommand::Schema(schema) = cli.command else {
            panic!("expected schema")
        };
        assert!(matches!(schema.command, SchemaSubcommand::Generate));
        // The verb is mandatory: a bare `phoxal schema` is not a command.
        assert!(Cli::try_parse_from(["phoxal", "schema"]).is_err());
        // No per-document flags, output formats, or install lifecycle exist.
        assert!(Cli::try_parse_from(["phoxal", "schema", "generate", "--robot"]).is_err());
        assert!(Cli::try_parse_from(["phoxal", "schema", "install"]).is_err());
    }

    #[test]
    fn help_explains_discovery_output_editors_and_the_validate_boundary() {
        let help = generate_long_about();
        for expected in [
            "or from the global --project-path",
            "<project>/.phoxal/schemas/",
            "robot, component, and simulation editor schemas",
            "RustRover",
            "yaml-language-server",
            "never edits authored YAML",
            "rerun it after upgrading phoxal",
            "phoxal schema generate\n  phoxal --project-path <project> schema generate",
            "`phoxal validate` remains authoritative",
        ] {
            assert!(
                help.contains(expected),
                "long help must contain `{expected}`:\n{help}"
            );
        }
        // The JetBrains-compatible comment is the only documented form.
        assert!(!help.contains("yaml-language-server: $schema="));
    }

    #[test]
    fn help_and_output_document_the_same_robot_association() {
        assert!(generate_long_about().contains(ROBOT_ASSOCIATION));
        assert!(EDITOR_COMMENTS.contains(ROBOT_ASSOCIATION));
    }

    #[test]
    fn the_documented_paths_are_the_ones_the_command_actually_writes() {
        // This prose lives in a different module from the path and the file
        // names it quotes, so nothing but this test stops the help and the
        // success output from confidently naming somewhere nothing is written.
        let help = generate_long_about();
        let directory = crate::application::schema::SCHEMA_DIR_RELATIVE;
        assert!(help.contains(directory), "help must name `{directory}`");
        assert!(
            EDITOR_COMMENTS.contains(directory),
            "printed comments must name `{directory}`"
        );
        for kind in phoxal_manifest::schema::DocumentKind::ALL {
            let name = kind.file_name();
            assert!(
                EDITOR_COMMENTS.contains(name),
                "printed comments must associate `{name}`"
            );
        }
    }

    #[test]
    fn every_printed_association_is_a_selectable_comment_line() {
        let lines = EDITOR_COMMENTS.lines().collect::<Vec<_>>();
        let comments = lines
            .iter()
            .filter(|line| line.trim_start().starts_with("# $schema: "))
            .collect::<Vec<_>>();
        assert_eq!(comments.len(), 3, "one comment per authored document kind");
        for comment in comments {
            assert_eq!(
                comment.trim_start(),
                comment.trim(),
                "a printed comment must be one whole line: {comment:?}"
            );
        }
        for expected in [
            "  robot.yaml",
            "  components/<id>/component.yaml",
            "  components/<id>/simulation.yaml",
            "    # $schema: ../../.phoxal/schemas/component.schema.json",
            "    # $schema: ../../.phoxal/schemas/simulation.schema.json",
        ] {
            assert!(lines.contains(&expected), "missing `{expected}`");
        }
    }
}
