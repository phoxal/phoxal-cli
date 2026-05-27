use std::collections::BTreeSet;

use anyhow::{Context, Result, bail};
use webots_proto::ast::proto::ast::{AstNode, ExternProto};
use webots_proto::{Proto, ProtoExt, Severity};

pub fn validate_proto_document(proto_name: &str, proto: &Proto) -> Result<()> {
    let diagnostics = proto
        .validate()
        .with_context(|| format!("failed to validate generated Webots PROTO '{proto_name}'"))?;
    if diagnostics.has_errors() {
        let errors = diagnostics
            .iter()
            .filter(|diagnostic| matches!(diagnostic.severity, Severity::Error))
            .map(|diagnostic| diagnostic.message.clone())
            .collect::<Vec<_>>();
        bail!(
            "generated Webots PROTO '{}' is invalid: {}",
            proto_name,
            errors.join("; ")
        );
    }
    Ok(())
}

pub fn stage_world_source_with_proto(
    source_world: &str,
    externproto: &ExternProto,
    root_nodes: &[AstNode],
) -> Result<String> {
    let mut document: Proto = source_world
        .parse()
        .context("failed to parse source Webots world")?;
    document.externprotos.push(externproto.clone());
    document.root_nodes.extend(root_nodes.iter().cloned());
    document.source_content = None;

    let staged = document
        .to_canonical_string()
        .context("failed to serialize staged world")
        .map(|content| format!("{content}\n"))?;

    let _: Proto = staged
        .parse()
        .context("failed to parse staged world after serialization")?;

    Ok(staged)
}

pub fn validate_world_contact_materials(
    staged_world: &str,
    referenced_contact_materials: &BTreeSet<String>,
) -> Result<()> {
    if referenced_contact_materials.is_empty() {
        return Ok(());
    }

    let defined_materials = collect_world_contact_materials(staged_world);
    let missing_materials = referenced_contact_materials
        .difference(&defined_materials)
        .cloned()
        .collect::<Vec<_>>();

    if missing_materials.is_empty() {
        return Ok(());
    }

    bail!(
        "staged Webots world is missing contact material definitions for [{}]; defined materials: [{}]",
        missing_materials.join(", "),
        defined_materials.into_iter().collect::<Vec<_>>().join(", ")
    );
}

pub fn stage_world_source_with_text_fallback(
    source: &str,
    externproto: &ExternProto,
    root_nodes: &[AstNode],
) -> Result<String> {
    let externproto_document = Proto {
        header: None,
        externprotos: vec![externproto.clone()],
        proto: None,
        root_nodes: Vec::new(),
        source_path: None,
        source_content: None,
    };
    let externproto_line = externproto_document
        .to_canonical_string()
        .context("failed to serialize EXTERNPROTO declaration for text fallback")?;

    let robot_document = Proto {
        header: None,
        externprotos: Vec::new(),
        proto: None,
        root_nodes: root_nodes.to_vec(),
        source_path: None,
        source_content: None,
    };
    let root_nodes_source = robot_document
        .to_canonical_string()
        .context("failed to serialize root nodes for text fallback")?;

    let trimmed_source = source.trim_end();
    let insertion_index = externproto_insertion_index(trimmed_source);
    let (prefix, suffix) = trimmed_source.split_at(insertion_index);

    let mut staged = String::with_capacity(
        trimmed_source.len() + externproto_line.len() + root_nodes_source.len() + 8,
    );
    staged.push_str(prefix);
    if !prefix.ends_with('\n') {
        staged.push('\n');
    }
    if !prefix.ends_with("\n\n") {
        staged.push('\n');
    }
    staged.push_str(externproto_line.trim_end());
    staged.push_str("\n\n");
    staged.push_str(suffix.trim_start_matches('\n'));
    if !staged.ends_with('\n') {
        staged.push('\n');
    }
    if !staged.ends_with("\n\n") {
        staged.push('\n');
    }
    staged.push_str(root_nodes_source.trim_end());
    staged.push('\n');
    Ok(staged)
}

fn collect_world_contact_materials(world_source: &str) -> BTreeSet<String> {
    ["material1", "material2"]
        .into_iter()
        .flat_map(|field| collect_quoted_field_values(world_source, field))
        .collect()
}

fn collect_quoted_field_values(source: &str, field_name: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut search_start = 0usize;

    while let Some(relative_index) = source[search_start..].find(field_name) {
        let field_index = search_start + relative_index;
        let mut cursor = field_index + field_name.len();
        if !source[field_index..].chars().next().is_some_and(|_| {
            field_index == 0 || !is_identifier_char(source[..field_index].chars().last())
        }) {
            search_start = cursor;
            continue;
        }
        while let Some(ch) = source[cursor..].chars().next() {
            if ch == '"' {
                break;
            }
            cursor += ch.len_utf8();
        }
        let Some(_) = source[cursor..].chars().next().filter(|ch| *ch == '"') else {
            search_start = cursor;
            continue;
        };
        cursor += 1;
        let value_start = cursor;
        while let Some(ch) = source[cursor..].chars().next() {
            if ch == '"' {
                values.push(source[value_start..cursor].to_string());
                cursor += 1;
                break;
            }
            cursor += ch.len_utf8();
        }
        search_start = cursor;
    }

    values
}

fn is_identifier_char(ch: Option<char>) -> bool {
    ch.is_some_and(|value| value.is_ascii_alphanumeric() || value == '_')
}

fn externproto_insertion_index(source: &str) -> usize {
    let mut offset = 0usize;
    let mut seen_header = false;
    let mut after_externproto_block = None;

    for segment in source.split_inclusive('\n') {
        let line = segment.trim_end_matches('\n');
        let trimmed = line.trim();
        let next_offset = offset + segment.len();

        if !seen_header {
            seen_header = true;
            offset = next_offset;
            continue;
        }
        if trimmed.is_empty() {
            offset = next_offset;
            continue;
        }
        if trimmed.starts_with("EXTERNPROTO ") {
            after_externproto_block = Some(next_offset);
            offset = next_offset;
            continue;
        }
        return after_externproto_block.unwrap_or(offset);
    }

    after_externproto_block.unwrap_or(source.len())
}

#[cfg(test)]
mod tests {
    use super::{stage_world_source_with_text_fallback, validate_world_contact_materials};
    use std::collections::BTreeSet;
    use webots_proto::ast::proto::ast::{AstNode, AstNodeKind, ExternProto};
    use webots_proto::ast::proto::span::Span;

    #[test]
    fn stage_world_validation_rejects_missing_contact_material() {
        let world = r#"#VRML_SIM R2025a utf8

WorldInfo {
  contactProperties [
    ContactProperties {
      material1 "rubber_wheel"
      material2 "asphalt"
    }
  ]
}
"#;
        let referenced = BTreeSet::from(["caster_wheel".to_string()]);
        let error = validate_world_contact_materials(world, &referenced)
            .expect_err("missing material should fail validation");
        assert!(error.to_string().contains("caster_wheel"));
    }

    #[test]
    fn text_fallback_inserts_externproto_after_existing_block_and_appends_nodes() {
        let world = r#"#VRML_SIM R2025a utf8

EXTERNPROTO "existing.proto"

WorldInfo {
}
"#;
        let externproto = ExternProto::new("RobotV1.proto".to_string(), None, Span::default());
        let root_node = AstNode::new(
            AstNodeKind::Node {
                type_name: "RobotV1".to_string(),
                def_name: None,
                fields: Vec::new(),
            },
            Span::default(),
        );

        let staged =
            stage_world_source_with_text_fallback(world, &externproto, &[root_node]).unwrap();

        assert!(staged.contains("EXTERNPROTO \"existing.proto\""));
        assert!(staged.contains("EXTERNPROTO \"RobotV1.proto\""));
        assert!(staged.contains("RobotV1 {"));

        let existing_pos = staged.find("EXTERNPROTO \"existing.proto\"").unwrap();
        let inserted_pos = staged.find("EXTERNPROTO \"RobotV1.proto\"").unwrap();
        let world_info_pos = staged.find("WorldInfo {").unwrap();
        let robot_pos = staged.rfind("RobotV1 {").unwrap();

        assert!(existing_pos < inserted_pos);
        assert!(inserted_pos < world_info_pos);
        assert!(robot_pos > world_info_pos);
    }
}
