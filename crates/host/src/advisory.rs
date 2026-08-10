//! Advisory file locking for the CLI pair's supported Unix hosts.
//!
//! Used by the client's project-operation lock for the project-operation lock -
//! Phoxal-owned state (candidate publication, bundle/simulation root
//! replacement, Webots staging) that
//! Cargo's own build-directory locking does not cover.

use std::fs;

pub fn try_advisory_lock(file: &fs::File, exclusive: bool) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    let operation = if exclusive {
        libc::LOCK_EX
    } else {
        libc::LOCK_SH
    } | libc::LOCK_NB;
    if unsafe { libc::flock(file.as_raw_fd(), operation) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

pub fn unlock_advisory(file: &fs::File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}
