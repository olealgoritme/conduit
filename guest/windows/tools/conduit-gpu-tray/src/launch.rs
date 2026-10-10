//! Starting programs for the control channel `run` and `stop` ops.
//!
//! The tray runs elevated, but what the host asks for should start the way a
//! double click would: with the user's normal token. The technique is the
//! shell's own token: the process behind the desktop (`GetShellWindow`) is
//! the user's non-elevated Explorer; its token is duplicated to a primary
//! token and handed to `CreateProcessWithTokenW` (an elevated administrator
//! holds `SeImpersonatePrivilege`, which that call needs).
//!
//! * Programs (`.exe`, `.com`) are created directly with that token, so we
//!   get a pid, can put the process in a job object and stop its tree.
//! * Shortcuts, documents, scripts and URLs (`steam://...`) are handed to
//!   `explorer.exe <target>` started with the same token: the shell opens
//!   it as it would for the user. There is no pid to report (0).
//!
//! A tray that is not elevated has nothing to drop and starts programs with
//! `CreateProcessW` and everything else with `ShellExecuteExW`.

use crate::apps::wide_os;
use crate::gfx::wide;
use conduit_ctl::RunArgs;
use gpu_tray::procs::{
    classify, command_line, descendants, display_name, env_block, parse_env_block, quote_arg,
    LaunchTable, Target,
};
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Security::*;
use windows_sys::Win32::Storage::FileSystem::SearchPathW;
use windows_sys::Win32::System::Diagnostics::ToolHelp::*;
use windows_sys::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows_sys::Win32::System::JobObjects::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleFileNameW;
use windows_sys::Win32::System::Threading::*;
use windows_sys::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOASYNC, SHELLEXECUTEINFOW};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetShellWindow, GetWindowThreadProcessId};

/// Process and job handles of a launch, kept open so the pid stays ours.
struct Handles {
    process: isize,
    job: isize,
}

impl Drop for Handles {
    fn drop(&mut self) {
        unsafe {
            if self.process != 0 {
                CloseHandle(self.process as HANDLE);
            }
            if self.job != 0 {
                CloseHandle(self.job as HANDLE);
            }
        }
    }
}

static TABLE: Mutex<LaunchTable<Handles>> = Mutex::new(LaunchTable::new(32));

fn table() -> std::sync::MutexGuard<'static, LaunchTable<Handles>> {
    TABLE.lock().unwrap_or_else(|e| e.into_inner())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn alive(process: isize) -> bool {
    process != 0 && unsafe { WaitForSingleObject(process as HANDLE, 0) } == WAIT_TIMEOUT
}

/// A launch for the "Recent launches" menu.
pub struct Recent {
    pub pid: u32,
    pub label: String,
    pub alive: bool,
}

pub fn recent(n: usize) -> Vec<Recent> {
    table()
        .recent(n)
        .into_iter()
        .map(|l| {
            let alive = alive(l.extra.process);
            Recent {
                pid: l.pid,
                label: if alive {
                    format!("{}  (pid {})", l.name, l.pid)
                } else {
                    format!("{}  (exited)", l.name)
                },
                alive,
            }
        })
        .collect()
}

fn last_error() -> std::io::Error {
    std::io::Error::last_os_error()
}

fn token_elevated() -> bool {
    unsafe {
        let mut t: HANDLE = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut t) == 0 {
            return false;
        }
        let mut e = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut n = 0u32;
        let ok = GetTokenInformation(
            t,
            TokenElevation,
            &mut e as *mut _ as *mut c_void,
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut n,
        );
        CloseHandle(t);
        ok != 0 && e.TokenIsElevated != 0
    }
}

/// `CreateProcessWithTokenW` needs the privilege enabled, not just held.
fn enable_impersonate_privilege() {
    unsafe {
        let mut t: HANDLE = null_mut();
        if OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut t,
        ) == 0
        {
            return;
        }
        let name = wide("SeImpersonatePrivilege");
        let mut tp: TOKEN_PRIVILEGES = std::mem::zeroed();
        tp.PrivilegeCount = 1;
        tp.Privileges[0].Attributes = SE_PRIVILEGE_ENABLED;
        if LookupPrivilegeValueW(null(), name.as_ptr(), &mut tp.Privileges[0].Luid) != 0 {
            AdjustTokenPrivileges(t, 0, &tp, 0, null_mut(), null_mut());
        }
        CloseHandle(t);
    }
}

/// A primary copy of the desktop shell's (non-elevated) token.
pub struct ShellToken(HANDLE);

impl ShellToken {
    fn get() -> Result<ShellToken, String> {
        unsafe {
            let shell = GetShellWindow();
            if shell.is_null() {
                return Err("no desktop shell to start programs in".into());
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(shell, &mut pid);
            let p = OpenProcess(PROCESS_QUERY_INFORMATION, 0, pid);
            if p.is_null() {
                return Err(format!("cannot open the desktop shell: {}", last_error()));
            }
            let mut t: HANDLE = null_mut();
            let ok = OpenProcessToken(p, TOKEN_DUPLICATE | TOKEN_QUERY, &mut t);
            let err = last_error();
            CloseHandle(p);
            if ok == 0 {
                return Err(format!("cannot open the shell's token: {err}"));
            }
            let mut dup: HANDLE = null_mut();
            let ok = DuplicateTokenEx(
                t,
                0x0200_0000, // MAXIMUM_ALLOWED
                null(),
                SecurityImpersonation,
                TokenPrimary,
                &mut dup,
            );
            let err = last_error();
            CloseHandle(t);
            if ok == 0 {
                return Err(format!("cannot copy the shell's token: {err}"));
            }
            Ok(ShellToken(dup))
        }
    }
}

impl Drop for ShellToken {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

/// The user's environment for `token`, or ours without one.
fn base_env(token: Option<&ShellToken>) -> Vec<(String, String)> {
    if let Some(t) = token {
        let mut blk: *mut c_void = null_mut();
        if unsafe { CreateEnvironmentBlock(&mut blk, t.0, 0) } != 0 && !blk.is_null() {
            let p = blk as *const u16;
            // Up to the empty entry that ends the block (bounded).
            let mut n = 0usize;
            while n < (1 << 20) && !(unsafe { *p.add(n) } == 0 && unsafe { *p.add(n + 1) } == 0) {
                n += 1;
            }
            let v = parse_env_block(unsafe { std::slice::from_raw_parts(p, n + 2) });
            unsafe { DestroyEnvironmentBlock(blk) };
            return v;
        }
    }
    std::env::vars_os()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        })
        .collect()
}

fn find_on_path(name: &str, ext: &str) -> Option<PathBuf> {
    let n = wide(name);
    let e = wide(ext);
    let mut buf = vec![0u16; 1024];
    let len = unsafe {
        SearchPathW(
            null(),
            n.as_ptr(),
            e.as_ptr(),
            buf.len() as u32,
            buf.as_mut_ptr(),
            null_mut(),
        )
    };
    if len == 0 || len as usize >= buf.len() {
        return None;
    }
    Some(PathBuf::from(String::from_utf16_lossy(
        &buf[..len as usize],
    )))
}

/// The SID bytes of `token`'s user.
fn token_user(token: HANDLE) -> Option<Vec<u8>> {
    unsafe {
        let mut n = 0u32;
        GetTokenInformation(token, TokenUser, null_mut(), 0, &mut n);
        if n == 0 {
            return None;
        }
        let mut buf = vec![0u8; n as usize];
        if GetTokenInformation(token, TokenUser, buf.as_mut_ptr() as *mut c_void, n, &mut n) == 0 {
            return None;
        }
        let tu = &*(buf.as_ptr() as *const TOKEN_USER);
        let sid = tu.User.Sid;
        if IsValidSid(sid) == 0 {
            return None;
        }
        let len = GetLengthSid(sid) as usize;
        Some(std::slice::from_raw_parts(sid as *const u8, len).to_vec())
    }
}

fn own_user() -> Option<Vec<u8>> {
    unsafe {
        let mut t: HANDLE = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut t) == 0 {
            return None;
        }
        let u = token_user(t);
        CloseHandle(t);
        u
    }
}

/// Runs `f` with the desktop user's normal token: the calling thread
/// impersonates the shell's token for the call (only when this process is
/// elevated; otherwise it already has the user's rights). Errors when there
/// is no shell of this same user to take the token from; `f` does not run.
pub fn as_user<R>(f: impl FnOnce() -> R) -> Result<R, String> {
    if !token_elevated() {
        return Ok(f());
    }
    let t = ShellToken::get()?;
    match (token_user(t.0), own_user()) {
        (Some(a), Some(b)) if a == b => {}
        _ => return Err("the desktop shell belongs to another user".into()),
    }
    if unsafe { ImpersonateLoggedOnUser(t.0) } == 0 {
        return Err(format!("cannot take the user's token: {}", last_error()));
    }
    struct Revert;
    impl Drop for Revert {
        fn drop(&mut self) {
            // Carrying on with the wrong token is never an option.
            if unsafe { RevertToSelf() } == 0 {
                std::process::abort();
            }
        }
    }
    let _revert = Revert;
    Ok(f())
}

/// A started helper's process handle.
pub struct Child(HANDLE);

impl Child {
    /// Waits for the process; its exit code.
    pub fn wait(&self) -> Option<u32> {
        unsafe {
            WaitForSingleObject(self.0, INFINITE);
            let mut code = 0u32;
            (GetExitCodeProcess(self.0, &mut code) != 0).then_some(code)
        }
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

/// Starts this exe with `--send-list` on a new list file of `paths`, with
/// the desktop user's normal token, so the copy has the user's rights only
/// (a drop on the elevated popup must not read what the user cannot).
pub fn send_as_user(paths: &[Vec<u16>]) -> Result<Child, String> {
    let token = if token_elevated() {
        enable_impersonate_privilege();
        Some(ShellToken::get()?)
    } else {
        None
    };
    let env = base_env(token.as_ref());
    let temp = env
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("TEMP"))
        .map(|(_, v)| PathBuf::from(v))
        .unwrap_or_else(std::env::temp_dir);
    let list = temp.join(format!(
        "{}{}-{}.txt",
        gpu_tray::policy::SEND_LIST_PREFIX,
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    ));
    let bytes = gpu_tray::policy::send_list_bytes(paths);
    as_user(|| {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&list)
            .and_then(|mut f| f.write_all(&bytes))
    })?
    .map_err(|e| format!("cannot write {}: {e}", list.display()))?;
    let mut exe = vec![0u16; 32768];
    let n = unsafe { GetModuleFileNameW(null_mut(), exe.as_mut_ptr(), exe.len() as u32) } as usize;
    let exe = PathBuf::from(String::from_utf16_lossy(&exe[..n]));
    let line = command_line(
        &exe.to_string_lossy(),
        &[
            "--send-list".to_string(),
            list.to_string_lossy().into_owned(),
        ],
    );
    let started = create(
        token.as_ref(),
        &exe,
        &line,
        None,
        env_block(env, &Default::default()),
    );
    match started {
        Ok(s) => {
            resume(s.thread);
            Ok(Child(s.process))
        }
        Err(e) => {
            let _ = as_user(|| std::fs::remove_file(&list));
            Err(e)
        }
    }
}

/// The file `cmd` names. A bare name is looked up like a shell would
/// (`notepad`, `cmd.exe`); a path must exist.
fn locate(cmd: &str, cwd: Option<&Path>) -> Result<PathBuf, String> {
    let c = cmd.replace('/', "\\");
    if !c.contains(['\\', ':']) {
        let ext = if c.contains('.') { "" } else { ".exe" };
        return find_on_path(&c, ext).ok_or_else(|| format!("not found: {cmd}"));
    }
    let p = PathBuf::from(&c);
    let p = match cwd {
        Some(d) if !p.is_absolute() => d.join(p),
        _ => p,
    };
    if p.exists() {
        Ok(p)
    } else {
        Err(format!("not found: {cmd}"))
    }
}

struct Started {
    pid: u32,
    process: HANDLE,
    /// The main thread, still suspended.
    thread: HANDLE,
}

fn resume(thread: HANDLE) {
    if !thread.is_null() {
        unsafe {
            ResumeThread(thread);
            CloseHandle(thread);
        }
    }
}

fn create(
    token: Option<&ShellToken>,
    app: &Path,
    cmdline: &str,
    cwd: Option<&Path>,
    env: Vec<u16>,
) -> Result<Started, String> {
    let app_w = wide_os(app);
    let mut cmd_w = wide(cmdline);
    let cwd_w = cwd.map(wide_os);
    let mut desktop = wide("winsta0\\default");
    let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
    si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    si.lpDesktop = desktop.as_mut_ptr();
    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let flags = CREATE_UNICODE_ENVIRONMENT | CREATE_SUSPENDED;
    let cwd_p = cwd_w.as_ref().map_or(null(), |w| w.as_ptr());
    let ok = unsafe {
        match token {
            Some(t) => CreateProcessWithTokenW(
                t.0,
                0,
                app_w.as_ptr(),
                cmd_w.as_mut_ptr(),
                flags,
                env.as_ptr() as *const c_void,
                cwd_p,
                &si,
                &mut pi,
            ),
            None => CreateProcessW(
                app_w.as_ptr(),
                cmd_w.as_mut_ptr(),
                null(),
                null(),
                0,
                flags,
                env.as_ptr() as *const c_void,
                cwd_p,
                &si,
                &mut pi,
            ),
        }
    };
    if ok == 0 {
        return Err(format!("cannot start {}: {}", app.display(), last_error()));
    }
    Ok(Started {
        pid: pi.dwProcessId,
        process: pi.hProcess,
        thread: pi.hThread,
    })
}

/// A job for the new process (so `stop` takes its children too), then let
/// it run. Without a job the process still runs.
fn adopt(process: HANDLE, thread: HANDLE) -> isize {
    unsafe {
        let job = CreateJobObjectW(null(), null());
        let mut jh = 0isize;
        if !job.is_null() {
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            // Children may leave the job on request; the rest stay in it.
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_BREAKAWAY_OK;
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if AssignProcessToJobObject(job, process) != 0 {
                jh = job as isize;
            } else {
                CloseHandle(job);
            }
        }
        resume(thread);
        jh
    }
}

fn record(name: String, pid: u32, process: HANDLE, job: isize) {
    // Launches that fall off the end close their handles here.
    let gone = table().record(
        name,
        pid,
        now(),
        Handles {
            process: process as isize,
            job,
        },
    );
    drop(gone);
}

/// Starts `a.cmd`; returns the pid, 0 when the shell took it over.
pub fn run(a: &RunArgs) -> Result<u32, String> {
    let cmd = a.cmd.trim();
    let cwd = match a.cwd.trim() {
        "" => None,
        d => {
            let p = PathBuf::from(d.replace('/', "\\"));
            if !p.is_dir() {
                return Err(format!("not found: {d}"));
            }
            Some(p)
        }
    };
    let elevated = token_elevated();
    let token = if elevated {
        enable_impersonate_privilege();
        Some(ShellToken::get()?)
    } else {
        None
    };
    let target = classify(cmd);
    // Bare names such as `notepad` are programs.
    let program = match target {
        Target::Exe => Some(locate(cmd, cwd.as_deref())?),
        Target::Shell if !cmd.contains(['\\', '/', ':', '.']) => Some(locate(cmd, cwd.as_deref())?),
        _ => None,
    };
    if let Some(exe) = program {
        let dir = cwd.clone().or_else(|| exe.parent().map(Path::to_path_buf));
        let env = env_block(base_env(token.as_ref()), &a.env);
        let line = command_line(&exe.to_string_lossy(), &a.args);
        let s = create(token.as_ref(), &exe, &line, dir.as_deref(), env)?;
        let job = adopt(s.process, s.thread);
        record(display_name(&exe.to_string_lossy()), s.pid, s.process, job);
        return Ok(s.pid);
    }
    if !a.args.is_empty() {
        return Err("arguments are only supported for programs (.exe)".into());
    }
    let item = if target == Target::Url {
        cmd.to_string()
    } else {
        locate(cmd, cwd.as_deref())?.to_string_lossy().into_owned()
    };
    match token {
        Some(t) => {
            let mut win = vec![0u16; 520];
            let n = unsafe {
                windows_sys::Win32::System::SystemInformation::GetWindowsDirectoryW(
                    win.as_mut_ptr(),
                    win.len() as u32,
                )
            } as usize;
            if n == 0 || n >= win.len() {
                return Err("cannot find the Windows folder".into());
            }
            let explorer = PathBuf::from(String::from_utf16_lossy(&win[..n])).join("explorer.exe");
            let line = format!("\"{}\" {}", explorer.display(), quote_arg(&item));
            let env = env_block(base_env(Some(&t)), &a.env);
            let s = create(Some(&t), &explorer, &line, cwd.as_deref(), env)?;
            // Not tracked: it hands the target to the shell and exits.
            resume(s.thread);
            unsafe { CloseHandle(s.process) };
            Ok(0)
        }
        None => shell_execute(&item, cwd.as_deref()).map(|()| 0),
    }
}

fn shell_execute(item: &str, cwd: Option<&Path>) -> Result<(), String> {
    let verb = wide("open");
    let file = wide(item);
    let dir = cwd.map(wide_os);
    let mut sei: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    sei.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    sei.fMask = SEE_MASK_NOASYNC;
    sei.lpVerb = verb.as_ptr();
    sei.lpFile = file.as_ptr();
    sei.lpDirectory = dir.as_ref().map_or(null(), |d| d.as_ptr());
    sei.nShow = 1; // SW_SHOWNORMAL
    if unsafe { ShellExecuteExW(&mut sei) } == 0 {
        return Err(format!("cannot open {item}: {}", last_error()));
    }
    Ok(())
}

/// Ends a program `run` started, with its children. Refuses other pids.
pub fn stop(pid: u32) -> Result<(), String> {
    let (process, job) = {
        let t = table();
        let l = t
            .get(pid)
            .ok_or_else(|| format!("pid {pid} was not started by conduit"))?;
        // Duplicate so the table can drop its own while we work.
        let dup = |h: isize| -> isize {
            if h == 0 {
                return 0;
            }
            let mut out: HANDLE = null_mut();
            let ok = unsafe {
                DuplicateHandle(
                    GetCurrentProcess(),
                    h as HANDLE,
                    GetCurrentProcess(),
                    &mut out,
                    0,
                    0,
                    DUPLICATE_SAME_ACCESS,
                )
            };
            if ok != 0 {
                out as isize
            } else {
                0
            }
        };
        (dup(l.extra.process), dup(l.extra.job))
    };
    let was_alive = alive(process);
    if job != 0 {
        unsafe { TerminateJobObject(job as HANDLE, 1) };
    } else if was_alive {
        // No job: walk the process tree from a snapshot.
        for p in descendants(pid, &snapshot()).into_iter().rev() {
            if p == pid {
                continue;
            }
            kill_pid(p);
        }
    }
    if was_alive {
        unsafe { TerminateProcess(process as HANDLE, 1) };
    }
    unsafe {
        if process != 0 {
            CloseHandle(process as HANDLE);
        }
        if job != 0 {
            CloseHandle(job as HANDLE);
        }
    }
    Ok(())
}

fn kill_pid(pid: u32) {
    unsafe {
        let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if !h.is_null() {
            TerminateProcess(h, 1);
            CloseHandle(h);
        }
    }
}

/// `(pid, parent pid)` of every process.
fn snapshot() -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    unsafe {
        let s = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if s == INVALID_HANDLE_VALUE {
            return out;
        }
        let mut e: PROCESSENTRY32W = std::mem::zeroed();
        e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut ok = Process32FirstW(s, &mut e);
        while ok != 0 {
            out.push((e.th32ProcessID, e.th32ParentProcessID));
            ok = Process32NextW(s, &mut e);
        }
        CloseHandle(s);
    }
    out
}
