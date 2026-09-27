//! Own the operating-system group used by a command. This is lifecycle cleanup,
//! not a sandbox: processes which deliberately escape a Unix group are not owned.
//! Call prepare before spawning, then attach immediately to that owned child PID.

pub fn prepare(command: &mut tokio::process::Command) {
    command.kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
}

#[cfg(unix)]
pub struct ProcessTree {
    group: Option<libc::pid_t>,
}

#[cfg(unix)]
impl ProcessTree {
    pub fn attach(pid: u32) -> std::io::Result<Self> {
        let group = i32::try_from(pid)
            .map_err(|_| std::io::Error::other("process ID exceeds the platform range"))?;
        Ok(Self { group: Some(group) })
    }

    pub fn terminate(&mut self) {
        if let Some(group) = self.group.take() {
            // The child starts its own process group (PTY children create a session).
            // A negative PID targets that group, never the host's group.
            unsafe { libc::kill(-group, libc::SIGKILL) };
        }
    }
}

#[cfg(windows)]
pub struct ProcessTree {
    job: Option<std::os::windows::io::OwnedHandle>,
}

#[cfg(windows)]
impl ProcessTree {
    pub fn attach(pid: u32) -> std::io::Result<Self> {
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows_sys::Win32::System::{
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
                SetInformationJobObject,
            },
            Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE},
        };
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            let job = OwnedHandle::from_raw_handle(job);
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                job.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&limits) as u32,
            ) == 0
            {
                return Err(std::io::Error::last_os_error());
            }
            let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
            if process.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            let process = OwnedHandle::from_raw_handle(process);
            if AssignProcessToJobObject(job.as_raw_handle(), process.as_raw_handle()) == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(Self { job: Some(job) })
        }
    }

    pub fn terminate(&mut self) {
        // Closing the sole job handle kills its processes and inherited descendants.
        self.job.take();
    }
}

impl Drop for ProcessTree {
    fn drop(&mut self) {
        self.terminate();
    }
}
