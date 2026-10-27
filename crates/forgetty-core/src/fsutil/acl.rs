//! User-only files and folders. The client's identity key, `client_id`, machine list and
//! address cache, and the server's state folder, live in places only the current user may read.
//! On Windows each gets a protected DACL (inheritance off) with one entry: the current user, full
//! access. On Unix the mode is 0600. Moved here from the Windows client so that the server uses
//! the same code.

use std::io;
use std::path::Path;

/// Write `bytes` to `path` so that only the current user can read the file at
/// any time after it exists: write a temporary file, restrict it, flush it to disk, then
/// rename it over `path` (the DACL travels with the file) and flush the rename too. A power
/// cut leaves the old file or the new one, never an empty one.
pub fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    // Fresh temporary every time: a stale one may carry another DACL.
    let _ = std::fs::remove_file(&tmp);
    let written = (|| {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        // The data must be on disk before the rename makes it the file.
        f.sync_all()
    })();
    if let Err(e) = written.and_then(|()| restrict_to_user(&tmp)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    replace_file(&tmp, path)?;
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => sync_dir(dir),
        _ => Ok(()),
    }
}

/// Rename `tmp` over `to`.
#[cfg(unix)]
fn replace_file(tmp: &Path, to: &Path) -> io::Result<()> {
    std::fs::rename(tmp, to)
}

/// Flush a directory, so a rename in it survives a power cut.
#[cfg(unix)]
fn sync_dir(dir: &Path) -> io::Result<()> {
    std::fs::File::open(dir)?.sync_all()
}

/// Create `path` new (it must not exist: `AlreadyExists` otherwise) with `bytes`, readable by
/// the current user only from the first moment: the security descriptor is part of the create
/// call. Two processes racing to create the same file cannot both win.
#[cfg(unix)]
pub fn create_private_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
    f.write_all(bytes)
}

#[cfg(unix)]
pub fn restrict_to_user(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

/// The ACL of `path` as an SDDL string (Windows), or the octal mode (Unix).
#[cfg(unix)]
pub fn describe(path: &Path) -> io::Result<String> {
    use std::os::unix::fs::PermissionsExt;
    Ok(format!("{:o}", std::fs::metadata(path)?.permissions().mode() & 0o777))
}

/// True when `path` is readable by the current user only.
#[cfg(unix)]
pub fn is_user_only(path: &Path) -> io::Result<bool> {
    Ok(describe(path)? == "600")
}

#[cfg(windows)]
pub use win::{
    create_private_file, create_private_new, current_integrity_rid, current_user_sid, describe,
    is_elevated, is_user_only, only_sid_sddl, pipe_client_identity, pipe_sddl, process_user_sid,
    replace_file, restrict_dir_to_user, restrict_to_user, same_account, sync_dir, INTEGRITY_HIGH,
    INTEGRITY_MEDIUM,
};

/// Like [`restrict_to_user`] for a folder. On Unix the mode is 0700.
#[cfg(unix)]
pub fn restrict_dir_to_user(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

#[cfg(windows)]
mod win {
    use std::ffi::c_void;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    use windows::core::{PCWSTR, PWSTR};
    use windows::core::BOOL;
    use windows::Win32::Foundation::{CloseHandle, LocalFree, ERROR_SUCCESS, HANDLE, HLOCAL};
    use windows::Win32::Security::SECURITY_ATTRIBUTES;
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, WriteFile, CREATE_NEW, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_WRITE, FILE_SHARE_READ,
    };
    use windows::Win32::Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW, ConvertSidToStringSidW,
        ConvertStringSecurityDescriptorToSecurityDescriptorW, ConvertStringSidToSidW, GetNamedSecurityInfoW, SetNamedSecurityInfoW,
        SDDL_REVISION_1, SE_FILE_OBJECT,
    };
    use windows::Win32::Security::{
        EqualSid, GetSecurityDescriptorDacl, GetTokenInformation, TokenUser, ACL, DACL_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, TOKEN_QUERY, TOKEN_USER, PSID,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    fn err(e: windows::core::Error) -> io::Error {
        // A Win32 error comes as HRESULT 0x8007xxxx: keep the code, so the kind is right.
        let h = e.code().0 as u32;
        if h & 0xFFFF_0000 == 0x8007_0000 {
            io::Error::from_raw_os_error((h & 0xFFFF) as i32)
        } else {
            io::Error::other(e.to_string())
        }
    }

    /// Rename `tmp` over `to` and wait until the move is on disk (`MOVEFILE_WRITE_THROUGH`).
    ///
    /// Windows refuses the replace with "access denied" or "sharing violation" while another
    /// thread is replacing the same file or a reader (an indexer, a virus scanner) has it open
    /// for a moment. Those errors go away within milliseconds, so the call is repeated.
    pub fn replace_file(tmp: &Path, to: &Path) -> io::Result<()> {
        use windows::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH};
        let (from, dest) = (wide(tmp.as_os_str()), wide(to.as_os_str()));
        let mut tries = 0u32;
        loop {
            // SAFETY: both buffers are NUL-terminated and live for the call.
            let r = unsafe {
                MoveFileExW(PCWSTR(from.as_ptr()), PCWSTR(dest.as_ptr()), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)
            };
            match r {
                Ok(()) => return Ok(()),
                Err(e) => {
                    let e = err(e);
                    // 5 = ERROR_ACCESS_DENIED, 32 = ERROR_SHARING_VIOLATION, 33 = ERROR_LOCK_VIOLATION.
                    let busy = matches!(e.raw_os_error(), Some(5 | 32 | 33));
                    tries += 1;
                    if !busy || tries > 200 {
                        return Err(e);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        }
    }

    /// Windows cannot flush a directory handle; `replace_file` already waited for the move.
    pub fn sync_dir(_dir: &Path) -> io::Result<()> {
        Ok(())
    }

    /// The user SID of an access token, in string form (`S-1-5-21-...`).
    fn token_user_sid(token: HANDLE) -> io::Result<String> {
        // SAFETY: plain Win32 calls on a valid token handle with buffers sized by the API.
        unsafe {
            let mut len = 0u32;
            // First call reports the size.
            let _ = GetTokenInformation(token, TokenUser, None, 0, &mut len);
            if len == 0 {
                return Err(io::Error::other("GetTokenInformation returned no size"));
            }
            let mut buf = vec![0u8; len as usize];
            GetTokenInformation(token, TokenUser, Some(buf.as_mut_ptr() as *mut c_void), len, &mut len)
                .map_err(err)?;
            let user = &*(buf.as_ptr() as *const TOKEN_USER);
            let mut s = PWSTR::null();
            ConvertSidToStringSidW(user.User.Sid, &mut s).map_err(err)?;
            let out = s.to_string().map_err(|e| io::Error::other(e.to_string()));
            let _ = LocalFree(Some(HLOCAL(s.0 as *mut c_void)));
            out
        }
    }

    /// The current user's SID in string form (`S-1-5-21-...`).
    pub fn current_user_sid() -> io::Result<String> {
        // SAFETY: the pseudo handle of this process is valid; the token is closed after use.
        unsafe {
            let mut token = HANDLE::default();
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).map_err(err)?;
            let result = token_user_sid(token);
            let _ = CloseHandle(token);
            result
        }
    }

    /// The user SID of the process `pid`. Fails (access denied) for a process of another user and
    /// for an elevated process when this one is not elevated.
    pub fn process_user_sid(pid: u32) -> io::Result<String> {
        use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
        // SAFETY: handles are closed on every path.
        unsafe {
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).map_err(err)?;
            let mut token = HANDLE::default();
            let opened = OpenProcessToken(process, TOKEN_QUERY, &mut token).map_err(err);
            let _ = CloseHandle(process);
            opened?;
            let result = token_user_sid(token);
            let _ = CloseHandle(token);
            result
        }
    }

    /// Mandatory integrity levels (the RID of the label SID).
    pub const INTEGRITY_MEDIUM: u32 = 0x2000;
    pub const INTEGRITY_HIGH: u32 = 0x3000;

    /// The integrity level (RID of the mandatory label) of an access token.
    fn token_integrity_rid(token: HANDLE) -> io::Result<u32> {
        use windows::Win32::Security::{
            GetSidSubAuthority, GetSidSubAuthorityCount, TokenIntegrityLevel, TOKEN_MANDATORY_LABEL,
        };
        // SAFETY: buffers are sized by the API; the SID pointers stay inside `buf`.
        unsafe {
            let mut len = 0u32;
            let _ = GetTokenInformation(token, TokenIntegrityLevel, None, 0, &mut len);
            if len == 0 {
                return Err(io::Error::other("GetTokenInformation returned no size"));
            }
            let mut buf = vec![0u8; len as usize];
            GetTokenInformation(token, TokenIntegrityLevel, Some(buf.as_mut_ptr() as *mut c_void), len, &mut len)
                .map_err(err)?;
            let label = &*(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL);
            let sid = label.Label.Sid;
            let count = *GetSidSubAuthorityCount(sid);
            if count == 0 {
                return Err(io::Error::other("integrity label without a sub authority"));
            }
            Ok(*GetSidSubAuthority(sid, u32::from(count) - 1))
        }
    }

    /// The integrity level of this process: 0x2000 medium (a normal process), 0x3000 high (an
    /// elevated one), 0x4000 system.
    pub fn current_integrity_rid() -> io::Result<u32> {
        // SAFETY: the pseudo handle of this process is valid; the token is closed after use.
        unsafe {
            let mut token = HANDLE::default();
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).map_err(err)?;
            let result = token_integrity_rid(token);
            let _ = CloseHandle(token);
            result
        }
    }

    /// Does this process run elevated (high integrity or above)? When it cannot be told, yes:
    /// the stricter rules are the safe answer.
    pub fn is_elevated() -> bool {
        current_integrity_rid().map(|r| r >= INTEGRITY_HIGH).unwrap_or(true)
    }

    /// The user SID and the integrity level of the client on the other end of a named pipe, read
    /// by impersonating the client for a moment on this thread. Works for an elevated client,
    /// which a process-id lookup cannot. Windows allows it only after the server read from the
    /// pipe once (`ERROR_CANNOT_IMPERSONATE` before), so call it after the first read.
    pub fn pipe_client_identity(pipe: HANDLE) -> io::Result<(String, u32)> {
        use windows::Win32::Security::RevertToSelf;
        use windows::Win32::System::Pipes::ImpersonateNamedPipeClient;
        use windows::Win32::System::Threading::{GetCurrentThread, OpenThreadToken};

        /// Reverts on every exit path: a thread must never stay in the client's identity.
        struct Revert;
        impl Drop for Revert {
            fn drop(&mut self) {
                // SAFETY: no preconditions. If this failed the thread would keep the client's
                // identity, so stop the process rather than run on as someone else.
                if unsafe { RevertToSelf() }.is_err() {
                    std::process::abort();
                }
            }
        }

        // SAFETY: the pipe handle is valid for the call; the thread token is closed.
        unsafe {
            ImpersonateNamedPipeClient(pipe).map_err(err)?;
            let _revert = Revert;
            let mut token = HANDLE::default();
            // `true`: open with the identity of the process, so the check does not depend on
            // what the client's token allows.
            OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, true, &mut token).map_err(err)?;
            let result = token_user_sid(token).and_then(|sid| Ok((sid, token_integrity_rid(token)?)));
            let _ = CloseHandle(token);
            result
        }
    }

    /// Do the two SID strings name the same account?
    pub fn same_account(a: &str, b: &str) -> bool {
        same_sid(a, b)
    }

    /// An SDDL string for a pipe or other kernel object that only `sid` may use.
    pub fn only_sid_sddl(sid: &str) -> String {
        format!("D:P(A;;GA;;;{sid})")
    }

    /// Like [`only_sid_sddl`], and for a process at high integrity also a mandatory label that
    /// stops processes of a lower level from writing to, reading or executing the object (the
    /// default label only stops writes). An elevated server must not be reachable from a normal
    /// process of the same user: that would be a way around the UAC prompt.
    pub fn pipe_sddl(sid: &str, high_integrity: bool) -> String {
        if high_integrity {
            format!("D:P(A;;GA;;;{sid})S:(ML;;NWNRNX;;;HI)")
        } else {
            only_sid_sddl(sid)
        }
    }

    /// Create `path` new with `bytes`, readable by the current user only from the first moment
    /// (`CreateFileW` with `CREATE_NEW` and a security descriptor). `AlreadyExists` when the
    /// file is there, so two first runs cannot overwrite each other's key.
    pub fn create_private_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let sid = current_user_sid()?;
        let sddl: Vec<u16> = format!("D:P(A;;FA;;;{sid})").encode_utf16().chain(std::iter::once(0)).collect();
        let name = wide(path.as_os_str());
        unsafe {
            let mut sd = PSECURITY_DESCRIPTOR::default();
            ConvertStringSecurityDescriptorToSecurityDescriptorW(PCWSTR(sddl.as_ptr()), SDDL_REVISION_1, &mut sd, None)
                .map_err(err)?;
            let sa = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: sd.0,
                bInheritHandle: BOOL(0),
            };
            let h = CreateFileW(
                PCWSTR(name.as_ptr()),
                FILE_GENERIC_WRITE.0,
                FILE_SHARE_READ,
                Some(&sa),
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL,
                None,
            );
            let result = match h {
                Err(_) => Err(io::Error::last_os_error()),
                Ok(h) => {
                    let mut written = 0u32;
                    let w = WriteFile(h, Some(bytes), Some(&mut written), None);
                    let _ = CloseHandle(h);
                    let r = match w {
                        Ok(()) if written as usize == bytes.len() => Ok(()),
                        Ok(()) => Err(io::Error::other("short write")),
                        Err(e) => Err(err(e)),
                    };
                    // Only a file this call created may be removed. A failed CreateFileW (the file
                    // exists) must leave the winner's file alone.
                    if r.is_err() {
                        let _ = std::fs::remove_file(path);
                    }
                    r
                }
            };
            let _ = LocalFree(Some(HLOCAL(sd.0)));
            result
        }
    }

    /// Replace the DACL of `path` with one entry: the current user, full
    /// access, inheritance off.
    pub fn restrict_to_user(path: &Path) -> io::Result<()> {
        let sid = current_user_sid()?;
        apply_dacl(path, &format!("D:P(A;;FA;;;{sid})"))
    }

    /// Like [`restrict_to_user`] for a folder: files and folders created in it later inherit the
    /// one entry (`OICI`), so a file made in the state folder is user-only from its first moment.
    pub fn restrict_dir_to_user(path: &Path) -> io::Result<()> {
        let sid = current_user_sid()?;
        apply_dacl(path, &format!("D:P(A;OICI;FA;;;{sid})"))
    }

    fn apply_dacl(path: &Path, sddl: &str) -> io::Result<()> {
        let sddl: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
        let name = wide(path.as_os_str());
        unsafe {
            let mut sd = PSECURITY_DESCRIPTOR::default();
            ConvertStringSecurityDescriptorToSecurityDescriptorW(PCWSTR(sddl.as_ptr()), SDDL_REVISION_1, &mut sd, None)
                .map_err(err)?;
            let result = (|| {
                let mut present = BOOL(0);
                let mut defaulted = BOOL(0);
                let mut dacl: *mut ACL = std::ptr::null_mut();
                GetSecurityDescriptorDacl(sd, &mut present, &mut dacl, &mut defaulted).map_err(err)?;
                if !present.as_bool() || dacl.is_null() {
                    return Err(io::Error::other("no DACL in the descriptor"));
                }
                let rc = SetNamedSecurityInfoW(
                    PCWSTR(name.as_ptr()),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                    None,
                    None,
                    Some(dacl as *const ACL),
                    None,
                );
                if rc != ERROR_SUCCESS {
                    return Err(io::Error::from_raw_os_error(rc.0 as i32));
                }
                Ok(())
            })();
            let _ = LocalFree(Some(HLOCAL(sd.0)));
            result
        }
    }

    /// Create `path` new and return it open for reading and writing. Like
    /// [`create_private_new`] the descriptor is part of the create call, so the file is user-only
    /// from the first moment. `AlreadyExists` when the file is there. The share mode is the one
    /// `std::fs::File::create` uses (read, write and delete), so a rename of the file works.
    pub fn create_private_file(path: &Path) -> io::Result<std::fs::File> {
        use std::os::windows::io::FromRawHandle;
        use windows::Win32::Storage::FileSystem::{
            FILE_GENERIC_READ, FILE_SHARE_DELETE, FILE_SHARE_WRITE,
        };
        let sid = current_user_sid()?;
        let sddl: Vec<u16> = format!("D:P(A;;FA;;;{sid})").encode_utf16().chain(std::iter::once(0)).collect();
        let name = wide(path.as_os_str());
        unsafe {
            let mut sd = PSECURITY_DESCRIPTOR::default();
            ConvertStringSecurityDescriptorToSecurityDescriptorW(PCWSTR(sddl.as_ptr()), SDDL_REVISION_1, &mut sd, None)
                .map_err(err)?;
            let sa = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: sd.0,
                bInheritHandle: BOOL(0),
            };
            let h = CreateFileW(
                PCWSTR(name.as_ptr()),
                FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                Some(&sa),
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL,
                None,
            );
            let result = match h {
                Err(_) => Err(io::Error::last_os_error()),
                Ok(h) => Ok(std::fs::File::from_raw_handle(h.0)),
            };
            let _ = LocalFree(Some(HLOCAL(sd.0)));
            result
        }
    }

    /// The DACL of `path` as an SDDL string, for example
    /// `D:P(A;;FA;;;S-1-5-21-...)`.
    pub fn describe(path: &Path) -> io::Result<String> {
        let name = wide(path.as_os_str());
        unsafe {
            let mut sd = PSECURITY_DESCRIPTOR::default();
            let rc = GetNamedSecurityInfoW(
                PCWSTR(name.as_ptr()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                None,
                None,
                None,
                None,
                &mut sd,
            );
            if rc != ERROR_SUCCESS {
                return Err(io::Error::from_raw_os_error(rc.0 as i32));
            }
            let mut s = PWSTR::null();
            let r = ConvertSecurityDescriptorToStringSecurityDescriptorW(
                sd,
                SDDL_REVISION_1,
                DACL_SECURITY_INFORMATION,
                &mut s,
                None,
            );
            let out = match r {
                Ok(()) => s.to_string().map_err(|e| io::Error::other(e.to_string())),
                Err(e) => Err(err(e)),
            };
            if !s.is_null() {
                let _ = LocalFree(Some(HLOCAL(s.0 as *mut c_void)));
            }
            let _ = LocalFree(Some(HLOCAL(sd.0)));
            out
        }
    }

    fn parse_sid(text: &str) -> Option<PSID> {
        let w: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let mut sid = PSID::default();
        unsafe { ConvertStringSidToSidW(PCWSTR(w.as_ptr()), &mut sid).ok()? };
        Some(sid)
    }

    /// SDDL prints some SIDs as two letter aliases (the built in administrator
    /// account is `LA`), so compare as SIDs and not as text.
    fn same_sid(a: &str, b: &str) -> bool {
        if a == b {
            return true;
        }
        let (Some(x), Some(y)) = (parse_sid(a), parse_sid(b)) else { return false };
        let eq = unsafe { EqualSid(x, y).is_ok() };
        unsafe {
            let _ = LocalFree(Some(HLOCAL(x.0)));
            let _ = LocalFree(Some(HLOCAL(y.0)));
        }
        eq
    }

    /// `D:` header with the P (protected) flag, exactly one ACE, an allow entry
    /// for `sid`.
    pub(super) fn sddl_is_user_only(sddl: &str, sid: &str) -> bool {
        let Some(rest) = sddl.strip_prefix("D:") else { return false };
        let (flags, aces) = match rest.find('(') {
            Some(i) => rest.split_at(i),
            None => return false,
        };
        if !(flags.contains('P') && aces.matches('(').count() == 1 && aces.starts_with("(A;") && aces.ends_with(')')) {
            return false;
        }
        let fields: Vec<&str> = aces[1..aces.len() - 1].split(';').collect();
        fields.len() >= 6 && same_sid(fields[5], sid)
    }

    /// True when the DACL is protected and holds exactly one allow entry for
    /// the current user.
    pub fn is_user_only(path: &Path) -> io::Result<bool> {
        let sid = current_user_sid()?;
        let got = describe(path)?;
        Ok(sddl_is_user_only(&got, &sid))
    }
}

#[cfg(all(test, windows))]
mod win_tests {
    #[test]
    fn sddl_check() {
        assert!(super::win::sddl_is_user_only("D:P(A;;FA;;;S-1-5-21-1)", "S-1-5-21-1"));
        assert!(!super::win::sddl_is_user_only("D:(A;;FA;;;S-1-5-21-1)", "S-1-5-21-1"));
        assert!(!super::win::sddl_is_user_only("D:P(A;;FA;;;S-1-5-21-1)(A;;FA;;;SY)", "S-1-5-21-1"));
    }
}

#[cfg(all(test, windows))]
mod sid_tests {
    use super::*;

    #[test]
    fn this_process_is_owned_by_the_current_user() {
        let me = current_user_sid().unwrap();
        assert!(me.starts_with("S-1-5-"), "{me}");
        assert_eq!(process_user_sid(std::process::id()).unwrap(), me);
        assert!(same_account(&me, &me));
        assert!(!same_account(&me, "S-1-5-18"));
    }

    #[test]
    fn the_pipe_sddl_names_one_sid_and_labels_a_high_server() {
        assert_eq!(only_sid_sddl("S-1-5-21-1-2-3-1001"), "D:P(A;;GA;;;S-1-5-21-1-2-3-1001)");
        assert_eq!(pipe_sddl("S-1-5-21-1-2-3-1001", false), only_sid_sddl("S-1-5-21-1-2-3-1001"));
        assert_eq!(
            pipe_sddl("S-1-5-21-1-2-3-1001", true),
            "D:P(A;;GA;;;S-1-5-21-1-2-3-1001)S:(ML;;NWNRNX;;;HI)"
        );
    }

    #[test]
    fn this_process_has_an_integrity_level() {
        let rid = current_integrity_rid().unwrap();
        assert!(rid >= INTEGRITY_MEDIUM, "{rid:#x}");
        assert_eq!(is_elevated(), rid >= INTEGRITY_HIGH);
    }
}
