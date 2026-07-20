//! Cross-process artifact-store locking.

use anyhow::Context;
use anyhow::Result;
use std::fs;
use std::path::PathBuf;

pub struct ArtifactStoreLock {
    file: fs::File,
    path: PathBuf,
    exclusive: bool,
}

impl ArtifactStoreLock {
    pub fn exclusive(command: &str) -> Result<Self> {
        Self::acquire(true, command)
    }

    pub fn shared() -> Result<Self> {
        Self::acquire(false, "staging")
    }

    fn acquire(exclusive: bool, command: &str) -> Result<Self> {
        let store = crate::host_paths::artifacts_dir()?;
        fs::create_dir_all(&store)?;
        let path = store.join(".lock");
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            const FILE_FLAG_DELETE_ON_CLOSE: u32 = 0x0400_0000;
            options.custom_flags(FILE_FLAG_DELETE_ON_CLOSE);
        }
        let file = options.open(&path)?;
        try_advisory_lock(&file, exclusive)
            .with_context(|| format!("artifact store lock is held (requested by {command})"))?;
        Ok(Self {
            file,
            path,
            exclusive,
        })
    }
}

impl Drop for ArtifactStoreLock {
    fn drop(&mut self) {
        if !self.exclusive {
            let _ = unlock_advisory(&self.file);
            if try_advisory_lock(&self.file, true).is_err() {
                return;
            }
        }
        fs::remove_file(&self.path).ok();
        let _ = unlock_advisory(&self.file);
    }
}

#[cfg(unix)]
pub(crate) fn try_advisory_lock(file: &fs::File, exclusive: bool) -> std::io::Result<()> {
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

#[cfg(unix)]
pub(crate) fn unlock_advisory(file: &fs::File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(windows)]
pub(crate) fn try_advisory_lock(file: &fs::File, exclusive: bool) -> std::io::Result<()> {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;

    const LOCKFILE_FAIL_IMMEDIATELY: u32 = 0x0000_0001;
    const LOCKFILE_EXCLUSIVE_LOCK: u32 = 0x0000_0002;
    let flags = LOCKFILE_FAIL_IMMEDIATELY
        | if exclusive {
            LOCKFILE_EXCLUSIVE_LOCK
        } else {
            0
        };
    let mut overlapped = WindowsOverlapped::default();
    let result = unsafe {
        LockFileEx(
            file.as_raw_handle().cast::<c_void>(),
            flags,
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    };
    if result != 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(windows)]
pub(crate) fn unlock_advisory(file: &fs::File) -> std::io::Result<()> {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;

    let mut overlapped = WindowsOverlapped::default();
    let result = unsafe {
        UnlockFileEx(
            file.as_raw_handle().cast::<c_void>(),
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    };
    if result != 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(windows)]
#[repr(C)]
#[derive(Default)]
pub(crate) struct WindowsOverlapped {
    internal: usize,
    internal_high: usize,
    offset: u32,
    offset_high: u32,
    event: *mut std::ffi::c_void,
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn LockFileEx(
        file: *mut std::ffi::c_void,
        flags: u32,
        reserved: u32,
        bytes_low: u32,
        bytes_high: u32,
        overlapped: *mut WindowsOverlapped,
    ) -> i32;
    fn UnlockFileEx(
        file: *mut std::ffi::c_void,
        reserved: u32,
        bytes_low: u32,
        bytes_high: u32,
        overlapped: *mut WindowsOverlapped,
    ) -> i32;
}
