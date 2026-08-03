# xtask - TUI harness

Drives the real `phoxal` binary in a real terminal so the TUI can be checked
without a human squinting at it.

```bash
cargo build --release                     # the harness drives the release build

cargo xtask tui list                      # what it knows how to run
cargo xtask tui show    --scenario attach --project ../robot-rover
cargo xtask tui screens --scenario attach --project ../robot-rover
cargo xtask tui screens --scenario attach --project ../robot-rover --bless
```

`show` prints one screen at one size - the "what does it look like right now"
verb. `screens` renders the scenario across the terminal matrix and compares
each screen to a recorded snapshot under `xtask/screens/<scenario>/`; `--bless`
accepts a change.

## Why this is not `cargo test`

Repository CI is for deterministic unit and compile-contract tests, and the
organization's AI assistant guide is explicit that CLI-binary and process
harnesses do not go there. That rule is right: a suite that spawns processes
goes flaky and then gets ignored.

The same guide *requires* that behaviour needing built artifacts is run on the
host and its outcome recorded as PR evidence. This is the tool for that run.

It contains **no `#[test]`s at all**, so nothing here can execute under `cargo
test` - the harness runs only when someone invokes `cargo xtask`. (`xtask` is
also kept out of `default-members`, which is what a bare `cargo build` and
`cargo test` honour; `--workspace` overrides that and will compile it, but
compiling is all it does.)

## What a scenario is

A launch, plus the marker that says the first usable frame has arrived. Waiting
on a marker instead of sleeping is what keeps snapshots stable: the screen is
read when the TUI says it is ready, not when a timer guessed.

A scenario that sets `needs_resident` gets one started for it - once for the
whole terminal matrix - and stopped again on the way out, including when the
scenario fails.

## The terminal matrix

`80x24` (the supported minimum), `120x32` (normal), `200x50` (wide), and
`40x12` (deliberately too small). Size is a parameter, so the matrix is
something the harness iterates rather than a chore someone performs by dragging
a window.

## Snapshots

Plain text, one file per size, diffed line by line. A new snapshot is recorded
rather than failed - the first run of a scenario should produce something to
read. A *changed* snapshot fails, because that is a screen someone should look
at before it ships.
