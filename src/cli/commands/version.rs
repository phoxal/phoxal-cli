//! Version reporting.

use anyhow::Result;
use clap::Args;

pub fn long_version() -> &'static str {
    static VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    VERSION.get_or_init(|| {
        format!(
            "{} ({}-{})",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })
}

#[derive(Debug, Args)]
pub struct VersionArgs {}

impl VersionArgs {
    pub fn run(&self) -> Result<()> {
        println!("phoxal {}", long_version());
        println!(
            "official packages: cargo install --registry {} at the Cargo.lock-selected framework train",
            phoxal_cli_catalog::REGISTRY_NAME
        );
        println!("registry index: {}", phoxal_cli_catalog::REGISTRY_INDEX);
        Ok(())
    }
}
