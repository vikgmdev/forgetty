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

/// Create `dest` new (it must not exist), open for writing, with the access rules of `model` from
/// the first moment: on Windows the descriptor of `model` goes to CreateFileW, so no handle can
/// open the file with the rules the folder would give. Without a `model` the file is made plain.
#[cfg(unix)]
pub fn create_new_like(model: &Path, dest: &Path) -> io::Result<std::fs::File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mode = std::fs::metadata(model).map(|m| m.permissions().mode() & 0o777).unwrap_or(0o600);
    let f = std::fs::OpenOptions::new().write(true).create_new(true).mode(mode).open(dest)?;
    // The umask only takes bits away: make the mode exactly the model's.
    f.set_permissions(std::fs::Permissions::from_mode(mode))?;
    Ok(f)
}

#[cfg(windows)]
pub fn create_new_like(model: &Path, dest: &Path) -> io::Result<std::fs::File> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;

    use windows::core::{BOOL, PCWSTR};
    use windows::Win32::Foundation::{LocalFree, ERROR_SUCCESS, HLOCAL};
    use windows::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows::Win32::Security::{DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    use windows::Win32::Storage::FileSystem::{CreateFileW, CREATE_NEW, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_WRITE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE};

    fn wide(p: &Path) -> Vec<u16> {
        p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
    }
    if !model.exists() {
        return std::fs::OpenOptions::new().write(true).create_new(true).open(dest);
    }
    let (src, dst) = (wide(model), wide(dest));
    // SAFETY: the names are NUL-terminated and live for the calls. The self-relative descriptor
    // that GetNamedSecurityInfoW allocates (DACL and its protected flag) is valid for the
    // CreateFileW call and freed once after it.
    let file = unsafe {
        let mut sd = PSECURITY_DESCRIPTOR::default();
        let rc = GetNamedSecurityInfoW(PCWSTR(src.as_ptr()), SE_FILE_OBJECT, DACL_SECURITY_INFORMATION, None, None, None, None, &mut sd);
        if rc != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(rc.0 as i32));
        }
        let sa = SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: sd.0, bInheritHandle: BOOL(0) };
        let h = CreateFileW(
            PCWSTR(dst.as_ptr()),
            FILE_GENERIC_WRITE.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            Some(&sa),
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            None,
        );
        let r = match h {
            Err(_) => Err(io::Error::last_os_error()),
            Ok(h) => Ok(std::fs::File::from_raw_handle(h.0)),
        };
        let _ = LocalFree(Some(HLOCAL(sd.0)));
        r
    }?;
    // Inherited entries of the descriptor are merged with the folder's at creation: set the
    // exact rules of the model now (nothing has been written yet).
    if let Err(e) = copy_security(model, dest) {
        drop(file);
        let _ = std::fs::remove_file(dest);
        return Err(e);
    }
    Ok(file)
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

    #[cfg(unix)]
    #[test]
    fn a_new_file_is_made_with_the_mode_of_the_model_and_never_overwrites() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let (a, b) = (d.path().join("a"), d.path().join("b"));
        std::fs::write(&a, "x").unwrap();
        std::fs::set_permissions(&a, std::fs::Permissions::from_mode(0o640)).unwrap();
        let mut f = super::create_new_like(&a, &b).unwrap();
        f.write_all(b"y").unwrap();
        assert_eq!(std::fs::metadata(&b).unwrap().permissions().mode() & 0o777, 0o640);
        assert!(super::create_new_like(&a, &b).is_err());
        // No model: a plain file.
        assert!(super::create_new_like(&d.path().join("none"), &d.path().join("c")).is_ok());
    }
}
