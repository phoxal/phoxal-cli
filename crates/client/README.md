# phoxal-cli-client

Project-local client for the resident Phoxal supervisor.

This crate owns `supervisor.sock` connections, bounded framing, role
handshakes, the latest-snapshot watch store, reconnect, and command calls. It
depends only on `phoxal-cli-core`; it does not own process supervision, raw
Zenoh sessions, terminal rendering, or project resolution.
