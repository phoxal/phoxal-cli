use anyhow::Result;
use clap::Args;

use crate::AppContext;

#[derive(Debug, Args)]
pub struct Doctor {}

impl Doctor {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        crate::host_doctor::report(app);
        let gitignore = app.project.root().join(".gitignore");
        let ignored = std::fs::read_to_string(&gitignore)
            .ok()
            .is_some_and(|contents| contents.lines().any(|line| line.trim() == "/.phoxal/"));
        if !ignored {
            eprintln!(
                "warning: project-local .phoxal/ is not ignored; add this exact line to {}:\n/.phoxal/",
                gitignore.display()
            );
        }
        Ok(())
    }
}
