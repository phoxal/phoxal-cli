use anyhow::anyhow;

pub fn error(capability: &str) -> anyhow::Error {
    anyhow!(
        "the Docker distribution path was removed (framework #127): official artifacts use native release assets, but {capability} is not available in the current native distribution"
    )
}
