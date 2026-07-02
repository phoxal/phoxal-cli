use anyhow::Result;
use clap::Args;

use crate::AppContext;
use crate::commands::MessageFormat;

#[derive(Debug, Args)]
pub struct Outdated {
    #[arg(
        long,
        value_enum,
        default_value_t = MessageFormat::Human,
        help = "Output format for the drift report."
    )]
    pub message_format: MessageFormat,
}

impl Outdated {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        let _ = (app, self.message_format);
        Err(crate::native_pending::error(
            "native artifact freshness via the generated catalog (06)",
        ))
    }
}
