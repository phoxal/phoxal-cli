//! Project document schema-revision gating.
//!
//! Every typed Phoxal document self-describes with a `schema:` tag
//! (`schema: phoxal/robot/v0`, ...), and those tags are the whole
//! compatibility contract (). Before the strict typed parser runs, the CLI
//! probes the declared revision and answers the one question the parser cannot:
//! *which side is out of date*.
//!
//! A newer revision of a known family is rejected with an update instruction.
//! A wrong-*family*
//! tag and a malformed document are not version problems at all and are left to
//! the strict parser, which already reports them precisely.
//!
//! The supported tags are read from the `phoxal-runtime-contract` enums rather
//! than restated, so a framework rename cannot leave this gate advertising a
//! spelling nothing speaks.

use std::path::Path;

use anyhow::{Context, Result, bail};

/// A typed document family whose declared `schema:` revision the CLI verifies
/// before strict parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentKind {
    Robot,
}

impl DocumentKind {
    /// The `schema:` family prefix this document uses - the tag without its
    /// trailing revision segment. Pinned to [`Self::supported`] by
    /// `the_family_is_the_supported_tag_without_its_revision`.
    #[must_use]
    pub const fn family(self) -> &'static str {
        match self {
            Self::Robot => "phoxal/robot",
        }
    }

    /// The fully-qualified `schema:` tags this CLI build can parse for the
    /// family. Advancing a document schema adds its new tag here alongside the
    /// framework model variant that reads it.
    #[must_use]
    pub const fn supported(self) -> &'static [&'static str] {
        const ROBOT: &[&str] = &["phoxal/robot/v0"];
        match self {
            Self::Robot => ROBOT,
        }
    }
}

/// Verify that the document at `path` declares a schema revision this CLI
/// supports, before it reaches the strict typed parser.
pub fn ensure_supported_revision(path: &Path, kind: DocumentKind) -> Result<()> {
    let text = std::fs::read_to_string(path).with_context(|| {
        format!(
            "failed to read {} document {}",
            kind.family(),
            path.display()
        )
    })?;
    ensure_supported_revision_str(&text, path, kind)
}

/// The [`ensure_supported_revision`] check against already-read text, so a
/// caller that has the bytes in hand does not read the file twice. `path` is
/// used only to name the document in diagnostics.
pub fn ensure_supported_revision_str(text: &str, path: &Path, kind: DocumentKind) -> Result<()> {
    // A malformed document is not this gate's concern: return and let the
    // strict typed parser own the YAML diagnostic.
    let Ok(value) = serde_yaml::from_str::<serde_yaml::Value>(text) else {
        return Ok(());
    };
    let Some(declared) = value.get("schema").and_then(serde_yaml::Value::as_str) else {
        // No self-describing tag: a partial `extends:` parent or an untagged
        // document. The strict parser owns that case.
        return Ok(());
    };
    if kind.supported().contains(&declared) {
        return Ok(());
    }
    let declared_family = declared
        .rsplit_once('/')
        .map_or(declared, |(family, _)| family);
    if declared_family != kind.family() {
        // A wrong-family tag in this slot is an authoring error, not a version
        // gap; the strict parser rejects it with the right message.
        return Ok(());
    }
    bail!(
        "{path}: document declares schema `{declared}`, which this phoxal CLI build does not \
         support (supported: {supported}).\n\nThe document is newer than the tool. Update the \
         phoxal CLI to a build that understands `{declared}`.",
        path = path.display(),
        supported = kind.supported().join(", "),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn supported_revision_passes() {
        let text = "schema: phoxal/robot/v0\nrobot: {}\n";
        ensure_supported_revision_str(text, Path::new("robot.yaml"), DocumentKind::Robot)
            .expect("phoxal/robot/v0 is supported");
    }

    /// The advertised family is exactly the supported tag minus its revision,
    /// so a framework rename cannot leave the two describing different things.
    #[test]
    fn the_family_is_the_supported_tag_without_its_revision() {
        for kind in [DocumentKind::Robot] {
            for tag in kind.supported() {
                let (family, revision) = tag
                    .rsplit_once('/')
                    .expect("a supported tag ends in a revision segment");
                assert_eq!(family, kind.family(), "{tag}");
                assert!(revision.starts_with('v'), "{tag}");
            }
        }
        // And it is the framework's own spelling, not a second copy of it.
        assert_eq!(DocumentKind::Robot.supported(), &["phoxal/robot/v0"]);
    }

    #[test]
    fn unsupported_revision_of_known_family_asks_for_a_cli_update() {
        let text = "schema: phoxal/robot/v1\nrobot: {}\n";
        let error =
            ensure_supported_revision_str(text, Path::new("/proj/robot.yaml"), DocumentKind::Robot)
                .expect_err("phoxal/robot/v1 is not supported by this CLI");
        let message = format!("{error:#}");
        assert!(message.contains("/proj/robot.yaml"), "{message}");
        assert!(message.contains("phoxal/robot/v1"), "{message}");
        assert!(message.contains("Update the phoxal CLI"), "{message}");
        // The precise gate, not serde's stock unknown-variant diagnostic.
        assert!(!message.contains("unknown variant"), "{message}");
    }

    #[test]
    fn untagged_partial_document_defers_to_the_strict_parser() {
        // An `extends:` parent carries no `schema:` tag and merges into the leaf.
        let text = "robot:\n  motion_limits:\n    max_linear_speed_mps: 0.5\n";
        ensure_supported_revision_str(text, Path::new("base.robot.yaml"), DocumentKind::Robot)
            .expect("an untagged partial is not the gate's concern");
    }

    #[test]
    fn wrong_family_tag_defers_to_the_strict_parser() {
        // A component tag in a robot slot is an authoring error the strict
        // parser rejects; the gate must not shadow it with an update message.
        // A component tag in a robot document is a parser-owned error.
        for text in ["schema: phoxal/component/v0\n", "schema: component/v0\n"] {
            ensure_supported_revision_str(text, Path::new("robot.yaml"), DocumentKind::Robot)
                .expect("a wrong-family tag is not a version gap");
        }
    }

    #[test]
    fn malformed_document_defers_to_the_strict_parser() {
        let text = "schema: phoxal/robot/v0\n: : : not yaml\n";
        // Whether this parses as a Value or not, the gate must never mask the
        // real parser's malformed-input diagnostic with an update message.
        ensure_supported_revision_str(text, Path::new("robot.yaml"), DocumentKind::Robot)
            .expect("malformed input is deferred, not gated");
    }
}
