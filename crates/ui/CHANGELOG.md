# Changelog

All notable changes documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/) and the project follows
[Semantic Versioning](https://semver.org/).

## [0.36.1](https://github.com/phoxal/phoxal-cli/releases/tag/v0.36.1) - 2026-08-10


### Added

- *(telemetry)* Add retained robot diagnostics ([#166](https://github.com/phoxal/phoxal-cli/pull/166))
- Select immutable framework suites by Cargo.lock ([#168](https://github.com/phoxal/phoxal-cli/pull/168))
- *(suite)* Consume versioned launch profiles ([#183](https://github.com/phoxal/phoxal-cli/pull/183)) [**breaking**]
- *(runtime)* Adopt Cargo workspace model ([#184](https://github.com/phoxal/phoxal-cli/pull/184))
- Compile source projects into flat runtime roots and build.phoxal (phase 10) ([#186](https://github.com/phoxal/phoxal-cli/pull/186)) [**breaking**]
- *(cli)* Adopt execution-scoped time and command model ([#205](https://github.com/phoxal/phoxal-cli/pull/205)) [**breaking**]
- *(ui)* Rebuild attachment console with tui-realm ([#243](https://github.com/phoxal/phoxal-cli/pull/243))
- Gate dashboard on startup readiness
- Batch runtime preparation and progress ([#271](https://github.com/phoxal/phoxal-cli/pull/271))
- *(ui)* Remove the Bus page and the device and clock feeds ([#285](https://github.com/phoxal/phoxal-cli/pull/285)) [**breaking**]
- Read the joypad locally and delete the tool concept ([#290](https://github.com/phoxal/phoxal-cli/pull/290)) [**breaking**]
- Build, stage, and launch the mandatory root brain; drop behavior handling ([#293](https://github.com/phoxal/phoxal-cli/pull/293)) [**breaking**]
- Split phoxal from phoxald around finalized bundles and execution-scoped Zenoh ([#297](https://github.com/phoxal/phoxal-cli/pull/297)) [**breaking**]

### Fixed

- Harden attachment lifecycle resilience ([#247](https://github.com/phoxal/phoxal-cli/pull/247))
- Carry the resident's real failure reason to the client terminal ([#252](https://github.com/phoxal/phoxal-cli/pull/252))
- Restore private workspace releases ([#301](https://github.com/phoxal/phoxal-cli/pull/301))

### Refactored

- *(cli)* Establish core and UI crate boundaries ([#161](https://github.com/phoxal/phoxal-cli/pull/161))
- Complete CLI crate reorganization ([#162](https://github.com/phoxal/phoxal-cli/pull/162))
- *(supervisor)* Consume Zenoh Liveliness
- Harden supervisor state and readiness
- *(simulation)* Simplify Webots source runs ([#203](https://github.com/phoxal/phoxal-cli/pull/203))
- Drop every API-coherence consumer ([#214](https://github.com/phoxal/phoxal-cli/pull/214)) [**breaking**]
- Delete dead code ([#217](https://github.com/phoxal/phoxal-cli/pull/217))
- Materialize official runtimes through Cargo ([#219](https://github.com/phoxal/phoxal-cli/pull/219)) [**breaking**]
- Establish layered CLI ownership boundaries
- *(client)* Decompose attachment runtime
- *(cli)* Complete layered boundary cleanup ([#245](https://github.com/phoxal/phoxal-cli/pull/245))
- Consume canonical framework model ([#254](https://github.com/phoxal/phoxal-cli/pull/254))
- *(cli)* Remove obsolete executable plan store [**breaking**]
- *(supervisor)* Run the Zenoh router inside the supervisor process ([#279](https://github.com/phoxal/phoxal-cli/pull/279)) [**breaking**]
- Reconcile CLI runtime ownership ([#299](https://github.com/phoxal/phoxal-cli/pull/299))

### Tests

- Keep repository coverage to unit contracts ([#212](https://github.com/phoxal/phoxal-cli/pull/212))

