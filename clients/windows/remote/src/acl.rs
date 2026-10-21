//! User-only files. The identity key, `client_id`, the machine list and the
//! address cache live in `%APPDATA%\forgetty\`. On Windows each gets a
//! protected DACL (inheritance off) with one entry: the current user, full
//! access. On Unix the mode is 0600.

use std::io;
use std::path::Path;

/// Write `bytes` to `path` so that only the current user can read the file at
/// any time after it exists: write a temporary file, restrict it, then rename
/// it over `path` (the DACL travels with the file).
pub fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    // Fresh temporary every time: a stale one may carry another DACL.
    let _ = std::fs::remove_file(&tmp);
    std::fs::write(&tmp, bytes)?;
    if let Err(e) = restrict_to_user(&tmp) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, path)
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
pub use win::{create_private_new, current_user_sid, describe, is_user_only, restrict_to_user};

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
        io::Error::other(e.to_string())
    }

    /// The current user's SID in string form (`S-1-5-21-...`).
    pub fn current_user_sid() -> io::Result<String> {
        unsafe {
            let mut token = HANDLE::default();
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).map_err(err)?;
            let result = (|| {
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
            })();
            let _ = CloseHandle(token);
            result
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
                    match w {
                        Ok(()) if written as usize == bytes.len() => Ok(()),
                        Ok(()) => Err(io::Error::other("short write")),
                        Err(e) => Err(err(e)),
                    }
                }
            };
            let _ = LocalFree(Some(HLOCAL(sd.0)));
            if result.is_err() {
                let _ = std::fs::remove_file(path);
            }
            result
        }
    }

    /// Replace the DACL of `path` with one entry: the current user, full
    /// access, inheritance off.
    pub fn restrict_to_user(path: &Path) -> io::Result<()> {
        let sid = current_user_sid()?;
        let sddl: Vec<u16> = format!("D:P(A;;FA;;;{sid})").encode_utf16().chain(std::iter::once(0)).collect();
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
