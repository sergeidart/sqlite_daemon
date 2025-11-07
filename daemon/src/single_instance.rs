use anyhow::{bail, Result};
use tracing::info;

#[cfg(windows)]
use windows::Win32::Foundation::{CloseHandle, HANDLE};
#[cfg(windows)]
use windows::Win32::System::Threading::CreateMutexW;
#[cfg(windows)]
use windows::core::PCWSTR;

#[cfg(unix)]
use std::fs::{File, OpenOptions};
#[cfg(unix)]
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

pub struct SingleInstanceGuard {
    #[cfg(windows)]
    _mutex: HANDLE,
    #[cfg(unix)]
    _lock_file: File,
}

impl SingleInstanceGuard {
    /// Try to acquire single instance lock. Returns error if another instance is running.
    pub fn try_acquire() -> Result<Self> {
        #[cfg(windows)]
        {
            Self::try_acquire_windows()
        }
        
        #[cfg(unix)]
        {
            Self::try_acquire_unix()
        }
    }
    
    #[cfg(windows)]
    fn try_acquire_windows() -> Result<Self> {
        use windows::Win32::Foundation::ERROR_ALREADY_EXISTS;
        
        let mutex_name = "Global\\SkylineDBd-v1-SingleInstance\0"
            .encode_utf16()
            .collect::<Vec<u16>>();
        
        unsafe {
            let mutex = CreateMutexW(
                None,
                true, // bInitialOwner - we want to own it
                PCWSTR(mutex_name.as_ptr()),
            )?;
            
            // Check if mutex already existed
            let last_error = windows::Win32::Foundation::GetLastError();
            if last_error == ERROR_ALREADY_EXISTS {
                CloseHandle(mutex)?;
                bail!(
                    "Another daemon instance is already running!\n\
                    Only one daemon instance is allowed at a time.\n\
                    \n\
                    To check if daemon is running: skylinedb-cli.exe ping\n\
                    To stop existing daemon: skylinedb-cli.exe shutdown"
                );
            }
            
            info!("Acquired single-instance lock (Windows mutex)");
            Ok(Self { _mutex: mutex })
        }
    }
    
    #[cfg(unix)]
    fn try_acquire_unix() -> Result<Self> {
        use std::os::unix::io::AsRawFd;
        
        let lock_path = "/var/run/skylinedb-v1.lock";
        
        // Try to create lock file with exclusive access
        let lock_file = OpenOptions::new()
            .create(true)
            .write(true)
            .mode(0o644)
            .open(lock_path)?;
        
        // Try to acquire exclusive lock (non-blocking)
        let fd = lock_file.as_raw_fd();
        let result = unsafe {
            libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB)
        };
        
        if result != 0 {
            bail!(
                "Another daemon instance is already running!\n\
                Only one daemon instance is allowed at a time.\n\
                \n\
                Lock file: {}\n\
                \n\
                To check if daemon is running: skylinedb-cli ping\n\
                To stop existing daemon: skylinedb-cli shutdown",
                lock_path
            );
        }
        
        // Write PID to lock file for debugging
        let pid = std::process::id();
        let mut file = &lock_file;
        write!(file, "{}", pid)?;
        
        info!(lock_file = %lock_path, pid = pid, "Acquired single-instance lock (Unix flock)");
        Ok(Self { _lock_file: lock_file })
    }
}

impl Drop for SingleInstanceGuard {
    fn drop(&mut self) {
        #[cfg(windows)]
        {
            unsafe {
                let _ = CloseHandle(self._mutex);
            }
            info!("Released single-instance lock (Windows mutex)");
        }
        
        #[cfg(unix)]
        {
            // Lock is automatically released when file is closed
            let _ = std::fs::remove_file("/var/run/skylinedb-v1.lock");
            info!("Released single-instance lock (Unix flock)");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_single_instance() {
        // First instance should succeed
        let _guard1 = SingleInstanceGuard::try_acquire().expect("First instance should succeed");
        
        // Second instance should fail
        let result = SingleInstanceGuard::try_acquire();
        assert!(result.is_err(), "Second instance should fail");
        assert!(result.unwrap_err().to_string().contains("already running"));
        
        // After dropping first guard, second should succeed
        drop(_guard1);
        let _guard2 = SingleInstanceGuard::try_acquire().expect("Should succeed after first dropped");
    }
}
