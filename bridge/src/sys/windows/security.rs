//! Security descriptors that name the current user, shared by the named pipe,
//! the runtime directory and the GUI job object.
//!
//! Every private object the bridge creates carries a protected DACL built from
//! SDDL, so that no inherited or default entry can widen it. Reading one back
//! is how the runtime directory is checked, the way `stat` checks 0700 on Unix.

use std::ffi::{c_void, OsStr};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;

use windows_sys::Win32::Foundation::{LocalFree, ERROR_SUCCESS, HANDLE};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    GetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    GetAce, GetTokenInformation, TokenUser, ACCESS_ALLOWED_ACE, ACL, DACL_SECURITY_INFORMATION,
    OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, TOKEN_QUERY,
    TOKEN_USER,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};

/// `ACCESS_ALLOWED_ACE_TYPE` and `ACCESS_DENIED_ACE_TYPE`, which live in a
/// `windows-sys` feature this crate does not otherwise need.
pub(super) const ACCESS_ALLOWED: u8 = 0;
const ACCESS_DENIED: u8 = 1;

/// `NT AUTHORITY\SYSTEM` and `BUILTIN\Administrators`, which already reach
/// every file of the user and so cannot make a directory less private.
pub(super) const LOCAL_SYSTEM_SID: &str = "S-1-5-18";
pub(super) const ADMINISTRATORS_SID: &str = "S-1-5-32-544";

/// A `LocalAlloc` block, which the Windows security calls return and the
/// caller frees.
pub(super) struct LocalBlock(pub(super) *mut c_void);

impl Drop for LocalBlock {
    fn drop(&mut self) {
        unsafe { LocalFree(self.0) };
    }
}

// A local heap block has no thread affinity.
unsafe impl Send for LocalBlock {}
unsafe impl Sync for LocalBlock {}

pub(super) struct SecurityDescriptor(LocalBlock);

impl SecurityDescriptor {
    pub(super) fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0 .0,
            bInheritHandle: 0,
        }
    }
}

/// A descriptor built from SDDL. `D:P` protects the DACL, so nothing is
/// inherited into the object.
pub(super) fn descriptor(sddl: &str) -> io::Result<SecurityDescriptor> {
    let sddl = wide(OsStr::new(sddl));
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    let made = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    };
    if made == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(SecurityDescriptor(LocalBlock(descriptor)))
}

/// A descriptor granting all access to the current user and no one else.
pub(super) fn current_user_descriptor() -> io::Result<SecurityDescriptor> {
    descriptor(&format!("D:P(A;;GA;;;{})", current_user_sid()?))
}

pub(super) fn current_user_sid() -> io::Result<String> {
    token_user_sid(&process_token(unsafe { GetCurrentProcess() })?)
}

pub(super) fn process_user_sid(pid: u32) -> io::Result<String> {
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return Err(io::Error::last_os_error());
    }
    let process = unsafe { OwnedHandle::from_raw_handle(process) };
    token_user_sid(&process_token(process.as_raw_handle())?)
}

fn token_user_sid(token: &OwnedHandle) -> io::Result<String> {
    let mut needed = 0u32;
    unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            std::ptr::null_mut(),
            0,
            &mut needed,
        )
    };
    if needed == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buffer = vec![0u64; (needed as usize).div_ceil(8)];
    let read = unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buffer.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    };
    if read == 0 {
        return Err(io::Error::last_os_error());
    }
    let user = unsafe { buffer.as_ptr().cast::<TOKEN_USER>().read() };
    let sid = sid_to_string(user.User.Sid);
    drop(buffer);
    sid
}

fn process_token(process: HANDLE) -> io::Result<OwnedHandle> {
    let mut token: HANDLE = std::ptr::null_mut();
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(token) })
}

pub(super) fn sid_to_string(sid: PSID) -> io::Result<String> {
    let mut text = std::ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let owned = LocalBlock(text.cast());
    let mut len = 0;
    while unsafe { *text.add(len) } != 0 {
        len += 1;
    }
    let sid = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len) });
    drop(owned);
    Ok(sid)
}

/// The owner of a file system object and the subject of every access-allowed
/// entry in its DACL. A missing DACL, which grants everyone, and an entry kind
/// this cannot read are both errors rather than an empty answer.
pub(super) fn file_access(path: &Path) -> io::Result<(String, Vec<String>)> {
    let name = wide(path.as_os_str());
    let mut owner: PSID = std::ptr::null_mut();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    let status = unsafe {
        GetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | OWNER_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    let descriptor = LocalBlock(descriptor);
    let access = read_access(owner, dacl);
    drop(descriptor);
    access
}

fn read_access(owner: PSID, dacl: *mut ACL) -> io::Result<(String, Vec<String>)> {
    if dacl.is_null() {
        return Err(io::Error::other(
            "the object has no discretionary access control list, so everyone may reach it",
        ));
    }
    let owner = sid_to_string(owner)?;
    let mut allowed = Vec::new();
    for index in 0..unsafe { (*dacl).AceCount } {
        let mut entry = std::ptr::null_mut();
        if unsafe { GetAce(dacl, u32::from(index), &mut entry) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let entry = entry.cast::<ACCESS_ALLOWED_ACE>();
        match unsafe { (*entry).Header.AceType } {
            ACCESS_ALLOWED => {
                let sid = unsafe { std::ptr::addr_of!((*entry).SidStart) }
                    .cast_mut()
                    .cast();
                allowed.push(sid_to_string(sid)?);
            }
            ACCESS_DENIED => {}
            kind => {
                return Err(io::Error::other(format!(
                    "access control entry of type {kind} cannot be checked"
                )))
            }
        }
    }
    Ok((owner, allowed))
}

pub(super) fn wide(text: &OsStr) -> Vec<u16> {
    text.encode_wide().chain(std::iter::once(0)).collect()
}
