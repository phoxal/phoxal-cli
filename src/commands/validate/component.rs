use anyhow::Result;
use async_trait::async_trait;
use clap::Args;

use phoxal_cli_core::AppContext;
use phoxal_cli_core::Command;
use phoxal_cli_core::unit::Unit;

#[derive(Debug, Args)]
pub(crate) struct Component {
    #[arg(help = "Component type directory name under components/<component-type>/")]
    pub(crate) component: String,
}

#[async_trait(?Send)]
impl Command for Component {
    async fn execute(&self, app: &AppContext) -> Result<()> {
        phoxal_cli_core::unit::validate::component::Component::new(self.component.clone())
            .run(app)?;
        app.ui
            .success(format!("Component '{}' is valid", self.component));
        Ok(())
    }
}
