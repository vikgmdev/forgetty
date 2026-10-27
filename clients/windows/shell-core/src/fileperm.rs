//! Give a new file the access rules of an existing one. The Settings window writes
//! `config.toml` by writing a temporary file and renaming it over the old one; the temporary file
//! must not end up with other access rules than the file it replaces (a user who made the file
//! private keeps it private, one who shared it with a group keeps that).

use std::io;
use std::path::Path;

/// Copy the security of `from` to `to` (the file `to` exists and is still empty). On Windows this
/// is the DACL, protected or inherited as `from` has it. On Unix it is the permission bits.
#[cfg(unix)]
pub fn copy_security(from: &Path, to: &Path) -> io::Result<()> {
    let perms = std::fs::metadata(from)?.permissions();
    std::fs::set_permissions(to, perms)
}

#[cfg(windows)]
pub fn copy_security(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{LocalFree, ERROR_SUCCESS, HLOCAL};
    use windows::Win32::Security::Authorization::{GetNamedSecurityInfoW, SetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows::Win32::Security::{
        GetSecurityDescriptorControl, ACL, DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
        SE_DACL_PROTECTED, UNPROTECTED_DACL_SECURITY_INFORMATION,
    };

    fn wide(p: &Path) -> Vec<u16> {
        p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
    }
    let (src, dst) = (wide(from), wide(to));
    // SAFETY: both names are NUL-terminated and live for the calls. The descriptor that
    // GetNamedSecurityInfoW allocates is freed once, after the last use of the DACL pointer
    // (which points into it).
    unsafe {
        let mut sd = PSECURITY_DESCRIPTOR::default();
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let rc = GetNamedSecurityInfoW(
            PCWSTR(src.as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(&mut dacl as *mut *mut ACL),
            None,
            &mut sd,
        );
        if rc != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(rc.0 as i32));
        }
        let result = (|| {
            let mut control = 0u16;
            let mut revision = 0u32;
            GetSecurityDescriptorControl(sd, &mut control, &mut revision).map_err(|e| io::Error::other(e.to_string()))?;
            let inherit = if control & SE_DACL_PROTECTED.0 != 0 { PROTECTED_DACL_SECURITY_INFORMATION } else { UNPROTECTED_DACL_SECURITY_INFORMATION };
            let rc = SetNamedSecurityInfoW(PCWSTR(dst.as_ptr()), SE_FILE_OBJECT, DACL_SECURITY_INFORMATION | inherit, None, None, Some(dacl as *const ACL), None);
            if rc != ERROR_SUCCESS {
                return Err(io::Error::from_raw_os_error(rc.0 as i32));
            }
            Ok(())
        })();
        let _ = LocalFree(Some(HLOCAL(sd.0)));
        result
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    #[test]
    fn a_copy_gets_the_mode_of_the_original() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let (a, b) = (d.path().join("a"), d.path().join("b"));
        std::fs::write(&a, "x").unwrap();
        std::fs::write(&b, "").unwrap();
        std::fs::set_permissions(&a, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::set_permissions(&b, std::fs::Permissions::from_mode(0o644)).unwrap();
        super::copy_security(&a, &b).unwrap();
        assert_eq!(std::fs::metadata(&b).unwrap().permissions().mode() & 0o777, 0o600);
    }
}
