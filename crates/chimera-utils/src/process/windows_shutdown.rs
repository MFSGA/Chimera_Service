//! Windows console setup and targeted graceful-shutdown helper.

use std::{
    io,
    os::windows::io::{FromRawHandle, OwnedHandle, RawHandle},
    path::PathBuf,
    sync::OnceLock,
};

use windows::{
    Win32::{
        System::{
            Console::{
                AllocConsole, AttachConsole, CTRL_BREAK_EVENT, FreeConsole,
                GenerateConsoleCtrlEvent, GetConsoleProcessList, GetConsoleWindow,
            },
            LibraryLoader::{GetModuleHandleW, GetProcAddress},
            Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
        },
        UI::WindowsAndMessaging::{SW_HIDE, ShowWindow},
    },
    core::{s, w},
};

const HELPER_ARG: &str = "--chimera-process-shutdown";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const ALLOC_CONSOLE_MODE_NO_WINDOW: i32 = 2;

#[repr(C)]
struct AllocConsoleOptions {
    mode: i32,
    use_show_window: i32,
    show_window: u16,
}

/// Keeps one hidden console attached to the host so spawned cores inherit it.
/// Existing developer terminals are left visible and unchanged.
pub(super) fn prepare_shutdown_console() -> bool {
    static CONSOLE_READY: OnceLock<bool> = OnceLock::new();
    *CONSOLE_READY.get_or_init(allocate_shutdown_console)
}

fn allocate_shutdown_console() -> bool {
    if has_console() {
        return true;
    }

    // Windows 11 24H2 adds a no-window allocation mode. Resolve it dynamically
    // so the executable continues to load on earlier supported Windows builds.
    if unsafe { allocate_without_window() } {
        return true;
    }

    // Older Windows versions have no hidden-console allocation API. Hide the
    // newly allocated window immediately; if that cannot be done, detach and
    // retain the existing hard-kill shutdown behavior.
    unsafe {
        if AllocConsole().is_err() {
            return has_console();
        }
        let window = GetConsoleWindow();
        if window.0.is_null() {
            let _ = FreeConsole();
            return false;
        }
        let _ = ShowWindow(window, SW_HIDE);
    }
    has_console()
}

fn has_console() -> bool {
    let mut process_ids = [0_u32; 1];
    unsafe { GetConsoleProcessList(&mut process_ids) != 0 }
}

unsafe fn allocate_without_window() -> bool {
    let Ok(kernel32) = (unsafe { GetModuleHandleW(w!("kernel32.dll")) }) else {
        return false;
    };
    let Some(function) = (unsafe { GetProcAddress(kernel32, s!("AllocConsoleWithOptions")) })
    else {
        return false;
    };

    type AllocConsoleWithOptionsFn =
        unsafe extern "system" fn(*const AllocConsoleOptions, *mut i32) -> i32;
    // SAFETY: the resolved export has this documented Win32 signature.
    let allocate: AllocConsoleWithOptionsFn = unsafe { std::mem::transmute(function) };
    let options = AllocConsoleOptions {
        mode: ALLOC_CONSOLE_MODE_NO_WINDOW,
        use_show_window: 0,
        show_window: 0,
    };
    let mut result = 0_i32;
    // HRESULT values below zero are failures.
    unsafe { allocate(&options, &mut result) >= 0 && has_console() }
}

/// Dispatches the private helper mode before the application initializes.
/// `None` means normal startup; the returned result becomes the helper exit code.
pub fn windows_shutdown_helper_entry() -> Option<io::Result<()>> {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(std::ffi::OsStr::new(HELPER_ARG)) {
        return None;
    }
    let pid = args
        .next()
        .and_then(|arg| arg.to_str()?.parse().ok())
        .filter(|_| args.next().is_none());
    Some(match pid {
        Some(pid) => send_ctrl_break(pid),
        None => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid shutdown target",
        )),
    })
}

fn send_ctrl_break(pid: u32) -> io::Result<()> {
    // The helper is a separate process so attaching to the core's console does
    // not change the GUI/service host's console association.
    unsafe {
        let _ = FreeConsole();
        AttachConsole(pid)?;
        let result = GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid);
        let _ = FreeConsole();
        result?;
    }
    Ok(())
}

/// A single, bounded request per process. Repeated stops must not resend a
/// control event that a core could interpret as an immediate second interrupt.
pub(super) struct Shutdown {
    helper: PathBuf,
    pid: u32,
    _target: TargetProcess,
    requested: bool,
}

impl Shutdown {
    pub(super) fn new(helper: PathBuf, pid: u32, target: TargetProcess) -> Self {
        Self {
            helper,
            pid,
            _target: target,
            requested: false,
        }
    }

    pub(super) fn request(&mut self) -> Option<tokio::process::Command> {
        if std::mem::replace(&mut self.requested, true) {
            return None;
        }
        let mut command = tokio::process::Command::new(&self.helper);
        command
            .args([HELPER_ARG, &self.pid.to_string()])
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        Some(command)
    }
}

/// Keeps the target PID reserved until any shutdown helper has exited or been
/// canceled and reaped. Windows may reuse a PID after the process object is
/// released; this handle keeps the target identity valid across helper cleanup.
pub(super) struct TargetProcess {
    _handle: OwnedHandle,
}

impl TargetProcess {
    pub(super) fn open(pid: u32) -> io::Result<Self> {
        // SAFETY: OpenProcess receives a PID and a read-only query permission.
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
            .map_err(|error| io::Error::other(error.to_string()))?;
        Ok(Self {
            // SAFETY: OpenProcess returned a valid, uniquely owned process handle.
            _handle: unsafe { OwnedHandle::from_raw_handle(handle.0 as RawHandle) },
        })
    }
}
