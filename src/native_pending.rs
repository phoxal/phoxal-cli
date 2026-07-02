use anyhow::anyhow;

pub fn error(capability: &str) -> anyhow::Error {
    anyhow!(
        "the Docker distribution path was removed (framework #127): official artifacts now ship as native release assets, and {capability} lands with the native distribution work (phoxal/organization tmp/framework-rewrite follow-ups 02/06/03/10)"
    )
}
