# Device preparation and deployment

Phoxal installs compiled runtime roots; it does not provision robot-specific
hardware. Prepare each Linux device explicitly, then use the same installer
whether the archive was built locally or from a source snapshot on the device.

## Generic host contract

`sudo phoxal service install` creates or verifies:

- the system account and group `phoxal`;
- the engineering access group `phoxal-engineering`;
- root-owned `/var/lib/phoxal/releases` at mode `0755` for immutable runtime
  releases;
- `/var/lib/phoxal/state` owned by `phoxal:phoxal-engineering` at setgid mode
  `2775` for plan content, with `project.lock` owned by the same identity at
  mode `0660`;
- `/run/phoxal` owned by `phoxal:phoxal-engineering` at setgid mode `2775` for
  reboot-ephemeral supervisor and Zenoh sockets; and
- exactly one `/etc/systemd/system/phoxal.service`.

The unit runs as `phoxal:phoxal-engineering`, starts
`/usr/local/bin/phoxal start /var/phoxal`, uses `Type=notify`, and makes the
resident CLI supervisor the sole readiness and watchdog authority. Participant
children receive no systemd notification environment.

On a host upgraded from the retired `/opt/phoxal` deployment, `service install`
first disables and removes systemd wiring for the legacy `phoxal.target`,
`phoxal-router.service`, and `phoxal-participant-*.service` units. It removes
only unit symlinks whose resolved targets are under `/opt/phoxal`; same-named
foreign units are reported and left untouched. Legacy runtime data under
`/opt/phoxal` is preserved for the administrator to remove explicitly after
the new installation is verified.

The equivalent manual preparation is:

```sh
sudo groupadd --system phoxal
sudo groupadd phoxal-engineering
sudo useradd --system --gid phoxal --groups phoxal-engineering \
  --home-dir /var/lib/phoxal --shell /usr/sbin/nologin phoxal
sudo install -d -m 0755 /var/lib/phoxal/releases
sudo install -d -o phoxal -g phoxal-engineering -m 2775 \
  /var/lib/phoxal/state /run/phoxal
sudo touch /var/lib/phoxal/state/project.lock
sudo chown phoxal:phoxal-engineering /var/lib/phoxal/state/project.lock
sudo chmod 0660 /var/lib/phoxal/state/project.lock
sudo phoxal service install
sudo systemctl daemon-reload
sudo systemctl enable phoxal.service
```

Add engineering users to `phoxal-engineering` only when they should operate
the runtime. Configure SPI, I²C, CAN, GPU, camera, udev rules, and any
supplementary hardware groups separately for the robot. `deploy` never changes
those settings.

Verify preparation with:

```sh
phoxal service status
phoxal doctor
```

`sudo phoxal service uninstall` disables and removes only the managed unit. It
deliberately preserves releases, state, accounts, group membership, and hardware
configuration so uninstall cannot erase robot data.

The equivalent manual unit removal is:

```sh
sudo systemctl disable --now phoxal.service
sudo rm /etc/systemd/system/phoxal.service
sudo systemctl daemon-reload
```

Accounts, groups, `/var/lib/phoxal/releases`, and `/var/lib/phoxal/state` remain
until an administrator deliberately removes them; the uninstall command never
turns that data deletion into an implicit side effect.

## Installed runtime layout

```text
/var/lib/phoxal/
├── releases/
│   └── 20260725T012345.678Z-deadbeef/
│       ├── phoxal.runtime.json
│       ├── robot.yaml
│       ├── bin/
│       └── runtime assets
└── state/
    ├── project.lock
    └── plans/content/

/var/phoxal -> /var/lib/phoxal/releases/20260725T012345.678Z-deadbeef
/run/phoxal/
├── supervisor.sock
└── zenoh.sock
```

There is no `current/`, `installed.json`, `previous.json`, `/var/cache/phoxal`,
or robot-user `~/.phoxal`. Release directory names are the rollback index.

## Install and rollback

Install a prebuilt archive locally:

```sh
sudo phoxal install rover.build.phoxal
```

The installer rejects unsafe or oversized archives, validates the strict
`phoxal.runtime/v0` header before typed documents, inspects only binaries the
compiled graph selects, dry-compiles the complete execution plan, fsyncs the
candidate, stops the service, acquires the install lock, and atomically switches
`/var/phoxal`. It waits for supervisor readiness. A normal readiness failure
restores and restarts the previous release.

The header records the locked framework train in `built_with.phoxal` for
provenance only. Compatibility is decided by the schema and five explicit
revision fields, never by comparing that version string.

Roll back to the immediately older release, or name one explicitly:

```sh
sudo phoxal rollback
sudo phoxal rollback --to 20260724T091315.117Z-139be552
```

The release is complete before activation and symlink replacement is atomic, so
a partial release is never active. The v0 format intentionally has no persistent
transaction marker: power loss after activation but before readiness
confirmation cannot be identified automatically on the next boot. Recovery
after that narrow window may require an explicit `phoxal rollback`.

## Deploy

The ordinary source workflow snapshots tracked and untracked non-ignored source
while excluding `.phoxal`, transfers the vendored artifact store as a separate
payload, builds in a remote temporary directory, and invokes the same installer:

```sh
phoxal deploy robot@jetson-nano-orin
phoxal deploy robot@jetson-nano-orin ../my-robot-project
```

The remote host must already have `phoxal`, Cargo, and Rust for this mode. When
the toolchain is missing, the command stops before modifying the installed
runtime and prints the exact prebuilt alternative:

```sh
phoxal build --target aarch64-unknown-linux-gnu
phoxal deploy robot@jetson-nano-orin \
  --build .phoxal/build/aarch64-unknown-linux-gnu.build.phoxal
```

Prebuilt deployment requires neither Cargo nor Git on the robot. Both modes
copy into `/tmp`, call `sudo -n phoxal install`, and remove the temporary
payload after success.
