# phoxal-cli-core

Terminal-independent project, simulation, session, and check behavior for
`phoxal-cli`.

The root CLI and terminal UI may depend on this crate. This crate must not take
a non-development dependency on Clap and must not depend on terminal rendering
or either consumer crate. Its modules currently own project, simulation,
session, and check behavior. The Clap dev-dependency only verifies the exact
framework-runner argument shape emitted by session launch encoding.
