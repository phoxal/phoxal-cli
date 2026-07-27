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

pub fn stage_world_source_with_protos(
    source_world: &str,
    externprotos: &[ExternProto],
    root_nodes: &[AstNode],
) -> Result<String> {
    let mut document: Proto = source_world
        .parse()
        .context("failed to parse source Webots world")?;
    document.externprotos.extend(externprotos.iter().cloned());
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

pub fn stage_world_source_with_text_fallback(
    source: &str,
    externprotos: &[ExternProto],
    root_nodes: &[AstNode],
) -> Result<String> {
    let externproto_document = Proto {
        header: None,
        externprotos: externprotos.to_vec(),
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
