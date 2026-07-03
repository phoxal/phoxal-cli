use std::time::Duration;

use anyhow::Result;
use clap::Args;
use tokio::time::MissedTickBehavior;

use crate::AppContext;
use crate::supervisor::{read_supervisor_state, supervisor_state_path};

#[derive(Debug, Args)]
pub struct Status {
    #[arg(long, help = "Keep printing the supervisor status snapshot.")]
    pub watch: bool,
}

impl Status {
    pub async fn run(&self, _app: &AppContext) -> Result<()> {
        let path = supervisor_state_path()?;
        if !self.watch {
            let snapshot = read_supervisor_state(&path)?;
            print!("{}", snapshot.render());
            return Ok(());
        }

        let mut ticker = tokio::time::interval(Duration::from_secs(1));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            match read_supervisor_state(&path) {
                Ok(snapshot) => print!("{}", snapshot.render()),
                Err(error) => eprintln!("{error:#}"),
            }
        }
    }
}
