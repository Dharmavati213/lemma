//! Starting programs: without a console window on Windows, and (on Windows) as a [`Job`] that can
//! be stopped as a whole, the counterpart of a Unix process group.

use std::ffi::OsStr;
use std::process::Command;

/// `Command::new(program)`. On Windows the program gets no console window: Lemma is a GUI
/// program, and every console program it starts (latexmk, git, node, …) would otherwise open a
/// window of its own. Elsewhere this is `Command::new`.
pub fn command(program: impl AsRef<OsStr>) -> Command {
    #[allow(unused_mut)]
    let mut command = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        command.creation_flags(windows::NO_WINDOW);
    }
    command
}

#[cfg(windows)]
pub use windows::{Job, is_running};

#[cfg(windows)]
mod windows {
    use std::io;
    use std::os::windows::io::AsRawHandle as _;
    use std::os::windows::process::CommandExt as _;
    use std::process::{Child, Command};
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, STILL_ACTIVE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
        QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
    };
    use windows_sys::Win32::System::Threading::{
        CREATE_NO_WINDOW, CREATE_SUSPENDED, GetExitCodeProcess, OpenProcess, OpenThread,
        PROCESS_QUERY_LIMITED_INFORMATION, ResumeThread, THREAD_SUSPEND_RESUME,
    };

    /// The creation flags of every program Lemma starts (see [`super::command`]).
    pub(super) const NO_WINDOW: u32 = CREATE_NO_WINDOW;

    /// A process and everything it starts: a job object that ends all of them when it is
    /// terminated or closed (also when Lemma itself ends, however it ends).
    #[derive(Debug)]
    pub struct Job {
        handle: HANDLE,
    }

    // SAFETY: a job object handle may be used from any thread.
    unsafe impl Send for Job {}
    // SAFETY: as above; every call on it is a plain system call.
    unsafe impl Sync for Job {}

    impl Job {
        /// Starts `command` in a new job. The process starts suspended and only runs once it is
        /// in the job, so nothing it starts can escape it. Replaces the command's creation flags
        /// with "no console window" (as [`super::command`] sets them).
        pub fn spawn(command: &mut Command) -> io::Result<(Child, Job)> {
            let job = Job::new()?;
            command.creation_flags(NO_WINDOW | CREATE_SUSPENDED);
            let mut child = command.spawn()?;
            // SAFETY: both handles are valid: the job's is ours, the child's is open while
            // `child` lives.
            let assigned =
                unsafe { AssignProcessToJobObject(job.handle, child.as_raw_handle()) } != 0;
            let started = if assigned {
                resume(child.id())
            } else {
                Err(io::Error::last_os_error())
            };
            if let Err(e) = started {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e);
            }
            Ok((child, job))
        }

        fn new() -> io::Result<Job> {
            // SAFETY: no security attributes and no name: a private, unnamed job.
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            let job = Job { handle };
            // SAFETY: an all-zero JOBOBJECT_EXTENDED_LIMIT_INFORMATION means "no limits".
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            // SAFETY: `limits` is the structure the information class names, with its size.
            let set = unsafe {
                SetInformationJobObject(
                    job.handle,
                    JobObjectExtendedLimitInformation,
                    (&raw const limits).cast(),
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            if set == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(job)
        }

        /// Ends every process in the job at once, with exit code `code`.
        pub fn terminate(&self, code: u32) {
            // SAFETY: plain system call on our own handle.
            unsafe {
                TerminateJobObject(self.handle, code);
            }
        }

        /// Whether a process in the job is still running.
        pub fn has_processes(&self) -> bool {
            // SAFETY: an all-zero structure is a valid value for the call to fill in.
            let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
            // SAFETY: `info` is the structure the information class names, with its size.
            let ok = unsafe {
                QueryInformationJobObject(
                    self.handle,
                    JobObjectBasicAccountingInformation,
                    (&raw mut info).cast(),
                    size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                    std::ptr::null_mut(),
                )
            };
            ok != 0 && info.ActiveProcesses > 0
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: the handle is ours and closed only here. Closing the last handle ends the
            // processes still in the job.
            unsafe {
                CloseHandle(self.handle);
            }
        }
    }

    /// Resumes the thread of the suspended new process `pid` (it has exactly one).
    fn resume(pid: u32) -> io::Result<()> {
        // SAFETY: a thread snapshot of the whole system; the process id is ignored for threads.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: an all-zero THREADENTRY32 with its size set is what Thread32First expects.
        let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
        entry.dwSize = size_of::<THREADENTRY32>() as u32;
        let mut resumed = false;
        // SAFETY: `entry` is a valid THREADENTRY32 for the snapshot to fill in.
        let mut more = unsafe { Thread32First(snapshot, &raw mut entry) } != 0;
        while more && !resumed {
            if entry.th32OwnerProcessID == pid {
                // SAFETY: plain system calls; the thread handle is closed right away.
                unsafe {
                    let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                    if !thread.is_null() {
                        resumed = ResumeThread(thread) != u32::MAX;
                        CloseHandle(thread);
                    }
                }
            }
            // SAFETY: as for Thread32First.
            more = unsafe { Thread32Next(snapshot, &raw mut entry) } != 0;
        }
        // SAFETY: the snapshot handle is ours.
        unsafe {
            CloseHandle(snapshot);
        }
        if resumed {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "cannot resume the new process {pid}"
            )))
        }
    }

    /// Whether the process `pid` exists and has not exited.
    pub fn is_running(pid: u32) -> bool {
        // SAFETY: plain system calls; the handle is closed right away.
        unsafe {
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if process.is_null() {
                // Access denied means it exists (another user's process).
                return io::Error::last_os_error().raw_os_error() == Some(5);
            }
            let mut code = 0u32;
            let ok = GetExitCodeProcess(process, &raw mut code) != 0;
            CloseHandle(process);
            ok && code == STILL_ACTIVE as u32
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn a_job_ends_the_process_and_its_children() {
        // cmd waits for its child, ping; terminating the job ends both.
        let mut cmd = command("cmd");
        cmd.args(["/c", "ping", "-n", "30", "127.0.0.1"])
            .stdout(std::process::Stdio::null());
        let (mut child, job) = Job::spawn(&mut cmd).expect("spawn");
        assert!(job.has_processes());
        assert!(is_running(child.id()));
        job.terminate(1);
        let deadline = Instant::now() + Duration::from_secs(5);
        while job.has_processes() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!job.has_processes(), "every process in the job ended");
        let _ = child.wait();
        assert!(!is_running(u32::MAX - 3));
    }
}
