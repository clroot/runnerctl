// Windows counterparts of the owner-only Unix modes runnerctl relies on. A private object
// has a protected DACL that allows only the current user, SYSTEM and Administrators, like
// mode 0700/0600 under a root-administered Unix host.
use anyhow::{Context, Result, bail, ensure};
use std::{
    ffi::c_void,
    os::windows::{ffi::OsStrExt, io::AsRawHandle},
    path::Path,
    ptr,
    sync::OnceLock,
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE, HLOCAL, INVALID_HANDLE_VALUE, LocalFree},
    Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            ConvertStringSidToSidW, GetNamedSecurityInfoW, GetSecurityInfo, SDDL_REVISION_1,
            SE_FILE_OBJECT, SetNamedSecurityInfoW,
        },
        DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetAclInformation, GetSecurityDescriptorDacl,
        GetTokenInformation, INHERIT_ONLY_ACE, PROTECTED_DACL_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
    },
    System::{
        Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
            TH32CS_SNAPPROCESS,
        },
        Threading::{
            GetCurrentProcess, GetCurrentProcessId, INFINITE, OpenProcess, OpenProcessToken,
            PROCESS_SYNCHRONIZE, WaitForSingleObject,
        },
    },
};

// ACCESS_ALLOWED_ACE_TYPE and its object/callback variants grant access.
const ALLOW_TYPES: [u8; 4] = [0, 5, 9, 11];

fn wide(value: &std::ffi::OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

/// String SID of the user running this process.
pub fn user_sid() -> Result<&'static str> {
    static SID: OnceLock<String> = OnceLock::new();
    if let Some(sid) = SID.get() {
        return Ok(sid);
    }
    let sid = unsafe {
        let mut token: HANDLE = ptr::null_mut();
        ensure!(
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) != 0,
            "cannot open the process token"
        );
        let mut size = 0;
        GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut size);
        // u64 storage keeps TOKEN_USER aligned.
        let mut buffer = vec![0u64; (size as usize).div_ceil(8)];
        let ok = GetTokenInformation(
            token,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            size,
            &mut size,
        );
        CloseHandle(token);
        ensure!(ok != 0, "cannot read the process user");
        let user = &*(buffer.as_ptr() as *const TOKEN_USER);
        let mut text = ptr::null_mut();
        ensure!(
            ConvertSidToStringSidW(user.User.Sid, &mut text) != 0,
            "cannot format the process user"
        );
        let length = (0..).take_while(|&i| *text.add(i) != 0).count();
        let sid = String::from_utf16_lossy(std::slice::from_raw_parts(text, length));
        LocalFree(text as HLOCAL);
        sid
    };
    Ok(SID.get_or_init(|| sid))
}

/// A security descriptor parsed from SDDL, freed on drop.
pub struct Descriptor(PSECURITY_DESCRIPTOR);
// The descriptor is immutable after parsing.
unsafe impl Send for Descriptor {}
unsafe impl Sync for Descriptor {}
impl Descriptor {
    pub fn new(sddl: &str) -> Result<Self> {
        let sddl = wide(sddl.as_ref());
        let mut descriptor = ptr::null_mut();
        ensure!(
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    ptr::null_mut(),
                )
            } != 0,
            "invalid security descriptor"
        );
        Ok(Self(descriptor))
    }
    /// Owner-only access, as for the manager's named pipe.
    pub fn private() -> Result<Self> {
        Self::new(&format!("D:P(A;;GA;;;{})(A;;GA;;;SY)", user_sid()?))
    }
    pub fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0,
            bInheritHandle: 0,
        }
    }
    fn dacl(&self) -> Result<*mut ACL> {
        let (mut present, mut defaulted, mut dacl) = (0, 0, ptr::null_mut());
        ensure!(
            unsafe { GetSecurityDescriptorDacl(self.0, &mut present, &mut dacl, &mut defaulted) }
                != 0
                && present != 0,
            "security descriptor has no DACL"
        );
        Ok(dacl)
    }
}
impl Drop for Descriptor {
    fn drop(&mut self) {
        unsafe { LocalFree(self.0 as HLOCAL) };
    }
}

/// Replaces inherited and explicit access with a private DACL; directories pass it on.
pub fn restrict(path: &Path, directory: bool) -> Result<()> {
    let inherit = if directory { "OICI" } else { "" };
    let user = user_sid()?;
    let descriptor = Descriptor::new(&format!(
        "D:P(A;{inherit};FA;;;{user})(A;{inherit};FA;;;SY)(A;{inherit};FA;;;BA)"
    ))?;
    let path = wide(path.as_os_str());
    let error = unsafe {
        SetNamedSecurityInfoW(
            path.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            descriptor.dacl()?,
            ptr::null(),
        )
    };
    ensure!(error == 0, "cannot restrict access (Windows error {error})");
    Ok(())
}

fn trusted() -> Result<&'static [Vec<u8>]> {
    static SIDS: OnceLock<Vec<Vec<u8>>> = OnceLock::new();
    if let Some(sids) = SIDS.get() {
        return Ok(sids);
    }
    let mut sids = vec![];
    for text in [user_sid()?, "S-1-5-18", "S-1-5-32-544"] {
        let text = wide(text.as_ref());
        let mut sid: PSID = ptr::null_mut();
        ensure!(
            unsafe { ConvertStringSidToSidW(text.as_ptr(), &mut sid) } != 0,
            "invalid SID"
        );
        let length = unsafe { windows_sys::Win32::Security::GetLengthSid(sid) } as usize;
        sids.push(unsafe { std::slice::from_raw_parts(sid as *const u8, length) }.to_vec());
        unsafe { LocalFree(sid as HLOCAL) };
    }
    Ok(SIDS.get_or_init(|| sids))
}

// True when no access-allowed entry names anyone but the trusted SIDs.
fn private_dacl(dacl: *mut ACL) -> Result<bool> {
    if dacl.is_null() {
        // A null DACL grants everyone full access.
        return Ok(false);
    }
    let trusted = trusted()?;
    let mut size = ACL_SIZE_INFORMATION::default();
    ensure!(
        unsafe {
            GetAclInformation(
                dacl,
                (&mut size as *mut ACL_SIZE_INFORMATION).cast(),
                size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            )
        } != 0,
        "cannot read access control list"
    );
    for index in 0..size.AceCount {
        let mut ace: *mut c_void = ptr::null_mut();
        ensure!(
            unsafe { GetAce(dacl, index, &mut ace) } != 0,
            "cannot read access control entry"
        );
        let header = unsafe { &*(ace as *const ACE_HEADER) };
        if !ALLOW_TYPES.contains(&header.AceType) || header.AceFlags & INHERIT_ONLY_ACE as u8 != 0 {
            continue;
        }
        if header.AceType != 0 {
            // Object and callback entries are not used on files; treat them as foreign.
            return Ok(false);
        }
        let sid = unsafe { &(*(ace as *const ACCESS_ALLOWED_ACE)).SidStart } as *const u32;
        let known = trusted
            .iter()
            .any(|known| unsafe { EqualSid(sid as PSID, known.as_ptr() as PSID) != 0 });
        if !known {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Whether only the current user, SYSTEM and Administrators can access an open file.
pub fn private_file(file: &std::fs::File) -> Result<bool> {
    let (mut dacl, mut descriptor) = (ptr::null_mut(), ptr::null_mut());
    let error = unsafe {
        GetSecurityInfo(
            file.as_raw_handle() as HANDLE,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut dacl,
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    ensure!(
        error == 0,
        "cannot read file access (Windows error {error})"
    );
    let result = private_dacl(dacl);
    unsafe { LocalFree(descriptor as HLOCAL) };
    result
}

/// Whether only the current user, SYSTEM and Administrators can access a path.
pub fn private_path(path: &Path) -> Result<bool> {
    let wide_path = wide(path.as_os_str());
    let (mut dacl, mut descriptor) = (ptr::null_mut(), ptr::null_mut());
    let error = unsafe {
        GetNamedSecurityInfoW(
            wide_path.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut dacl,
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    ensure!(
        error == 0,
        "cannot read access to {} (Windows error {error})",
        path.display()
    );
    let result = private_dacl(dacl);
    unsafe { LocalFree(descriptor as HLOCAL) };
    result
}

/// Waits until the process that started this one exits.
pub fn wait_for_parent() -> Result<()> {
    let parent = unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        ensure!(snapshot != INVALID_HANDLE_VALUE, "cannot list processes");
        let mut entry = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let me = GetCurrentProcessId();
        let mut parent = None;
        let mut more = Process32FirstW(snapshot, &mut entry) != 0;
        while more {
            if entry.th32ProcessID == me {
                parent = Some(entry.th32ParentProcessID);
                break;
            }
            more = Process32NextW(snapshot, &mut entry) != 0;
        }
        CloseHandle(snapshot);
        parent.context("cannot find the parent process")?
    };
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, parent) };
    if handle.is_null() {
        bail!("the parent process has already exited");
    }
    unsafe {
        WaitForSingleObject(handle, INFINITE);
        CloseHandle(handle);
    }
    Ok(())
}

/// Sends this process's standard output and error to a file.
pub fn redirect_output(file: &std::fs::File) -> Result<()> {
    use windows_sys::Win32::System::Console::{STD_ERROR_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle};
    // Rust looks up the standard handles on each write, so this takes effect immediately.
    for target in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        ensure!(
            unsafe { SetStdHandle(target, file.as_raw_handle() as HANDLE) } != 0,
            "cannot redirect output"
        );
    }
    Ok(())
}

/// Grants everyone read access, as mode 0644 does in Unix tests.
#[cfg(test)]
pub fn share(path: &Path) {
    let descriptor = Descriptor::new("D:P(A;;FA;;;OW)(A;;FR;;;WD)").unwrap();
    let path = wide(path.as_os_str());
    let error = unsafe {
        SetNamedSecurityInfoW(
            path.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            descriptor.dacl().unwrap(),
            ptr::null(),
        )
    };
    assert_eq!(error, 0);
}
