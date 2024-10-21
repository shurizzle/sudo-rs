use core::fmt;
// TODO: remove unused attribute when system is cleaned up
use std::{
    collections::BTreeSet,
    ffi::{c_uint, CStr, CString},
    io,
    mem::MaybeUninit,
    ops,
    os::{
        fd::AsRawFd,
        unix::{self, prelude::OsStrExt},
    },
    path::{Path, PathBuf},
    str::FromStr,
};

use crate::{
    common::{Error, SudoPath, SudoString},
    cutils::*,
};
pub use audit::secure_open;
use interface::{DeviceId, GroupId, ProcessId, UserId};
pub use libc::PATH_MAX;
use libc::STDERR_FILENO;
use time::SystemTime;

use self::signal::SignalNumber;

mod audit;
// generalized traits for when we want to hide implementations
pub mod interface;

pub mod kernel;

pub mod file;

pub mod time;

pub mod timestamp;

pub mod signal;

pub mod term;

pub mod wait;

pub(crate) fn can_execute<P: AsRef<Path>>(path: P) -> bool {
    let Ok(path) = CString::new(path.as_ref().as_os_str().as_bytes()) else {
        return false;
    };

    // SAFETY: we are passing a proper pointer to access
    unsafe { libc::access(path.as_ptr(), libc::X_OK) == 0 }
}

pub(crate) fn _exit(status: libc::c_int) -> ! {
    // SAFETY: this function is safe to call
    unsafe { libc::_exit(status) }
}

/// A type able to close every file descriptor except for the ones pased via [`FileCloser::except`]
/// and the IO streams.
pub(crate) struct FileCloser {
    fds: BTreeSet<c_uint>,
}

impl FileCloser {
    pub(crate) const fn new() -> Self {
        Self {
            fds: BTreeSet::new(),
        }
    }

    pub(crate) fn except<F: AsRawFd>(&mut self, fd: &F) {
        self.fds.insert(fd.as_raw_fd() as c_uint);
    }

    /// Close every file descriptor that is not one of the IO streams or one of the file
    /// descriptors passed via [`FileCloser::except`].
    pub(crate) fn close_the_universe(self) -> io::Result<()> {
        let mut fds = self.fds.into_iter();

        let Some(mut curr_fd) = fds.next() else {
            return close_range(STDERR_FILENO as c_uint + 1, c_uint::MAX);
        };

        if let Some(max_fd) = curr_fd.checked_sub(1) {
            close_range(STDERR_FILENO as c_uint + 1, max_fd)?;
        }

        for next_fd in fds {
            if let Some(min_fd) = curr_fd.checked_add(1) {
                if let Some(max_fd) = next_fd.checked_sub(1) {
                    close_range(min_fd, max_fd)?;
                }
            }

            curr_fd = next_fd;
        }

        if let Some(min_fd) = curr_fd.checked_add(1) {
            close_range(min_fd, c_uint::MAX)?;
        }

        Ok(())
    }
}

fn close_range(min_fd: c_uint, max_fd: c_uint) -> io::Result<()> {
    if min_fd <= max_fd {
        // SAFETY: this function is safe to call:
        // - any errors while closing a specific fd will be effectively ignored
        // - if the provided range or flags are invalid, that will be reported
        //   as an error but will not cause undefined behaviour
        cerr(unsafe { libc::close_range(min_fd, max_fd, 0) })?;
    }

    Ok(())
}

pub(crate) enum ForkResult {
    // Parent process branch with the child process' PID.
    Parent(ProcessId),
    // Child process branch.
    Child,
}

/// Create a new process.
///
/// # Safety
///
/// Must not be called in multithreaded programs.
pub(crate) unsafe fn fork() -> io::Result<ForkResult> {
    // FIXME add debug assertion that we are not currently using multiple threads.
    let pid = cerr(unsafe { libc::fork() })?;
    if pid == 0 {
        Ok(ForkResult::Child)
    } else {
        Ok(ForkResult::Parent(pid))
    }
}

/// Create a new process with extra precautions for usage in tests.
///
/// # Safety
///
/// In a multithreaded program, only async-signal-safe functions are guaranteed to work in the
/// child process until a call to `execve` or a similar function is done.
#[cfg(test)]
unsafe fn fork_for_test() -> ForkResult {
    let result = fork().unwrap();
    if let ForkResult::Child = result {
        // Make sure that panics in the child always abort the process if it doesn't deadlock.
        // FIXME use std::panic::always_abort() once it is stable
        std::panic::set_hook(Box::new(|info| {
            use std::io::Write;
            let _ = writeln!(std::io::stderr(), "{info}");
            std::process::exit(101);
        }));
    }
    result
}

pub fn setsid() -> io::Result<ProcessId> {
    // SAFETY: this function is memory-safe to call
    cerr(unsafe { libc::setsid() })
}

#[derive(Clone)]
#[cfg_attr(test, derive(PartialEq))]
pub struct Hostname {
    inner: String,
}

impl fmt::Debug for Hostname {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Hostname").field(&self.inner).finish()
    }
}

impl fmt::Display for Hostname {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.inner)
    }
}

impl ops::Deref for Hostname {
    type Target = str;

    fn deref(&self) -> &str {
        &self.inner
    }
}

impl Hostname {
    #[cfg(test)]
    pub fn fake(hostname: &str) -> Self {
        Self {
            inner: hostname.to_string(),
        }
    }

    pub fn resolve() -> Self {
        // see `man 2 gethostname`
        const MAX_HOST_NAME_SIZE_ACCORDING_TO_SUSV2: libc::c_long = 255;

        // POSIX.1 systems limit hostnames to `HOST_NAME_MAX` bytes
        // not including null-byte in the count
        let max_hostname_size = sysconf(libc::_SC_HOST_NAME_MAX)
            .unwrap_or(MAX_HOST_NAME_SIZE_ACCORDING_TO_SUSV2)
            as usize;

        let buffer_size = max_hostname_size + 1 /* null byte delimiter */ ;
        let mut buf = vec![0; buffer_size];

        // SAFETY: we are passing a valid pointer to gethostname
        match cerr(unsafe { libc::gethostname(buf.as_mut_ptr(), buffer_size) }) {
            Ok(_) => Self {
                // SAFETY: gethostname succeeded, so `buf` will hold a null-terminated C string
                inner: unsafe { string_from_ptr(buf.as_ptr()) },
            },

            // ENAMETOOLONG is returned when hostname is greater than `buffer_size`
            Err(_) => {
                // but we have chosen a `buffer_size` larger than `max_hostname_size` so no truncation error is possible
                panic!("Unexpected error while retrieving hostname, this should not happen");
            }
        }
    }
}

pub fn syslog(priority: libc::c_int, facility: libc::c_int, message: &CStr) {
    const MSG: *const libc::c_char = match CStr::from_bytes_until_nul(b"%s\0") {
        Ok(cstr) => cstr.as_ptr(),
        Err(_) => panic!("syslog formatting string is not null-terminated"),
    };

    // SAFETY:
    // - "MSG" is a constant expression that is a null-terminated C string that represents "%s";
    //   this also means that to achieve safety we MUST pass one more argument to syslog that is a proper
    //   pointer to a null-terminated C string
    // - message.as_ptr() is a pointer to a proper null-terminated C string (message being a &CStr)
    // for more info: read the manpage for syslog(2)
    unsafe {
        libc::syslog(priority | facility, MSG, message.as_ptr());
    }
}

/// set target user and groups (uid, gid, additional groups) for a command
pub fn set_target_user(
    cmd: &mut std::process::Command,
    mut target_user: User,
    target_group: Group,
) {
    use std::os::unix::process::CommandExt;

    // add target group to list of additional groups if not present
    if !target_user.groups.contains(&target_group.gid) {
        target_user.groups.push(target_group.gid);
    }

    // we need to do this in a `pre_exec` call since the `groups` method in `process::Command` is unstable
    // see https://github.com/rust-lang/rust/blob/a01b4cc9f375f1b95fa8195daeea938d3d9c4c34/library/std/src/sys/unix/process/process_unix.rs#L329-L352
    // for the std implementation of the libc calls to `setgroups`, `setgid` and `setuid`
    unsafe {
        cmd.pre_exec(move || {
            cerr(libc::setgroups(
                target_user.groups.len(),
                target_user.groups.as_ptr(),
            ))?;
            cerr(libc::setgid(target_group.gid))?;
            cerr(libc::setuid(target_user.uid))?;

            Ok(())
        });
    }
}

/// Send a signal to a process with the specified ID.
pub fn kill(pid: ProcessId, signal: SignalNumber) -> io::Result<()> {
    // SAFETY: This function cannot cause UB even if `pid` is not a valid process ID or if
    // `signal` is not a valid signal code.
    cerr(unsafe { libc::kill(pid, signal) }).map(|_| ())
}

/// Send a signal to a process group with the specified ID.
pub fn killpg(pgid: ProcessId, signal: SignalNumber) -> io::Result<()> {
    // SAFETY: This function cannot cause UB even if `pgid` is not a valid process ID or if
    // `signal` is not a valid signal code.
    cerr(unsafe { libc::killpg(pgid, signal) }).map(|_| ())
}

/// Get the process group ID of the current process.
pub fn getpgrp() -> ProcessId {
    // SAFETY: This function is always safe to call
    unsafe { libc::getpgrp() }
}

/// Get a process group ID.
pub fn getpgid(pid: ProcessId) -> io::Result<ProcessId> {
    // SAFETY: This function cannot cause UB even if `pid` is not a valid process ID
    cerr(unsafe { libc::getpgid(pid) })
}

/// Set a process group ID.
pub fn setpgid(pid: ProcessId, pgid: ProcessId) -> io::Result<()> {
    // SAFETY: This function cannot cause UB even if `pid` or `pgid` are not a valid process IDs:
    // https://pubs.opengroup.org/onlinepubs/007904975/functions/setpgid.html
    cerr(unsafe { libc::setpgid(pid, pgid) }).map(|_| ())
}

pub fn chown<S: AsRef<CStr>>(
    path: &S,
    uid: impl Into<UserId>,
    gid: impl Into<GroupId>,
) -> io::Result<()> {
    let path = path.as_ref().as_ptr();
    let uid = uid.into();
    let gid = gid.into();

    // SAFETY: path is a valid pointer to a null-terminated C string; chown cannot cause safety
    // issues even if uid and/or gid would be invalid identifiers.
    cerr(unsafe { libc::chown(path, uid, gid) }).map(|_| ())
}

#[derive(Debug, Clone, PartialEq)]
pub struct User {
    pub uid: UserId,
    pub gid: GroupId,
    pub name: SudoString,
    pub gecos: String,
    pub home: SudoPath,
    pub shell: PathBuf,
    pub passwd: String,
    pub groups: Vec<GroupId>,
}

impl User {
    /// # Safety
    /// This function expects `pwd` to be a result from a succesful call to `getpwXXX_r`.
    /// (It can cause UB if any of `pwd`'s pointed-to strings does not have a null-terminator.)
    unsafe fn from_libc(pwd: &libc::passwd) -> Result<User, Error> {
        let mut buf_len: libc::c_int = 32;
        let mut groups_buffer: Vec<libc::gid_t>;

        while {
            groups_buffer = vec![0; buf_len as usize];
            // SAFETY: getgrouplist is passed valid pointers
            // in particular `groups_buffer` is an array of `buf.len()` bytes, as required
            let result = unsafe {
                libc::getgrouplist(
                    pwd.pw_name,
                    pwd.pw_gid,
                    groups_buffer.as_mut_ptr(),
                    &mut buf_len,
                )
            };

            result == -1
        } {
            if buf_len >= 65536 {
                panic!("user has too many groups (> 65536), this should not happen");
            }

            buf_len *= 2;
        }

        groups_buffer.resize_with(buf_len as usize, || {
            panic!("invalid groups count returned from getgrouplist, this should not happen")
        });

        Ok(User {
            uid: pwd.pw_uid,
            gid: pwd.pw_gid,
            name: SudoString::new(string_from_ptr(pwd.pw_name))?,
            gecos: string_from_ptr(pwd.pw_gecos),
            home: SudoPath::new(os_string_from_ptr(pwd.pw_dir).into())?,
            shell: os_string_from_ptr(pwd.pw_shell).into(),
            passwd: string_from_ptr(pwd.pw_passwd),
            groups: groups_buffer,
        })
    }

    pub fn from_uid(uid: UserId) -> Result<Option<User>, Error> {
        let max_pw_size = sysconf(libc::_SC_GETPW_R_SIZE_MAX).unwrap_or(16_384);
        let mut buf = vec![0; max_pw_size as usize];
        let mut pwd = MaybeUninit::uninit();
        let mut pwd_ptr = std::ptr::null_mut();
        // SAFETY: getpwuid_r is passed valid (although partly uninitialized) pointers to memory,
        // in particular `buf` points to an array of `buf.len()` bytes, as required.
        // After this call, if `pwd_ptr` is not NULL, `*pwd_ptr` and `pwd` will be aliased;
        // but we never dereference `pwd_ptr`.
        cerr(unsafe {
            libc::getpwuid_r(
                uid,
                pwd.as_mut_ptr(),
                buf.as_mut_ptr(),
                buf.len(),
                &mut pwd_ptr,
            )
        })?;
        if pwd_ptr.is_null() {
            Ok(None)
        } else {
            // SAFETY: pwd_ptr was not null, and getpwuid_r succeeded, so we have assurances that
            // the `pwd` structure was written to by getpwuid_r
            let pwd = unsafe { pwd.assume_init() };
            // SAFETY: `pwd` was obtained by a call to getpwXXX_r, as required.
            unsafe { Self::from_libc(&pwd).map(Some) }
        }
    }

    pub fn effective_uid() -> UserId {
        // SAFETY: this function cannot cause memory safety issues
        unsafe { libc::geteuid() }
    }

    pub fn effective_gid() -> GroupId {
        // SAFETY: this function cannot cause memory safety issues
        unsafe { libc::getegid() }
    }

    pub fn real_uid() -> UserId {
        // SAFETY: this function cannot cause memory safety issues
        unsafe { libc::getuid() }
    }

    pub fn real_gid() -> GroupId {
        // SAFETY: this function cannot cause memory safety issues
        unsafe { libc::getgid() }
    }

    pub fn real() -> Result<Option<User>, Error> {
        Self::from_uid(Self::real_uid())
    }

    pub fn from_name(name_c: &CStr) -> Result<Option<User>, Error> {
        let max_pw_size = sysconf(libc::_SC_GETPW_R_SIZE_MAX).unwrap_or(16_384);
        let mut buf = vec![0; max_pw_size as usize];
        let mut pwd = MaybeUninit::uninit();
        let mut pwd_ptr = std::ptr::null_mut();

        // SAFETY: analogous to getpwuid_r above
        cerr(unsafe {
            libc::getpwnam_r(
                name_c.as_ptr(),
                pwd.as_mut_ptr(),
                buf.as_mut_ptr(),
                buf.len(),
                &mut pwd_ptr,
            )
        })?;
        if pwd_ptr.is_null() {
            Ok(None)
        } else {
            // SAFETY: pwd_ptr was not null, and getpwnam_r succeeded, so we have assurances that
            // the `pwd` structure was written to by getpwnam_r
            let pwd = unsafe { pwd.assume_init() };
            // SAFETY: `pwd` was obtained by a call to getpwXXX_r, as required.
            unsafe { Self::from_libc(&pwd).map(Some) }
        }
    }
}

#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq))]
pub struct Group {
    pub gid: GroupId,
    pub name: String,
}

impl Group {
    /// # Safety
    /// This function expects `grp` to be a result from a succesful call to `getgrXXX_r`.
    /// In particular the grp.gr_mem pointer is assumed to be non-null, and pointing to a
    /// null-terminated list; the pointed-to strings are expected to be null-terminated.
    unsafe fn from_libc(grp: &libc::group) -> Group {
        Group {
            gid: grp.gr_gid,
            name: string_from_ptr(grp.gr_name),
        }
    }

    pub fn from_gid(gid: GroupId) -> std::io::Result<Option<Group>> {
        let max_gr_size = sysconf(libc::_SC_GETGR_R_SIZE_MAX).unwrap_or(16_384);
        let mut buf = vec![0; max_gr_size as usize];
        let mut grp = MaybeUninit::uninit();
        let mut grp_ptr = std::ptr::null_mut();
        // SAFETY: analogous to getpwuid_r above
        cerr(unsafe {
            libc::getgrgid_r(
                gid,
                grp.as_mut_ptr(),
                buf.as_mut_ptr(),
                buf.len(),
                &mut grp_ptr,
            )
        })?;
        if grp_ptr.is_null() {
            Ok(None)
        } else {
            // SAFETY: grp_ptr was not null, and getgrgid_r succeeded, so we have assurances that
            // the `grp` structure was written to by getgrgid_r
            let grp = unsafe { grp.assume_init() };
            // SAFETY: `pwd` was obtained by a call to getgrXXX_r, as required.
            Ok(Some(unsafe { Group::from_libc(&grp) }))
        }
    }

    pub fn from_name(name_c: &CStr) -> std::io::Result<Option<Group>> {
        let max_gr_size = sysconf(libc::_SC_GETGR_R_SIZE_MAX).unwrap_or(16_384);
        let mut buf = vec![0; max_gr_size as usize];
        let mut grp = MaybeUninit::uninit();
        let mut grp_ptr = std::ptr::null_mut();
        // SAFETY: analogous to getpwuid_r above
        cerr(unsafe {
            libc::getgrnam_r(
                name_c.as_ptr(),
                grp.as_mut_ptr(),
                buf.as_mut_ptr(),
                buf.len(),
                &mut grp_ptr,
            )
        })?;
        if grp_ptr.is_null() {
            Ok(None)
        } else {
            // SAFETY: grp_ptr was not null, and getgrgid_r succeeded, so we have assurances that
            // the `grp` structure was written to by getgrgid_r
            let grp = unsafe { grp.assume_init() };
            // SAFETY: `pwd` was obtained by a call to getgrXXX_r, as required.
            Ok(Some(unsafe { Group::from_libc(&grp) }))
        }
    }
}

pub enum WithProcess {
    Current,
    Other(ProcessId),
}

impl WithProcess {
    fn to_proc_string(&self) -> String {
        match self {
            WithProcess::Current => "self".into(),
            WithProcess::Other(pid) => pid.to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Process {
    pub pid: ProcessId,
    pub parent_pid: Option<ProcessId>,
    pub session_id: ProcessId,
}

impl Default for Process {
    fn default() -> Self {
        Self::new()
    }
}

impl Process {
    pub fn new() -> Process {
        Process {
            pid: Self::process_id(),
            parent_pid: Self::parent_id(),
            session_id: Self::session_id(),
        }
    }

    /// Return the process identifier for the current process
    pub fn process_id() -> ProcessId {
        // NOTE libstd casts the `i32` that `libc::getpid` returns into `u32`
        // here we cast it back into `i32` (`ProcessId`)
        std::process::id() as ProcessId
    }

    /// Return the parent process identifier for the current process
    pub fn parent_id() -> Option<ProcessId> {
        // NOTE libstd casts the `i32` that `libc::getppid` returns into `u32`
        // here we cast it back into `i32` (`ProcessId`)
        let pid = unix::process::parent_id() as ProcessId;
        if pid == 0 {
            None
        } else {
            Some(pid)
        }
    }

    /// Get the session id for the current process
    pub fn session_id() -> ProcessId {
        // SAFETY: this function is explicitly safe to call with argument 0,
        // and more generally getsid will never cause memory safety issues.
        unsafe { libc::getsid(0) }
    }

    /// Returns the device identifier of the TTY device that is currently
    /// attached to the given process
    pub fn tty_device_id(pid: WithProcess) -> std::io::Result<Option<DeviceId>> {
        // device id of tty is displayed as a signed integer of 32 bits
        let data: i32 = read_proc_stat(pid, 6 /* tty_nr */)?;
        if data == 0 {
            Ok(None)
        } else {
            // While the integer was displayed as signed in the proc stat file,
            // we actually need to interpret the bits of that integer as an unsigned
            // int. We convert via u32 because a direct conversion to DeviceId
            // would use sign extension, which would result in a different bit
            // representation
            Ok(Some(data as u32 as DeviceId))
        }
    }

    /// Get the process starting time of a specific process
    pub fn starting_time(pid: WithProcess) -> io::Result<SystemTime> {
        let process_start: u64 = read_proc_stat(pid, 21 /* start_time */)?;

        // the startime field is stored in ticks since the system start, so we need to know how many
        // ticks go into a second
        let ticks_per_second = crate::cutils::sysconf(libc::_SC_CLK_TCK).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Other,
                "Could not retrieve system config variable for ticks per second",
            )
        })? as u64;

        // finally compute the system time at which the process was started
        Ok(SystemTime::new(
            (process_start / ticks_per_second) as i64,
            ((process_start % ticks_per_second) * (1_000_000_000 / ticks_per_second)) as i64,
        ))
    }
}

/// Read the n-th field (with 1-based indexing) from `/proc/<pid>/self`.
///
/// See ["Table 1-4: Contents of the stat fields" of "The /proc
/// Filesystem"][proc_stat_fields] in the Linux docs for all available fields.
///
/// [proc_stat_fields]: https://www.kernel.org/doc/html/latest/filesystems/proc.html#id10
fn read_proc_stat<T: FromStr>(pid: WithProcess, field_idx: isize) -> io::Result<T> {
    // read from a specific pid file, or use `self` to refer to our own process
    let pidref = pid.to_proc_string();

    // read the data from the stat file for the process with the given pid
    let path = PathBuf::from_iter(&["/proc", &pidref, "stat"]);
    let proc_stat = std::fs::read(path)?;

    // first get the part of the stat file past the second argument, we then reverse
    // search for a ')' character and start the search for the starttime field from there on
    let skip_past_second_arg = proc_stat.iter().rposition(|b| *b == b')').ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Could not find position of 'comm' field in process stat",
        )
    })?;
    let mut stat = &proc_stat[skip_past_second_arg..];

    // we've now passed the first two fields, so we are at index 1, now we skip over
    // fields until we arrive at the field we are searching for
    let mut curr_field = 1;
    while curr_field < field_idx && !stat.is_empty() {
        if stat[0] == b' ' {
            curr_field += 1;
        }
        stat = &stat[1..];
    }

    // The expected field cannot be in the file anymore when we are at EOF
    if stat.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Stat file was not of the expected format",
        ));
    }

    // we've now arrived at the field we are looking for, we now check how
    // long this field is by finding where the next space is
    let mut idx = 0;
    while stat[idx] != b' ' && idx < stat.len() {
        idx += 1;
    }
    let field = &stat[0..idx];

    // we first convert the data to a string slice, this should not fail with a normal /proc filesystem
    let fielddata = std::str::from_utf8(field).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Could not interpret byte slice as string",
        )
    })?;

    // then we convert the string slice to whatever the requested type was
    fielddata.parse().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Could not interpret string as number",
        )
    })
}

pub fn escape_os_str_lossy(s: &std::ffi::OsStr) -> String {
    s.to_string_lossy().escape_default().collect()
}

pub fn make_zeroed_sigaction() -> libc::sigaction {
    // SAFETY: since sigaction is a C struct, all-zeroes is a valid representation
    // We cannot use a "literal struct" initialization method since the exact representation
    // of libc::sigaction is not fixed, see e.g. https://github.com/trifectatechfoundation/sudo-rs/issues/829
    unsafe { std::mem::zeroed() }
}

#[cfg(all(test, target_os = "linux"))]
pub(crate) const ROOT_GROUP_NAME: &str = "root";

#[cfg(all(test, not(target_os = "linux")))]
pub(crate) const ROOT_GROUP_NAME: &str = "wheel";

#[cfg(test)]
mod tests {
    use std::{
        io::{self, Read, Write},
        os::{fd::AsRawFd, unix::net::UnixStream},
        process::exit,
    };

    use libc::SIGKILL;

    use super::{
        fork_for_test, getpgrp, setpgid,
        wait::{Wait, WaitOptions},
        ForkResult, Group, User, WithProcess, ROOT_GROUP_NAME,
    };

    pub(super) fn tempfile() -> std::io::Result<std::fs::File> {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("Failed to get system time")
            .as_nanos();
        let pid = std::process::id();

        let filename = format!("sudo_rs_test_{}_{}", pid, timestamp);
        let path = std::path::PathBuf::from("/tmp").join(filename);
        std::fs::File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
    }

    #[test]
    fn test_get_user_and_group_by_id() {
        let fixed_users = &[
            (0, "root"),
            (
                User::from_name(cstr!("daemon")).unwrap().unwrap().uid,
                "daemon",
            ),
        ];
        for &(id, name) in fixed_users {
            let root = User::from_uid(id).unwrap().unwrap();
            assert_eq!(root.uid, id as libc::uid_t);
            assert_eq!(root.name, name);
        }
        let fixed_groups = &[
            (0, ROOT_GROUP_NAME),
            (
                Group::from_name(cstr!("daemon")).unwrap().unwrap().gid,
                "daemon",
            ),
        ];
        for &(id, name) in fixed_groups {
            let root = Group::from_gid(id).unwrap().unwrap();
            assert_eq!(root.gid, id as libc::gid_t);
            assert_eq!(root.name, name);
        }
    }

    #[test]
    fn miri_test_group_impl() {
        use super::Group;
        use std::ffi::CString;

        fn test(name: &str, passwd: &str, gid: libc::gid_t, mem: &[&str]) {
            assert_eq!(
                {
                    let c_mem: Vec<CString> =
                        mem.iter().map(|&s| CString::new(s).unwrap()).collect();
                    let c_name = CString::new(name).unwrap();
                    let c_passwd = CString::new(passwd).unwrap();
                    unsafe {
                        Group::from_libc(&libc::group {
                            gr_name: c_name.as_ptr() as *mut _,
                            gr_passwd: c_passwd.as_ptr() as *mut _,
                            gr_gid: gid,
                            gr_mem: c_mem
                                .iter()
                                .map(|cs| cs.as_ptr() as *mut _)
                                .chain(std::iter::once(std::ptr::null_mut()))
                                .collect::<Vec<*mut libc::c_char>>()
                                .as_mut_ptr(),
                        })
                    }
                },
                Group {
                    name: name.to_string(),
                    gid,
                }
            )
        }

        test("dr. bill", "fidelio", 1999, &["eyes", "wide", "shut"]);
        test("eris", "fnord", 5, &[]);
        test("abc", "password123", 42, &[""]);
    }

    #[test]
    fn get_process_tty_device() {
        assert!(super::Process::tty_device_id(WithProcess::Current).is_ok());
    }

    #[test]
    fn get_process_start_time() {
        let time = super::Process::starting_time(WithProcess::Current).unwrap();
        let now = super::SystemTime::now().unwrap();
        assert!(time > now - super::time::Duration::minutes(24 * 60));
        assert!(time < now);
    }

    #[test]
    fn pgid_test() {
        use super::{getpgid, setpgid};

        let pgrp = getpgrp();
        assert_eq!(getpgid(0).unwrap(), pgrp);
        assert_eq!(getpgid(std::process::id() as i32).unwrap(), pgrp);

        // FIXME fork will deadlock when this test panics if it forked while
        // another test was panicking.
        match unsafe { super::fork_for_test() } {
            ForkResult::Child => {
                // wait for the parent.
                std::thread::sleep(std::time::Duration::from_secs(1))
            }
            ForkResult::Parent(child_pid) => {
                // The child should be in our process group.
                assert_eq!(getpgid(child_pid).unwrap(), getpgid(0).unwrap(),);
                // Move the child to its own process group
                setpgid(child_pid, child_pid).unwrap();
                // The process group of the child should have changed.
                assert_eq!(getpgid(child_pid).unwrap(), child_pid);
            }
        }
    }
    #[test]
    fn kill_test() {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("1")
            .spawn()
            .unwrap();
        super::kill(child.id() as i32, SIGKILL).unwrap();
        assert!(!child.wait().unwrap().success());
    }
    #[test]
    fn killpg_test() {
        // Create a socket so the children write to it if they aren't terminated by `killpg`.
        let (mut rx, mut tx) = UnixStream::pair().unwrap();

        // FIXME fork will deadlock when this test panics if it forked while
        // another test was panicking.
        let ForkResult::Parent(pid1) = (unsafe { fork_for_test() }) else {
            std::thread::sleep(std::time::Duration::from_secs(1));
            tx.write_all(&[42]).unwrap();
            exit(0);
        };

        // FIXME fork will deadlock when this test panics if it forked while
        // another test was panicking.
        let ForkResult::Parent(pid2) = (unsafe { fork_for_test() }) else {
            std::thread::sleep(std::time::Duration::from_secs(1));
            tx.write_all(&[42]).unwrap();
            exit(0);
        };

        drop(tx);

        let pgid = pid1;
        // Move the children to their own process group.
        setpgid(pid1, pgid).unwrap();
        setpgid(pid2, pgid).unwrap();
        // Send `SIGKILL` to the children process group.
        super::killpg(pgid, SIGKILL).unwrap();
        // Ensure that the child were terminated before writing.
        assert_eq!(
            rx.read_exact(&mut [0; 2]).unwrap_err().kind(),
            std::io::ErrorKind::UnexpectedEof
        );
    }

    fn is_closed<F: AsRawFd>(fd: &F) -> bool {
        crate::cutils::cerr(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) })
            .is_err_and(|err| err.raw_os_error() == Some(libc::EBADF))
    }

    #[test]
    fn close_the_universe() {
        // FIXME fork will deadlock when this test panics if it forked while
        // another test was panicking.
        let ForkResult::Parent(child_pid) = (unsafe { fork_for_test() }) else {
            let should_close =
                std::fs::File::create(std::env::temp_dir().join("should_close.txt")).unwrap();
            assert!(!is_closed(&should_close));

            let should_not_close =
                std::fs::File::create(std::env::temp_dir().join("should_not_close.txt")).unwrap();
            assert!(!is_closed(&should_not_close));

            let mut closer = super::FileCloser::new();

            closer.except(&should_not_close);

            closer.close_the_universe().unwrap();

            assert!(is_closed(&should_close));

            assert!(!is_closed(&io::stdin()));
            assert!(!is_closed(&io::stdout()));
            assert!(!is_closed(&io::stderr()));
            assert!(!is_closed(&should_not_close));

            exit(0)
        };

        let (_, status) = child_pid.wait(WaitOptions::new()).unwrap();
        assert_eq!(status.exit_status(), Some(0));
    }

    #[test]
    fn except_stdio_is_fine() {
        // FIXME fork will deadlock when this test panics if it forked while
        // another test was panicking.
        let ForkResult::Parent(child_pid) = (unsafe { fork_for_test() }) else {
            let mut closer = super::FileCloser::new();

            closer.except(&io::stdin());
            closer.except(&io::stdout());
            closer.except(&io::stderr());

            closer.close_the_universe().unwrap();

            assert!(!is_closed(&io::stdin()));
            assert!(!is_closed(&io::stdout()));
            assert!(!is_closed(&io::stderr()));

            exit(0)
        };

        let (_, status) = child_pid.wait(WaitOptions::new()).unwrap();
        assert_eq!(status.exit_status(), Some(0));
    }
}
