use std::time::Duration;

use anyhow::Result;
use clap::{Args, Subcommand};
use tokio::time::MissedTickBehavior;

use crate::AppContext;
use crate::commands::{MessageFormat, print_message};
use crate::supervisor::{
    SupervisorActionRequest, read_supervisor_state, request_supervisor_action,
    supervisor_state_path,
};

#[derive(Debug, Args)]
pub struct Status {
    #[arg(long, help = "Keep printing the supervisor status snapshot.")]
    pub watch: bool,
    #[arg(long, value_enum, default_value_t = MessageFormat::Human)]
    pub message_format: MessageFormat,
    #[command(subcommand)]
    pub command: Option<StatusSubcommand>,
}

#[derive(Debug, Subcommand)]
pub enum StatusSubcommand {
    #[command(about = "Stop a managed child and mark it released for a manual run.")]
    Release(ParticipantArg),
    #[command(about = "Respawn a released participant under supervisor control.")]
    Resume(ParticipantArg),
}

#[derive(Debug, Args)]
pub struct ParticipantArg {
    #[arg(value_name = "PARTICIPANT")]
    pub participant: String,
}

impl Status {
    pub async fn run(&self, app: &AppContext) -> Result<()> {
        if let Some(command) = &self.command {
            return run_action(command, app);
        }
        let path = supervisor_state_path()?;
        if !self.watch {
            let snapshot = read_supervisor_state(&path)?;
            return print_message(
                &snapshot,
                || {
                    print!("{}", snapshot.render());
                    Ok(())
                },
                self.message_format,
            );
        }

        let mut ticker = tokio::time::interval(Duration::from_secs(1));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            match read_supervisor_state(&path) {
                Ok(snapshot) => {
                    print_message(
                        &snapshot,
                        || {
                            print!("{}", snapshot.render());
                            Ok(())
                        },
                        self.message_format,
                    )?;
                }
                Err(error) if self.message_format == MessageFormat::Human => {
                    eprintln!("{error:#}");
                }
                Err(_) => {}
            }
        }
    }
}

fn run_action(command: &StatusSubcommand, app: &AppContext) -> Result<()> {
    let (participant, action, request) = match command {
        StatusSubcommand::Release(arg) => (
            arg.participant.as_str(),
            "release",
            SupervisorActionRequest::Release {
                participant: arg.participant.clone(),
            },
        ),
        StatusSubcommand::Resume(arg) => (
            arg.participant.as_str(),
            "resume",
            SupervisorActionRequest::Resume {
                participant: arg.participant.clone(),
            },
        ),
    };
    request_supervisor_action(request)?;
    app.ui
        .info(format!("queued supervisor {action} for {participant}"));
    Ok(())
}
