# phoxal-cli architecture

The workspace has three product crates with one-way dependencies:

```text
phoxal-cli (commands and operating-system adapters)
├── phoxal-cli-core (terminal-independent domain behavior)
└── phoxal-cli-ui   (terminal presentation primitives)
```

`phoxal-cli-core` and `phoxal-cli-ui` do not depend on the root crate or on
each other. The root crate composes them and owns process, network, terminal,
and deployment adapters.

## Root crate

The root package owns clap command parsing, command dispatch, process
supervision, artifact I/O, remote deployment transport, Webots staging, and
the live-session adapter. A command module should parse arguments and
coordinate domain operations; reusable domain rules belong in `core`, while
terminal styling and ratatui adaptation belong in `ui`.

## Core crate

- `project`: normalized project paths and project-local tooling.
- `simulation`: project-local simulation and world resolution.
- `session`: participant roles, launch-contract encoding, and human-readable
  domain formatting.
- `check`: compiled participant-metadata extraction.

Core modules must not import clap command types, root-crate modules, crossterm,
ratatui, or presentation policy.

## UI crate

- `theme`: the terminal palette and color-capability degradation.
- `ratatui`: conversion from semantic theme roles to ratatui styles.

UI modules must not resolve projects, artifacts, deployments, or participant
graphs. They receive already-computed values from the root session adapter.

## Dependency rule

Move behavior toward the crate that owns it; do not add root re-export shims
to preserve old internal paths. When extraction reveals a dependency pointing
from `core` or `ui` back into the root crate, move the data contract down or
keep the operating-system adapter in the root until the dependency is removed.
