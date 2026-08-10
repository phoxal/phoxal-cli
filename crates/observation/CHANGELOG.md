# Changelog

All notable changes documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/) and the project follows
[Semantic Versioning](https://semver.org/).

## [0.36.1](https://github.com/phoxal/phoxal-cli/releases/tag/v0.36.1) - 2026-08-10


### Added

- *(ui)* Rebuild attachment console with tui-realm ([#243](https://github.com/phoxal/phoxal-cli/pull/243))
- *(ui)* Remove the Bus page and the device and clock feeds ([#285](https://github.com/phoxal/phoxal-cli/pull/285)) [**breaking**]
- Read the joypad locally and delete the tool concept ([#290](https://github.com/phoxal/phoxal-cli/pull/290)) [**breaking**]
- Split phoxal from phoxald around finalized bundles and execution-scoped Zenoh ([#297](https://github.com/phoxal/phoxal-cli/pull/297)) [**breaking**]

### Fixed

- Harden attachment lifecycle resilience ([#247](https://github.com/phoxal/phoxal-cli/pull/247))
- Carry the resident's real failure reason to the client terminal ([#252](https://github.com/phoxal/phoxal-cli/pull/252))
- Restore private workspace releases ([#301](https://github.com/phoxal/phoxal-cli/pull/301))

### Refactored

- Establish layered CLI ownership boundaries
- *(supervisor)* Extract resident runtime authority
- *(client)* Decompose attachment runtime
- *(cli)* Complete layered boundary cleanup ([#245](https://github.com/phoxal/phoxal-cli/pull/245))
- *(cli)* Remove obsolete executable plan store [**breaking**]
- Reconcile CLI runtime ownership ([#299](https://github.com/phoxal/phoxal-cli/pull/299))

