//! An open file on an SFTP server. Reading front to back asks for what's
//! next ahead of time, and writes don't wait for each to be acknowledged:
//! otherwise every chunk would cost a round trip to the server.

use std::{
    collections::VecDeque,
    io::{self, Read, Seek, SeekFrom, Write},
    sync::Arc,
    time::SystemTime,
};

use super::{
    Location,
    session::{Pending, Session},
    wire::{self, Attrs, Reply, Writer},
};

/// The most reads asked for ahead, once reading has gone on a while.
const MAX_READ_AHEAD: usize = 32;
/// The most asked for by a read that may be all that's wanted (after opening
/// or seeking): enough for a look at a file's start, without the wait and the
/// traffic of a whole read's worth that may go unused.
const FIRST_READ: u32 = 72 * 1024;
/// Reads asked for at once for the rest of a file of unknown size.
const WHOLE_FILE_READ_AHEAD: usize = 4;
/// The most writes on their way at once.
const MAX_WRITES_IN_FLIGHT: usize = 64;

pub struct File {
    session: Arc<Session>,
    handle: Vec<u8>,
    location: Location,
    pos: u64,
    append: bool,
    /// The last data read, of which what's from `buffer_start` is still to
    /// be taken; `pos` is there. What's been taken stays for seeking back.
    buffer: Vec<u8>,
    buffer_start: usize,
    /// Reads asked for: where each starts, how much, and its reply.
    ahead: VecDeque<(u64, u32, Pending)>,
    /// Where the next read ahead starts.
    next_ahead: u64,
    /// How many reads to keep asked for; doubles as reading goes on.
    window: usize,
    writes: VecDeque<Pending>,
    /// A write that failed, reported at the next chance.
    write_error: Option<io::Error>,
    /// The end of the file, once a read reached it.
    eof: bool,
    /// The file's size when it was opened, if that was known without asking.
    known_len: Option<u64>,
    wrote: bool,
    #[cfg(test)]
    pub(super) reads_asked: u32,
}

impl File {
    pub(super) fn new(
        session: Arc<Session>,
        handle: Vec<u8>,
        location: Location,
        append: bool,
        known_len: Option<u64>,
    ) -> Self {
        Self {
            session,
            handle,
            location,
            pos: 0,
            append,
            buffer: Vec::new(),
            buffer_start: 0,
            ahead: VecDeque::new(),
            next_ahead: 0,
            window: 1,
            writes: VecDeque::new(),
            write_error: None,
            eof: false,
            known_len,
            wrote: false,
            #[cfg(test)]
            reads_asked: 0,
        }
    }

    pub fn location(&self) -> &Location {
        &self.location
    }

    #[cfg(all(test, unix))]
    pub(super) fn session_max_read(&self) -> u32 {
        self.session.max_read
    }

    #[cfg(all(test, unix))]
    pub(super) fn on_session(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.session, &other.session)
    }

    #[cfg(all(test, unix))]
    pub(super) fn is_on(&self, session: &Arc<Session>) -> bool {
        Arc::ptr_eq(&self.session, session)
    }

    fn buffered(&self) -> &[u8] {
        &self.buffer[self.buffer_start..]
    }

    /// Forgets what was read ahead, after a seek or write.
    fn drop_read_ahead(&mut self) {
        self.buffer.clear();
        self.buffer_start = 0;
        // Their replies are thrown away as they come.
        self.ahead.clear();
        self.next_ahead = self.pos;
        self.window = 1;
        self.eof = false;
    }

    fn ask_ahead(&mut self) -> io::Result<()> {
        // Past the end, only to find out it's the end when nothing else is
        // left to ask for: the file may have grown.
        while self.ahead.len() < self.window
            && (self.ahead.is_empty() || self.known_len.is_none_or(|len| self.next_ahead < len))
        {
            let len = if self.window == 1 {
                self.session.max_read.min(FIRST_READ)
            } else {
                self.session.max_read
            };
            #[cfg(test)]
            {
                self.reads_asked += 1;
            }
            let request = Writer::default()
                .bytes(&self.handle)
                .u64(self.next_ahead)
                .u32(len);
            let pending = self.session.request(wire::READ, request)?;
            self.ahead.push_back((self.next_ahead, len, pending));
            self.next_ahead += u64::from(len);
        }
        Ok(())
    }

    /// Waits for every write so far, returning the first that failed.
    fn finish_writes(&mut self) -> io::Result<()> {
        while let Some(pending) = self.writes.pop_front() {
            if let Err(err) = pending.wait().and_then(Reply::ok) {
                self.write_error.get_or_insert(err);
            }
        }
        match self.write_error.take() {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    pub fn metadata(&mut self) -> io::Result<Attrs> {
        self.finish_writes()?;
        self.session
            .call(wire::FSTAT, Writer::default().bytes(&self.handle))?
            .attrs()
    }

    fn set_attrs(&mut self, attrs: &Attrs) -> io::Result<()> {
        self.finish_writes()?;
        self.session
            .call(
                wire::FSETSTAT,
                Writer::default().bytes(&self.handle).attrs(attrs),
            )?
            .ok()?;
        super::changed(&self.location);
        Ok(())
    }

    pub fn set_len(&mut self, len: u64) -> io::Result<()> {
        self.drop_read_ahead();
        self.known_len = None;
        self.set_attrs(&Attrs {
            size: Some(len),
            ..Attrs::default()
        })
    }

    pub fn set_times(
        &mut self,
        accessed: Option<SystemTime>,
        modified: SystemTime,
    ) -> io::Result<()> {
        let accessed = match accessed {
            Some(time) => wire::seconds(time),
            // SFTP sets both or neither; keep when it was last read.
            None => self
                .metadata()?
                .atime_mtime
                .map_or_else(|| wire::seconds(modified), |(a, _)| a),
        };
        self.set_attrs(&Attrs {
            atime_mtime: Some((accessed, wire::seconds(modified))),
            ..Attrs::default()
        })
    }

    pub fn set_mode(&mut self, mode: u32) -> io::Result<()> {
        self.set_attrs(&Attrs {
            mode: Some(mode & 0o7777),
            ..Attrs::default()
        })
    }

    /// Copies `len` bytes at `offset` into `to`, at the same offset, on the
    /// server itself (OpenSSH's copy-data). `false` if it can't: another
    /// server, or one without the extension.
    pub fn copy_on_server(&mut self, to: &mut Self, offset: u64, len: u64) -> io::Result<bool> {
        if !Arc::ptr_eq(&self.session, &to.session) || !self.session.has_extension("copy-data") {
            return Ok(false);
        }
        self.finish_writes()?;
        to.finish_writes()?;
        let request = Writer::default()
            .bytes(&self.handle)
            .u64(offset)
            .u64(len)
            .bytes(&to.handle)
            .u64(offset);
        self.session.extended("copy-data", request)?.wait()?.ok()?;
        super::changed(&to.location);
        Ok(true)
    }

    pub fn sync(&mut self) -> io::Result<()> {
        self.finish_writes()?;
        if self.session.has_extension("fsync@openssh.com") {
            self.session
                .extended("fsync@openssh.com", Writer::default().bytes(&self.handle))?
                .wait()?
                .ok()?;
        }
        Ok(())
    }
}

impl Read for File {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        self.finish_writes()?;
        loop {
            if !self.buffered().is_empty() {
                let n = out.len().min(self.buffered().len());
                out[..n].copy_from_slice(&self.buffered()[..n]);
                self.buffer_start += n;
                self.pos += n as u64;
                return Ok(n);
            }
            if self.eof {
                return Ok(0);
            }
            self.ask_ahead()?;
            let (offset, asked, pending) = self.ahead.pop_front().expect("just asked");
            debug_assert_eq!(offset, self.pos);
            match pending.wait()? {
                Reply::Data(data) => {
                    let len = data.len() as u64;
                    self.buffer = data;
                    self.buffer_start = 0;
                    if len < u64::from(asked) {
                        // A short read: what's asked for after it starts
                        // too late, so ask again from where this ends,
                        // unless that's the end anyway.
                        self.ahead.clear();
                        self.next_ahead = offset + len;
                        if self.known_len == Some(offset + len) {
                            self.eof = true;
                        }
                    } else {
                        self.window = (self.window * 2).min(MAX_READ_AHEAD);
                    }
                    if len == 0 {
                        self.eof = true;
                    }
                }
                reply if reply.is_eof() => {
                    self.ahead.clear();
                    self.eof = true;
                }
                reply => return Err(reply.error()),
            }
        }
    }

    /// All the rest is wanted, so it's asked for at once where its size is
    /// known, rather than working up to it round trip by round trip.
    fn read_to_end(&mut self, out: &mut Vec<u8>) -> io::Result<usize> {
        let reads = match self.known_len {
            Some(len) => usize::try_from(
                len.saturating_sub(self.pos)
                    .div_ceil(u64::from(self.session.max_read)),
            )
            .unwrap_or(MAX_READ_AHEAD),
            None => WHOLE_FILE_READ_AHEAD,
        };
        self.window = self.window.max(reads.clamp(2, MAX_READ_AHEAD));
        Read::read_to_end(&mut Reads(self), out)
    }
}

/// std's way of reading to the end, by [`File`]'s `read`.
struct Reads<'a>(&'a mut File);

impl Read for Reads<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        self.0.read(out)
    }
}

impl Write for File {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if let Some(err) = self.write_error.take() {
            return Err(err);
        }
        if !self.ahead.is_empty() || !self.buffer.is_empty() {
            self.drop_read_ahead();
        }
        let n = data.len().min(self.session.max_write as usize);
        if n == 0 {
            return Ok(0);
        }
        // OpenSSH's server opens appending files with O_APPEND, so the
        // offset doesn't matter there.
        let request = Writer::default()
            .bytes(&self.handle)
            .u64(self.pos)
            .bytes(&data[..n]);
        let pending = self.session.request(wire::WRITE, request)?;
        self.writes.push_back(pending);
        self.wrote = true;
        self.known_len = None;
        if !self.append {
            self.pos += n as u64;
        }
        self.next_ahead = self.pos;
        super::changed(&self.location);
        while self.writes.len() > MAX_WRITES_IN_FLIGHT {
            let oldest = self.writes.pop_front().expect("more than the most");
            oldest.wait().and_then(Reply::ok)?;
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.finish_writes()
    }
}

impl Seek for File {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let target = match from {
            SeekFrom::Start(at) => Some(at),
            SeekFrom::Current(by) => self.pos.checked_add_signed(by),
            SeekFrom::End(by) => {
                let len = self.metadata()?.size.unwrap_or(0);
                len.checked_add_signed(by)
            }
        };
        let Some(target) = target else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid seek to a negative position",
            ));
        };
        if target == self.pos {
            return Ok(target);
        }
        // Within what's been read: keep it, and what's asked for after it.
        let ahead_of = target.checked_sub(self.pos);
        if let Some(skip) = ahead_of.filter(|&n| n < self.buffered().len() as u64) {
            self.buffer_start += skip as usize;
            self.pos = target;
            return Ok(target);
        }
        // Back within it, as when something looks at the start of a file to
        // tell what it is, and then reads it from the top.
        let behind = self.pos.checked_sub(target);
        if let Some(back) = behind.filter(|&n| n <= self.buffer_start as u64) {
            self.buffer_start -= back as usize;
            self.pos = target;
            return Ok(target);
        }
        self.finish_writes()?;
        self.pos = target;
        self.drop_read_ahead();
        Ok(target)
    }
}

impl Drop for File {
    fn drop(&mut self) {
        let _ = self.finish_writes();
        if let Ok(pending) = self
            .session
            .request(wire::CLOSE, Writer::default().bytes(&self.handle))
            && self.wrote
        {
            // Wait, so that once a file's dropped, it's all on the server
            // for whoever reads it next. Nothing's riding on it otherwise.
            let _ = pending.wait();
        }
    }
}
