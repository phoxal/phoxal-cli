//! Advisories a staging pass surfaces once, from the one shared path.

/// Surface workspace runtime crates that are present but not declared in
/// robot.yaml: legal drift, not built or launched. One advisory line
/// naming each crate and the map that would declare it, so authors notice a
/// service they forgot to declare. No output when there is no drift.
pub(crate) fn report_undeclared_runtimes(
    undeclared: &[crate::source::resolver::UndeclaredRuntime],
    ui: &dyn crate::Reporter,
) {
    if undeclared.is_empty() {
        return;
    }
    let list = undeclared
        .iter()
        .map(|runtime| {
            format!(
                "{family}/{name} (declare it under `{map}:` in robot.yaml to run it)",
                family = runtime.family,
                name = runtime.name,
                map = runtime.family
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    ui.warn(format!("undeclared workspace runtimes, not built: {list}"));
}
