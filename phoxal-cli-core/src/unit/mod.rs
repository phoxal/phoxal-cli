use crate::AppContext;

pub mod bundle;
pub mod compose;
pub mod container;
pub mod doctor;
pub mod inventory;
pub mod publish;
pub mod robot;
pub mod runtime_catalog;
pub mod source_graph;
pub mod stream_demand;
pub mod validate;

pub trait Unit {
    type Output;
    fn name(&self) -> &'static str;
    fn execute(&self, app: &AppContext) -> anyhow::Result<Self::Output>;

    fn run(&self, app: &AppContext) -> anyhow::Result<Self::Output> {
        app.ui.step(self.name(), || self.execute(app))
    }
}
