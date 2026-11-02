//! What differs between systems: owners, permission bits, attributes,
//! volumes, and changing them.

use std::{
    fs::Metadata,
    io,
    path::{Path, PathBuf},
};

use super::info::Item;

/// The filesystem an item is on.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Volume {
    pub mount: PathBuf,
    pub fs_type: String,
    /// The device or source it was mounted from, or the volume label.
    pub device: String,
}

pub use imp::*;

#[cfg(target_os = "linux")]
mod imp {
    use std::{
        ffi::CStr,
        fs,
        os::unix::fs::{MetadataExt, PermissionsExt},
        sync::OnceLock,
        time::{Duration, SystemTime},
    };

    use super::*;

    pub fn fill(item: &mut Item, meta: &Metadata) {
        item.mode = Some(meta.mode() & 0o7777);
        item.uid = Some(meta.uid());
        item.gid = Some(meta.gid());
        item.owner = user_name(meta.uid());
        item.group = group_name(meta.gid());
        item.inode = Some(meta.ino());
        item.links = Some(meta.nlink());
        item.device = Some(meta.dev());
        item.allocated = Some(meta.blocks() * 512);
        item.changed = u64::try_from(meta.ctime())
            .ok()
            .map(|secs| SystemTime::UNIX_EPOCH + Duration::new(secs, meta.ctime_nsec() as u32));
    }

    fn parse_ids(path: &str) -> Vec<(u32, String)> {
        let text = fs::read_to_string(path).unwrap_or_default();
        let mut ids: Vec<(u32, String)> = text
            .lines()
            .filter(|l| !l.starts_with('#'))
            .filter_map(|line| {
                let mut fields = line.split(':');
                let name = fields.next()?.to_owned();
                let id = fields.nth(1)?.parse().ok()?;
                Some((id, name))
            })
            .collect();
        ids.sort_by(|a, b| a.1.cmp(&b.1));
        ids
    }

    /// Local users, by name.
    pub fn users() -> &'static [(u32, String)] {
        static USERS: OnceLock<Vec<(u32, String)>> = OnceLock::new();
        USERS.get_or_init(|| parse_ids("/etc/passwd"))
    }

    /// Local groups, by name.
    pub fn groups() -> &'static [(u32, String)] {
        static GROUPS: OnceLock<Vec<(u32, String)>> = OnceLock::new();
        GROUPS.get_or_init(|| parse_ids("/etc/group"))
    }

    fn user_name(uid: u32) -> Option<String> {
        if let Some((_, name)) = users().iter().find(|(id, _)| *id == uid) {
            return Some(name.clone());
        }
        // Users from elsewhere (LDAP, systemd-homed) through NSS.
        let mut buf = vec![0 as libc::c_char; 4096];
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result = std::ptr::null_mut();
        // SAFETY: the buffers are as long as we say and outlive the call.
        let rc =
            unsafe { libc::getpwuid_r(uid, &mut pwd, buf.as_mut_ptr(), buf.len(), &mut result) };
        (rc == 0 && !result.is_null())
            // SAFETY: on success `pw_name` points into `buf`, NUL-terminated.
            .then(|| {
                unsafe { CStr::from_ptr(pwd.pw_name) }
                    .to_string_lossy()
                    .into_owned()
            })
    }

    fn group_name(gid: u32) -> Option<String> {
        if let Some((_, name)) = groups().iter().find(|(id, _)| *id == gid) {
            return Some(name.clone());
        }
        let mut buf = vec![0 as libc::c_char; 4096];
        let mut grp: libc::group = unsafe { std::mem::zeroed() };
        let mut result = std::ptr::null_mut();
        // SAFETY: as for `getpwuid_r`.
        let rc =
            unsafe { libc::getgrgid_r(gid, &mut grp, buf.as_mut_ptr(), buf.len(), &mut result) };
        (rc == 0 && !result.is_null())
            // SAFETY: on success `gr_name` points into `buf`.
            .then(|| {
                unsafe { CStr::from_ptr(grp.gr_name) }
                    .to_string_lossy()
                    .into_owned()
            })
    }

    /// Undoes the octal escapes (`\040` for a space) of `/proc` mount fields.
    fn unescape(field: &str) -> String {
        let bytes = field.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'\\'
                && i + 3 < bytes.len()
                && let Some(code) = std::str::from_utf8(&bytes[i + 1..i + 4])
                    .ok()
                    .and_then(|digits| u8::from_str_radix(digits, 8).ok())
            {
                out.push(code);
                i += 4;
            } else {
                out.push(bytes[i]);
                i += 1;
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    pub fn volume(path: &Path) -> Option<Volume> {
        let path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let text = fs::read_to_string("/proc/self/mountinfo").ok()?;
        text.lines()
            .filter_map(|line| {
                // id parent major:minor root mount options [optional…] - type source super
                let (left, right) = line.split_once(" - ")?;
                let mount = unescape(left.split(' ').nth(4)?);
                let mut right = right.split(' ');
                let fs_type = right.next()?.to_owned();
                let device = unescape(right.next()?);
                Some(Volume {
                    mount: PathBuf::from(mount),
                    fs_type,
                    device,
                })
            })
            .filter(|v| path.starts_with(&v.mount))
            // Later mounts over the same point hide earlier ones.
            .max_by_key(|v| v.mount.components().count())
    }

    /// On a network filesystem or a FUSE mount (sshfs, rclone, gvfs…),
    /// where reading every folder is slow or costs something.
    pub fn is_remote(path: &Path) -> bool {
        volume(path).is_some_and(|v| remote_fs_type(&v.fs_type))
    }

    pub(super) fn remote_fs_type(fs_type: &str) -> bool {
        const NETWORK: [&str; 14] = [
            "nfs",
            "nfs4",
            "cifs",
            "smb3",
            "smbfs",
            "ncpfs",
            "afs",
            "ceph",
            "glusterfs",
            "9p",
            "lustre",
            "gpfs",
            "davfs",
            "sshfs",
        ];
        // `fuseblk` is a local disk through FUSE (ntfs-3g, exfat-fuse).
        NETWORK.contains(&fs_type) || fs_type == "fuse" || fs_type.starts_with("fuse.")
    }

    pub fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
    }

    pub fn set_owner(
        path: &Path,
        uid: Option<u32>,
        gid: Option<u32>,
        _account: Option<&str>,
        follow: bool,
    ) -> io::Result<()> {
        if follow {
            std::os::unix::fs::chown(path, uid, gid)
        } else {
            std::os::unix::fs::lchown(path, uid, gid)
        }
    }

    pub fn set_attributes(_path: &Path, _attributes: u32) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "no attributes here",
        ))
    }

    pub fn acl(_path: &Path) -> Vec<String> {
        Vec::new()
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn unescapes_mount_points() {
            assert_eq!(super::unescape(r"/media/My\040Disk"), "/media/My Disk");
            assert_eq!(super::unescape("/plain"), "/plain");
        }
    }
}

#[cfg(windows)]
mod imp {
    use std::os::windows::fs::MetadataExt;

    use windows::{
        Win32::{
            Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE, HLOCAL, LUID, LocalFree},
            Security::{
                ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
                AdjustTokenPrivileges,
                Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT, SetNamedSecurityInfoW},
                DACL_SECURITY_INFORMATION, GROUP_SECURITY_INFORMATION, GetAce, GetAclInformation,
                LUID_AND_ATTRIBUTES, LookupAccountNameW, LookupAccountSidW, LookupPrivilegeValueW,
                OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SE_PRIVILEGE_ENABLED,
                SID_NAME_USE, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES,
            },
            Storage::FileSystem::{
                FILE_FLAGS_AND_ATTRIBUTES, GetDriveTypeW, GetVolumeInformationW,
                GetVolumePathNameW, SetFileAttributesW,
            },
            System::{
                Threading::{GetCurrentProcess, OpenProcessToken},
                WindowsProgramming::DRIVE_REMOTE,
            },
        },
        core::{HSTRING, PCWSTR, PWSTR},
    };

    use super::*;

    pub fn fill(item: &mut Item, meta: &Metadata) {
        item.attributes = Some(meta.file_attributes());
        let (owner, group) = owner_and_group(&item.path);
        item.owner = owner;
        item.group = group;
    }

    fn wide(path: &Path) -> HSTRING {
        HSTRING::from(path.as_os_str())
    }

    /// "DOMAIN\name" for a SID, or its string form if it has no name.
    fn account_name(sid: PSID) -> Option<String> {
        let mut name = [0u16; 256];
        let mut domain = [0u16; 256];
        let (mut name_len, mut domain_len) = (name.len() as u32, domain.len() as u32);
        let mut kind = SID_NAME_USE::default();
        // SAFETY: the buffers are as long as we say; `sid` is valid.
        unsafe {
            LookupAccountSidW(
                PCWSTR::null(),
                sid,
                Some(PWSTR(name.as_mut_ptr())),
                &mut name_len,
                Some(PWSTR(domain.as_mut_ptr())),
                &mut domain_len,
                &mut kind,
            )
        }
        .ok()?;
        let name = String::from_utf16_lossy(&name[..name_len as usize]);
        let domain = String::from_utf16_lossy(&domain[..domain_len as usize]);
        Some(if domain.is_empty() {
            name
        } else {
            format!("{domain}\\{name}")
        })
    }

    /// The security descriptor of a file, freed on drop.
    struct Descriptor(PSECURITY_DESCRIPTOR);

    impl Drop for Descriptor {
        fn drop(&mut self) {
            // SAFETY: allocated by GetNamedSecurityInfoW with LocalAlloc.
            unsafe { LocalFree(Some(HLOCAL(self.0.0))) };
        }
    }

    fn owner_and_group(path: &Path) -> (Option<String>, Option<String>) {
        let (mut owner, mut group) = (PSID::default(), PSID::default());
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        // SAFETY: out-pointers are valid; the SIDs point into `descriptor`.
        let err = unsafe {
            GetNamedSecurityInfoW(
                &wide(path),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION,
                Some(&mut owner),
                Some(&mut group),
                None,
                None,
                &mut descriptor,
            )
        };
        if err != ERROR_SUCCESS {
            return (None, None);
        }
        let _guard = Descriptor(descriptor);
        (account_name(owner), account_name(group))
    }

    /// The access control list in words, one line per entry.
    pub fn acl(path: &Path) -> Vec<String> {
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        // SAFETY: out-pointers are valid; `dacl` points into `descriptor`.
        let err = unsafe {
            GetNamedSecurityInfoW(
                &wide(path),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(&mut dacl),
                None,
                &mut descriptor,
            )
        };
        if err != ERROR_SUCCESS {
            return Vec::new();
        }
        let _guard = Descriptor(descriptor);
        if dacl.is_null() {
            return vec!["Everyone: full control (no access list)".to_owned()];
        }
        let mut info = ACL_SIZE_INFORMATION::default();
        // SAFETY: `dacl` is valid and `info` is the struct for this class.
        if unsafe {
            GetAclInformation(
                dacl,
                (&raw mut info).cast(),
                size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            )
        }
        .is_err()
        {
            return Vec::new();
        }
        let mut lines = Vec::new();
        for index in 0..info.AceCount {
            let mut ace = std::ptr::null_mut();
            // SAFETY: `index` is in range.
            if unsafe { GetAce(dacl, index, &mut ace) }.is_err() {
                continue;
            }
            // SAFETY: every ACE starts with a header.
            let header = unsafe { &*(ace as *const ACE_HEADER) };
            // ACCESS_ALLOWED and ACCESS_DENIED share a layout.
            let allowed = match header.AceType {
                0 => true,
                1 => false,
                _ => continue,
            };
            // SAFETY: types 0 and 1 are ACCESS_ALLOWED_ACE-shaped.
            let ace = unsafe { &*(ace as *const ACCESS_ALLOWED_ACE) };
            let sid = PSID((&raw const ace.SidStart).cast_mut().cast());
            let who = account_name(sid).unwrap_or_else(|| "Unknown account".to_owned());
            let inherited = header.AceFlags & 0x10 != 0;
            lines.push(format!(
                "{who}: {} {}{}",
                if allowed { "allow" } else { "deny" },
                access_words(ace.Mask),
                if inherited { " (inherited)" } else { "" }
            ));
        }
        lines
    }

    fn access_words(mask: u32) -> String {
        const FULL: u32 = 0x1f01ff;
        const MODIFY: u32 = 0x1301bf;
        const READ_EXECUTE: u32 = 0x1200a9;
        const READ: u32 = 0x120089;
        const WRITE: u32 = 0x100116;
        const GENERIC_ALL: u32 = 0x1000_0000;
        if mask & FULL == FULL || mask & GENERIC_ALL != 0 {
            "full control".to_owned()
        } else if mask & MODIFY == MODIFY {
            "modify".to_owned()
        } else if mask & READ_EXECUTE == READ_EXECUTE {
            if mask & WRITE == WRITE {
                "read, execute and write".to_owned()
            } else {
                "read and execute".to_owned()
            }
        } else if mask & READ == READ {
            if mask & WRITE == WRITE {
                "read and write".to_owned()
            } else {
                "read".to_owned()
            }
        } else if mask & WRITE == WRITE {
            "write".to_owned()
        } else {
            format!("special (0x{mask:x})")
        }
    }

    pub fn volume(path: &Path) -> Option<Volume> {
        let mut root = [0u16; 261];
        // SAFETY: `root` is the buffer it fills.
        unsafe { GetVolumePathNameW(&wide(path), &mut root) }.ok()?;
        let len = root.iter().position(|&c| c == 0).unwrap_or(root.len());
        let mount = String::from_utf16_lossy(&root[..len]);
        let mut label = [0u16; 261];
        let mut fs_name = [0u16; 261];
        // SAFETY: as above; `root` is NUL-terminated.
        unsafe {
            GetVolumeInformationW(
                PCWSTR(root.as_ptr()),
                Some(&mut label),
                None,
                None,
                None,
                Some(&mut fs_name),
            )
        }
        .ok()?;
        let text = |buf: &[u16]| {
            let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            String::from_utf16_lossy(&buf[..len])
        };
        Some(Volume {
            mount: PathBuf::from(mount),
            fs_type: text(&fs_name),
            device: text(&label),
        })
    }

    /// On a network share, mapped drive letters and UNC paths alike.
    pub fn is_remote(path: &Path) -> bool {
        let mut root = [0u16; 261];
        // SAFETY: `root` is the buffer it fills.
        if unsafe { GetVolumePathNameW(&wide(path), &mut root) }.is_err() {
            return false;
        }
        // SAFETY: `root` is NUL-terminated.
        unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) == DRIVE_REMOTE }
    }

    pub fn set_mode(_path: &Path, _mode: u32) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "no permission bits here",
        ))
    }

    pub fn set_attributes(path: &Path, attributes: u32) -> io::Result<()> {
        // SAFETY: a valid path string.
        unsafe { SetFileAttributesW(&wide(path), FILE_FLAGS_AND_ATTRIBUTES(attributes)) }
            .map_err(win_error)
    }

    fn win_error(error: windows::core::Error) -> io::Error {
        // HRESULT_FROM_WIN32 keeps the Win32 code in the low word.
        let code = error.code().0 as u32;
        if code & 0xffff_0000 == 0x8007_0000 {
            io::Error::from_raw_os_error((code & 0xffff) as i32)
        } else {
            io::Error::other(error)
        }
    }

    /// Lets this process set owners other than itself, as far as its token
    /// allows (an elevated administrator's does; a normal user's doesn't).
    fn enable_owner_privileges() {
        // SAFETY: plain token calls on our own process with valid out-pointers.
        unsafe {
            let mut token = HANDLE::default();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES, &mut token).is_err() {
                return;
            }
            for name in ["SeRestorePrivilege", "SeTakeOwnershipPrivilege"] {
                let mut luid = LUID::default();
                if LookupPrivilegeValueW(PCWSTR::null(), &HSTRING::from(name), &mut luid).is_ok() {
                    let privileges = TOKEN_PRIVILEGES {
                        PrivilegeCount: 1,
                        Privileges: [LUID_AND_ATTRIBUTES {
                            Luid: luid,
                            Attributes: SE_PRIVILEGE_ENABLED,
                        }],
                    };
                    let _ = AdjustTokenPrivileges(token, false, Some(&privileges), 0, None, None);
                }
            }
            let _ = CloseHandle(token);
        }
    }

    pub fn set_owner(
        path: &Path,
        _uid: Option<u32>,
        _gid: Option<u32>,
        account: Option<&str>,
        _follow: bool,
    ) -> io::Result<()> {
        let Some(account) = account else {
            return Ok(());
        };
        let mut sid = vec![0u8; 256];
        let mut domain = [0u16; 256];
        let (mut sid_len, mut domain_len) = (sid.len() as u32, domain.len() as u32);
        let mut kind = SID_NAME_USE::default();
        // SAFETY: the buffers are as long as we say.
        unsafe {
            LookupAccountNameW(
                PCWSTR::null(),
                &HSTRING::from(account),
                Some(PSID(sid.as_mut_ptr().cast())),
                &mut sid_len,
                Some(PWSTR(domain.as_mut_ptr())),
                &mut domain_len,
                &mut kind,
            )
        }
        .map_err(|_| io::Error::new(io::ErrorKind::NotFound, format!("No account “{account}”")))?;
        enable_owner_privileges();
        // SAFETY: `sid` holds the SID LookupAccountNameW wrote.
        let err = unsafe {
            SetNamedSecurityInfoW(
                &wide(path),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION,
                Some(PSID(sid.as_mut_ptr().cast())),
                None,
                None,
                None,
            )
        };
        if err == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(err.0 as i32))
        }
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
mod imp {
    use super::*;

    pub fn fill(_item: &mut Item, _meta: &Metadata) {}

    pub fn volume(_path: &Path) -> Option<Volume> {
        None
    }

    pub fn is_remote(_path: &Path) -> bool {
        false
    }

    pub fn set_mode(_path: &Path, _mode: u32) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "unsupported"))
    }

    pub fn set_owner(
        _path: &Path,
        _uid: Option<u32>,
        _gid: Option<u32>,
        _account: Option<&str>,
        _follow: bool,
    ) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "unsupported"))
    }

    pub fn set_attributes(_path: &Path, _attributes: u32) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "unsupported"))
    }

    pub fn acl(_path: &Path) -> Vec<String> {
        Vec::new()
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    #[test]
    fn network_and_fuse_types_are_remote() {
        for remote in ["nfs4", "cifs", "fuse.sshfs", "fuse.rclone", "fuse"] {
            assert!(super::imp::remote_fs_type(remote), "{remote}");
        }
        for local in ["ext4", "btrfs", "fuseblk", "tmpfs", "vfat"] {
            assert!(!super::imp::remote_fs_type(local), "{local}");
        }
    }
}
