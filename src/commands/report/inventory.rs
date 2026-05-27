use anyhow::Result;
use async_trait::async_trait;
use clap::Args;
use serde::Serialize;

use phoxal_cli_core::AppContext;
use phoxal_cli_core::Command;
use phoxal_cli_core::unit::inventory::{InventoryKind, inventory};

#[derive(Debug, Args)]
pub(crate) struct Inventory;

#[derive(Debug, Serialize)]
struct InventoryReport {
    topics: Vec<InventoryRow>,
    queries: Vec<InventoryRow>,
    commands: Vec<InventoryRow>,
}

#[derive(Debug, Serialize)]
struct InventoryRow {
    path: String,
    schema: String,
}

#[async_trait(?Send)]
impl Command for Inventory {
    async fn execute(&self, _app: &AppContext) -> Result<()> {
        let mut report = InventoryReport {
            topics: Vec::new(),
            queries: Vec::new(),
            commands: Vec::new(),
        };
        for entry in inventory() {
            let row = InventoryRow {
                path: entry.path,
                schema: entry.schema,
            };
            match entry.kind {
                InventoryKind::Topic => report.topics.push(row),
                InventoryKind::Query => report.queries.push(row),
                InventoryKind::Command => report.commands.push(row),
            }
        }
        println!("{}", serde_yaml::to_string(&report)?);
        Ok(())
    }
}
