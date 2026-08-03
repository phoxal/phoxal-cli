//! A real terminal for the real binary.
//!
//! The TUI is only itself in a terminal: it queries the size, enters the
//! alternate screen, and paints with escape sequences. Anything that captures
//! its stdout as a pipe measures a different program. So this opens a pty,
//! runs the shipped `phoxal` binary inside it, and feeds the bytes it emits
//! through a terminal emulator - the screen this asserts on is the screen a
//! human would see.

use std::io::{Read, Write};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

/// One terminal geometry under test. The size is a parameter here so the
/// terminal matrix is something a scenario iterates, not a chore a human
/// repeats by dragging a window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalSize {
    pub cols: u16,
    pub rows: u16,
}

impl TerminalSize {
    pub const fn new(cols: u16, rows: u16) -> Self {
        Self { cols, rows }
    }

    /// The label used in snapshot filenames.
    pub fn label(self) -> String {
        format!("{}x{}", self.cols, self.rows)
    }
}

impl std::fmt::Display for TerminalSize {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}x{}", self.cols, self.rows)
    }
}

/// A `phoxal` process running in its own terminal.
pub struct Session {
    /// Held, never read: dropping the master closes the terminal and hangs up
    /// on the child. Its lifetime IS its purpose.
    #[expect(dead_code, reason = "the master's Drop is what keeps the pty open")]
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    output: Receiver<Vec<u8>>,
    parser: vt100::Parser,
    size: TerminalSize,
}

impl Session {
    /// Launch `program` with `args` in a terminal of `size`, from `cwd`.
    pub fn spawn(
        program: &std::path::Path,
        args: &[String],
        cwd: &std::path::Path,
        size: TerminalSize,
    ) -> Result<Self> {
        let pty = native_pty_system()
            .openpty(PtySize {
                rows: size.rows,
                cols: size.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("failed to open a pty")?;

        let mut command = CommandBuilder::new(program);
        command.args(args);
        command.cwd(cwd);
        // A TUI reads these to decide colour depth and whether to degrade.
        // Pin them so a snapshot does not depend on the developer's shell.
        command.env("TERM", "xterm-256color");
        command.env("NO_COLOR", "1");
        command.env("COLUMNS", size.cols.to_string());
        command.env("LINES", size.rows.to_string());

        let child = pty
            .slave
            .spawn_command(command)
            .with_context(|| format!("failed to spawn {}", program.display()))?;
        // The slave handle must close here or the reader never sees EOF after
        // the child exits.
        drop(pty.slave);

        let writer = pty.master.take_writer().context("pty has no writer")?;
        let mut reader = pty.master.try_clone_reader().context("pty has no reader")?;
        let (tx, output) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buffer = [0u8; 8192];
            while let Ok(read) = reader.read(&mut buffer) {
                if read == 0 || tx.send(buffer[..read].to_vec()).is_err() {
                    break;
                }
            }
        });

        Ok(Self {
            master: pty.master,
            child,
            writer,
            output,
            parser: vt100::Parser::new(size.rows, size.cols, 0),
            size,
        })
    }

    /// Drain whatever the child has emitted, up to `quiet` of silence.
    ///
    /// A TUI paints in bursts, so "no bytes for a moment" is the only honest
    /// signal that a frame is finished. `budget` bounds the whole wait so a
    /// child that never stops talking cannot hang the harness.
    pub fn settle(&mut self, quiet: Duration, budget: Duration) -> Result<()> {
        let deadline = Instant::now() + budget;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(());
            }
            match self.output.recv_timeout(quiet.min(remaining)) {
                Ok(bytes) => self.parser.process(&bytes),
                // Silence: the frame is as finished as it is going to get.
                Err(RecvTimeoutError::Timeout) => return Ok(()),
                // The child closed its terminal; whatever it painted stands.
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            }
        }
    }

    /// Send raw bytes as if typed. Keys are literal so a scenario reads like
    /// the keystrokes a human would make.
    pub fn send(&mut self, keys: &str) -> Result<()> {
        self.writer
            .write_all(keys.as_bytes())
            .context("failed to write to the pty")?;
        self.writer.flush().context("failed to flush the pty")
    }

    /// The visible screen, trailing blanks trimmed so a snapshot diff shows
    /// content rather than padding.
    pub fn screen(&self) -> String {
        let screen = self.parser.screen();
        let mut lines = (0..screen.size().0)
            .map(|row| {
                let mut line = String::new();
                for col in 0..screen.size().1 {
                    line.push_str(screen.cell(row, col).map_or(" ", |cell| {
                        let contents = cell.contents();
                        if contents.is_empty() { " " } else { contents }
                    }));
                }
                line.trim_end().to_string()
            })
            .collect::<Vec<_>>();
        while lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        lines.join("\n")
    }

    /// Whether the visible screen contains `needle`.
    pub fn screen_contains(&self, needle: &str) -> bool {
        self.screen().contains(needle)
    }

    /// Wait until the screen contains `needle`, or fail with the screen that
    /// was actually rendered - a bare timeout says nothing about why.
    pub fn wait_for(&mut self, needle: &str, budget: Duration) -> Result<()> {
        let deadline = Instant::now() + budget;
        while Instant::now() < deadline {
            self.settle(Duration::from_millis(150), Duration::from_millis(500))?;
            if self.screen_contains(needle) {
                return Ok(());
            }
        }
        bail!(
            "timed out after {budget:?} waiting for {needle:?} at {}.\n\
             --- screen ---\n{}\n--------------",
            self.size,
            self.screen()
        )
    }

    /// Ask the program to quit, then stop waiting politely.
    pub fn shutdown(mut self) -> Result<()> {
        let _ = self.send("q");
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if self.child.try_wait()?.is_some() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        self.child.kill().context("failed to kill the child")?;
        Ok(())
    }
}
