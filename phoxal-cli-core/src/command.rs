use anyhow::Result;
use async_trait::async_trait;

use crate::AppContext;

#[async_trait(?Send)]
pub trait Command {
    async fn execute(&self, app: &AppContext) -> Result<()>;

    async fn run(&self, app: &AppContext) -> Result<()> {
        self.execute(app).await
    }
}
