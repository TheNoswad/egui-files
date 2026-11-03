//! SFTP version 3's packets, as OpenSSH speaks it
//! (draft-ietf-secsh-filexfer-02, with OpenSSH's extensions).

use std::{
    io,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub const INIT: u8 = 1;
pub const VERSION: u8 = 2;
pub const OPEN: u8 = 3;
pub const CLOSE: u8 = 4;
pub const READ: u8 = 5;
pub const WRITE: u8 = 6;
pub const LSTAT: u8 = 7;
pub const FSTAT: u8 = 8;
pub const SETSTAT: u8 = 9;
pub const FSETSTAT: u8 = 10;
pub const OPENDIR: u8 = 11;
pub const READDIR: u8 = 12;
pub const REMOVE: u8 = 13;
pub const MKDIR: u8 = 14;
pub const RMDIR: u8 = 15;
pub const REALPATH: u8 = 16;
pub const STAT: u8 = 17;
pub const RENAME: u8 = 18;
pub const READLINK: u8 = 19;
pub const SYMLINK: u8 = 20;
pub const EXTENDED: u8 = 200;

pub const STATUS: u8 = 101;
pub const HANDLE: u8 = 102;
pub const DATA: u8 = 103;
pub const NAME: u8 = 104;
pub const ATTRS: u8 = 105;
pub const EXTENDED_REPLY: u8 = 201;

pub const OPEN_READ: u32 = 0x01;
pub const OPEN_WRITE: u32 = 0x02;
pub const OPEN_APPEND: u32 = 0x04;
pub const OPEN_CREATE: u32 = 0x08;
pub const OPEN_TRUNCATE: u32 = 0x10;
pub const OPEN_EXCLUSIVE: u32 = 0x20;

const ATTR_SIZE: u32 = 0x01;
const ATTR_UIDGID: u32 = 0x02;
const ATTR_PERMISSIONS: u32 = 0x04;
const ATTR_ACMODTIME: u32 = 0x08;
const ATTR_EXTENDED: u32 = 0x8000_0000;

pub const STATUS_OK: u32 = 0;
pub const STATUS_EOF: u32 = 1;
const STATUS_NO_SUCH_FILE: u32 = 2;
const STATUS_PERMISSION_DENIED: u32 = 3;
pub const STATUS_FAILURE: u32 = 4;
const STATUS_OP_UNSUPPORTED: u32 = 8;

/// The file type bits of a mode, as in `stat`.
pub const S_IFMT: u32 = 0o170_000;
pub const S_IFDIR: u32 = 0o040_000;
pub const S_IFREG: u32 = 0o100_000;
pub const S_IFLNK: u32 = 0o120_000;

/// A file's attributes. Any of them may be missing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Attrs {
    pub size: Option<u64>,
    pub uid_gid: Option<(u32, u32)>,
    pub mode: Option<u32>,
    /// Seconds since 1970 when it was last read and changed.
    pub atime_mtime: Option<(u32, u32)>,
}

impl Attrs {
    pub fn kind(&self) -> u32 {
        self.mode.unwrap_or(0) & S_IFMT
    }

    pub fn is_dir(&self) -> bool {
        self.kind() == S_IFDIR
    }

    pub fn is_symlink(&self) -> bool {
        self.kind() == S_IFLNK
    }

    /// Without a type, as some servers send, a file is the safest guess.
    pub fn is_file(&self) -> bool {
        self.kind() == S_IFREG || self.mode.is_none()
    }

    pub fn modified(&self) -> Option<SystemTime> {
        self.atime_mtime
            .map(|(_, m)| UNIX_EPOCH + Duration::from_secs(m.into()))
    }

    pub fn accessed(&self) -> Option<SystemTime> {
        self.atime_mtime
            .map(|(a, _)| UNIX_EPOCH + Duration::from_secs(a.into()))
    }
}

/// Seconds since 1970, as SFTP 3 keeps times (in 32 bits, until 2106).
pub fn seconds(time: SystemTime) -> u32 {
    let secs = time.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    u32::try_from(secs).unwrap_or(u32::MAX)
}

/// A request's body, built up field by field.
#[derive(Default)]
pub struct Writer(pub Vec<u8>);

impl Writer {
    pub fn u8(mut self, value: u8) -> Self {
        self.0.push(value);
        self
    }

    pub fn u32(mut self, value: u32) -> Self {
        self.0.extend_from_slice(&value.to_be_bytes());
        self
    }

    pub fn u64(mut self, value: u64) -> Self {
        self.0.extend_from_slice(&value.to_be_bytes());
        self
    }

    pub fn bytes(mut self, value: &[u8]) -> Self {
        let len = u32::try_from(value.len()).expect("SFTP strings fit in 32 bits");
        self.0.extend_from_slice(&len.to_be_bytes());
        self.0.extend_from_slice(value);
        self
    }

    pub fn str(self, value: &str) -> Self {
        self.bytes(value.as_bytes())
    }

    pub fn attrs(self, attrs: &Attrs) -> Self {
        let mut flags = 0;
        if attrs.size.is_some() {
            flags |= ATTR_SIZE;
        }
        if attrs.uid_gid.is_some() {
            flags |= ATTR_UIDGID;
        }
        if attrs.mode.is_some() {
            flags |= ATTR_PERMISSIONS;
        }
        if attrs.atime_mtime.is_some() {
            flags |= ATTR_ACMODTIME;
        }
        let mut w = self.u32(flags);
        if let Some(size) = attrs.size {
            w = w.u64(size);
        }
        if let Some((uid, gid)) = attrs.uid_gid {
            w = w.u32(uid).u32(gid);
        }
        if let Some(mode) = attrs.mode {
            w = w.u32(mode);
        }
        if let Some((atime, mtime)) = attrs.atime_mtime {
            w = w.u32(atime).u32(mtime);
        }
        w
    }
}

/// A reply's body, read field by field.
pub struct Reader<'a>(pub &'a [u8]);

fn malformed() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "malformed SFTP reply")
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> io::Result<&[u8]> {
        if self.0.len() < n {
            return Err(malformed());
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(head)
    }

    pub fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub fn bytes(&mut self) -> io::Result<Vec<u8>> {
        let len = self.u32()? as usize;
        Ok(self.take(len)?.to_vec())
    }

    pub fn string(&mut self) -> io::Result<String> {
        Ok(String::from_utf8_lossy(&self.bytes()?).into_owned())
    }

    pub fn attrs(&mut self) -> io::Result<Attrs> {
        let flags = self.u32()?;
        let mut attrs = Attrs::default();
        if flags & ATTR_SIZE != 0 {
            attrs.size = Some(self.u64()?);
        }
        if flags & ATTR_UIDGID != 0 {
            attrs.uid_gid = Some((self.u32()?, self.u32()?));
        }
        if flags & ATTR_PERMISSIONS != 0 {
            attrs.mode = Some(self.u32()?);
        }
        if flags & ATTR_ACMODTIME != 0 {
            attrs.atime_mtime = Some((self.u32()?, self.u32()?));
        }
        if flags & ATTR_EXTENDED != 0 {
            for _ in 0..self.u32()? {
                self.bytes()?;
                self.bytes()?;
            }
        }
        Ok(attrs)
    }
}

/// A reply to a request.
#[derive(Debug)]
pub enum Reply {
    Status { code: u32, message: String },
    Handle(Vec<u8>),
    Data(Vec<u8>),
    Name(Vec<(String, Attrs)>),
    Attrs(Attrs),
    Extended(Vec<u8>),
}

impl Reply {
    pub fn parse(kind: u8, body: &[u8]) -> io::Result<Self> {
        let mut r = Reader(body);
        Ok(match kind {
            STATUS => Self::Status {
                code: r.u32()?,
                // Very old servers leave out the message.
                message: r.string().unwrap_or_default(),
            },
            HANDLE => Self::Handle(r.bytes()?),
            DATA => Self::Data(r.bytes()?),
            NAME => {
                let count = r.u32()?;
                let mut names = Vec::with_capacity(count.min(4096) as usize);
                for _ in 0..count {
                    let name = r.string()?;
                    let _long_name = r.bytes()?;
                    names.push((name, r.attrs()?));
                }
                Self::Name(names)
            }
            ATTRS => Self::Attrs(r.attrs()?),
            EXTENDED_REPLY => Self::Extended(r.0.to_vec()),
            _ => return Err(malformed()),
        })
    }

    /// An error for anything but an OK status.
    pub fn ok(self) -> io::Result<()> {
        match self {
            Self::Status {
                code: STATUS_OK, ..
            } => Ok(()),
            other => Err(other.error()),
        }
    }

    pub fn handle(self) -> io::Result<Vec<u8>> {
        match self {
            Self::Handle(handle) => Ok(handle),
            other => Err(other.error()),
        }
    }

    pub fn attrs(self) -> io::Result<Attrs> {
        match self {
            Self::Attrs(attrs) => Ok(attrs),
            other => Err(other.error()),
        }
    }

    pub fn names(self) -> io::Result<Vec<(String, Attrs)>> {
        match self {
            Self::Name(names) => Ok(names),
            other => Err(other.error()),
        }
    }

    pub fn name(self) -> io::Result<String> {
        self.names()?
            .into_iter()
            .next()
            .map(|(name, _)| name)
            .ok_or_else(malformed)
    }

    pub fn is_eof(&self) -> bool {
        matches!(
            self,
            Self::Status {
                code: STATUS_EOF,
                ..
            }
        )
    }

    pub fn is_failure(&self) -> bool {
        matches!(
            self,
            Self::Status {
                code: STATUS_FAILURE,
                ..
            }
        )
    }

    /// What went wrong, as std would put it.
    pub fn error(self) -> io::Error {
        let Self::Status { code, message } = self else {
            return io::Error::new(io::ErrorKind::InvalidData, "unexpected SFTP reply");
        };
        let kind = match code {
            STATUS_EOF => io::ErrorKind::UnexpectedEof,
            STATUS_NO_SUCH_FILE => io::ErrorKind::NotFound,
            STATUS_PERMISSION_DENIED => io::ErrorKind::PermissionDenied,
            STATUS_OP_UNSUPPORTED => io::ErrorKind::Unsupported,
            _ => io::ErrorKind::Other,
        };
        // OpenSSH's messages are std's in all but case: "No such file".
        let message = if message.is_empty() {
            format!("SFTP error {code}")
        } else {
            message
        };
        io::Error::new(kind, message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attrs_round_trip() {
        let attrs = Attrs {
            size: Some(1 << 40),
            uid_gid: Some((1000, 100)),
            mode: Some(S_IFREG | 0o644),
            atime_mtime: Some((1, 2)),
        };
        let bytes = Writer::default().attrs(&attrs).0;
        assert_eq!(Reader(&bytes).attrs().unwrap(), attrs);
        let none = Writer::default().attrs(&Attrs::default()).0;
        assert_eq!(none, [0, 0, 0, 0]);
    }

    #[test]
    fn statuses_become_std_errors() {
        let status = |code| Reply::Status {
            code,
            message: "msg".into(),
        };
        assert!(status(STATUS_OK).ok().is_ok());
        let err = status(STATUS_NO_SUCH_FILE).ok().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert_eq!(err.to_string(), "msg");
        assert!(Reader(&[0, 0]).u32().is_err());
    }
}
