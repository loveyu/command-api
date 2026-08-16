#[cfg(unix)]
mod implementation {
    use anyhow::{Context, Result};
    use nix::{
        errno::Errno,
        sys::{
            signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, killpg, sigaction},
            wait::{WaitPidFlag, WaitStatus, waitpid},
        },
        unistd::{Pid, getpgrp},
    };
    use std::{os::unix::process::CommandExt, time::Duration};
    use tokio::process::{Child, Command};

    pub struct ProcessTree {
        pid: Option<Pid>,
    }

    impl ProcessTree {
        pub fn new() -> Result<Self> {
            Ok(Self { pid: None })
        }

        pub fn prepare_command(&mut self, command: &mut Command) {
            command.as_std_mut().process_group(0);
        }

        pub fn attach(&mut self, child: &Child) -> Result<()> {
            let pid = child.id().context("Worker 启动后没有 PID")?;
            self.pid = Some(Pid::from_raw(pid as i32));
            Ok(())
        }

        pub fn graceful(&self) -> Result<()> {
            send_group(self.pid, Signal::SIGTERM)
        }

        pub fn force(&self) -> Result<()> {
            send_group(self.pid, Signal::SIGKILL)
        }

        pub async fn wait_empty(&self) -> Result<()> {
            Ok(())
        }
    }

    fn send_group(pid: Option<Pid>, signal: Signal) -> Result<()> {
        let pid = pid.context("进程树尚未初始化")?;
        match killpg(pid, signal) {
            Ok(()) | Err(Errno::ESRCH) => Ok(()),
            Err(error) => Err(error).context("发送进程组信号失败"),
        }
    }

    extern "C" fn ignore_term(_: i32) {}

    pub fn worker_setup() -> Result<()> {
        let action = SigAction::new(SigHandler::Handler(ignore_term), SaFlags::SA_RESTART, SigSet::empty());
        // SAFETY: handler has C ABI and remains valid for the process lifetime.
        unsafe { sigaction(Signal::SIGTERM, &action) }.context("安装 Worker SIGTERM 处理器失败")?;
        // SAFETY: PR_SET_CHILD_SUBREAPER only changes re-parenting behavior for this process.
        let result = unsafe { nix::libc::prctl(nix::libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) };
        if result != 0 {
            return Err(std::io::Error::last_os_error()).context("设置 Worker 为 child subreaper 失败");
        }
        Ok(())
    }

    pub fn worker_graceful() -> Result<()> {
        match killpg(getpgrp(), Signal::SIGTERM) {
            Ok(()) | Err(Errno::ESRCH) => Ok(()),
            Err(error) => Err(error).context("Worker 发送 SIGTERM 失败"),
        }
    }

    pub fn worker_force_after_parent_loss() -> ! {
        let _ = killpg(getpgrp(), Signal::SIGKILL);
        std::process::abort()
    }

    pub fn worker_wait_for_descendants() -> Result<()> {
        loop {
            match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::StillAlive) => std::thread::sleep(Duration::from_millis(50)),
                Ok(_) => continue,
                Err(Errno::ECHILD) => return Ok(()),
                Err(Errno::EINTR) => continue,
                Err(error) => return Err(error).context("等待脚本子进程失败"),
            }
        }
    }

    pub const fn graceful_method() -> &'static str {
        "process_group_sigterm"
    }

    pub const fn force_method() -> &'static str {
        "process_group_sigkill"
    }

    pub const fn graceful_signal() -> Option<&'static str> {
        Some("SIGTERM")
    }

    pub const fn force_signal() -> Option<&'static str> {
        Some("SIGKILL")
    }
}

#[cfg(windows)]
mod implementation {
    use anyhow::{Context, Result};
    use std::{
        mem::{size_of, zeroed},
        sync::atomic::{AtomicBool, Ordering},
        time::Duration,
    };
    use tokio::process::{Child, Command};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE, TRUE},
        System::{
            Console::{
                AllocConsole, CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent, GetConsoleProcessList, GetConsoleWindow,
                SetConsoleCtrlHandler,
            },
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation, QueryInformationJobObject,
                SetInformationJobObject, TerminateJobObject,
            },
            Threading::CREATE_NEW_PROCESS_GROUP,
        },
        UI::WindowsAndMessaging::{SW_HIDE, ShowWindow},
    };

    pub struct ProcessTree {
        job: HANDLE,
    }

    // SAFETY: Job handles may be used from any thread and ownership remains with ProcessTree.
    unsafe impl Send for ProcessTree {}
    unsafe impl Sync for ProcessTree {}

    impl ProcessTree {
        pub fn new() -> Result<Self> {
            // SAFETY: null security/name creates a private unnamed job object.
            let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if job.is_null() {
                return Err(std::io::Error::last_os_error()).context("创建 Windows Job Object 失败");
            }
            // SAFETY: the structure is fully initialized before passing its exact size.
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = unsafe {
                SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            if ok == 0 {
                // SAFETY: job is a valid owned handle.
                unsafe { CloseHandle(job) };
                return Err(std::io::Error::last_os_error()).context("配置 Windows Job Object 失败");
            }
            Ok(Self { job })
        }

        pub fn prepare_command(&mut self, command: &mut Command) {
            use std::os::windows::process::CommandExt;
            command.as_std_mut().creation_flags(CREATE_NEW_PROCESS_GROUP);
        }

        pub fn attach(&mut self, child: &Child) -> Result<()> {
            let process = child.raw_handle().context("Worker 启动后没有进程句柄")? as HANDLE;
            // SAFETY: both handles are valid for the duration of the call.
            if unsafe { AssignProcessToJobObject(self.job, process) } == 0 {
                return Err(std::io::Error::last_os_error()).context("将 Worker 加入 Windows Job Object 失败");
            }
            Ok(())
        }

        pub fn graceful(&self) -> Result<()> {
            Ok(())
        }

        pub fn force(&self) -> Result<()> {
            // SAFETY: job is a valid owned handle.
            if unsafe { TerminateJobObject(self.job, 1) } == 0 {
                return Err(std::io::Error::last_os_error()).context("强制终止 Windows Job Object 失败");
            }
            Ok(())
        }

        pub async fn wait_empty(&self) -> Result<()> {
            loop {
                // SAFETY: structure and output size are correct for the requested information class.
                let mut accounting: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { zeroed() };
                let ok = unsafe {
                    QueryInformationJobObject(
                        self.job,
                        JobObjectBasicAccountingInformation,
                        (&mut accounting as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                        size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                        std::ptr::null_mut(),
                    )
                };
                if ok == 0 {
                    return Err(std::io::Error::last_os_error()).context("查询 Windows Job Object 状态失败");
                }
                if accounting.ActiveProcesses == 0 {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }

    impl Drop for ProcessTree {
        fn drop(&mut self) {
            // SAFETY: ProcessTree exclusively owns this handle.
            unsafe { CloseHandle(self.job) };
        }
    }

    static BREAK_RECEIVED: AtomicBool = AtomicBool::new(false);

    unsafe extern "system" fn console_handler(event: u32) -> i32 {
        if event == CTRL_BREAK_EVENT {
            BREAK_RECEIVED.store(true, Ordering::SeqCst);
            TRUE
        } else {
            0
        }
    }

    pub fn worker_setup() -> Result<()> {
        // A service has no console. Give each worker a private hidden console so CTRL+BREAK
        // can be targeted without affecting the HTTP service or unrelated tasks.
        // SAFETY: console APIs have no pointer arguments here.
        unsafe {
            let mut console_process = 0;
            if GetConsoleProcessList(&mut console_process, 1) == 0 {
                if AllocConsole() == 0 {
                    return Err(std::io::Error::last_os_error()).context("为 Windows Worker 创建控制台失败");
                }
                let window = GetConsoleWindow();
                if !window.is_null() {
                    ShowWindow(window, SW_HIDE);
                }
            }
            if SetConsoleCtrlHandler(Some(console_handler), TRUE) == 0 {
                return Err(std::io::Error::last_os_error()).context("安装 Windows Worker 控制事件处理器失败");
            }
        }
        Ok(())
    }

    pub fn worker_graceful() -> Result<()> {
        let pid = std::process::id();
        // SAFETY: worker is the root of a CREATE_NEW_PROCESS_GROUP group.
        if unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) } == 0 {
            return Err(std::io::Error::last_os_error()).context("发送 CTRL+BREAK 失败");
        }
        Ok(())
    }

    pub fn worker_force_after_parent_loss() -> ! {
        // The parent owns a KILL_ON_JOB_CLOSE handle. If the control pipe reached EOF because
        // the parent exited, Windows terminates this worker and all descendants automatically.
        loop {
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    pub fn worker_wait_for_descendants() -> Result<()> {
        Ok(())
    }

    pub const fn graceful_method() -> &'static str {
        "ctrl_break_event"
    }

    pub const fn force_method() -> &'static str {
        "terminate_job_object"
    }

    pub const fn graceful_signal() -> Option<&'static str> {
        Some("CTRL_BREAK")
    }

    pub const fn force_signal() -> Option<&'static str> {
        None
    }
}

pub use implementation::*;
