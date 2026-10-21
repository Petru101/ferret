// A game's memory, opened once at attach: every engine reader and scan reads it through this,
// so a Windows helper only has to supply its own `Mem` (ReadProcessMemory/WriteProcessMemory).
// Linux: `/proc/<pid>/mem`.

use std::fs::File;
use std::io;
#[cfg(unix)]
use std::os::unix::fs::FileExt;

pub struct Mem(File);

impl Mem {
    /// An open `/proc/<pid>/mem`.
    pub fn new(file: File) -> Mem {
        Mem(file)
    }

    pub fn try_clone(&self) -> io::Result<Mem> {
        self.0.try_clone().map(Mem)
    }

    /// Reads from `addr` on, stopping early at memory that can't be read; `Ok(0)` or an error
    /// when its first page can't be.
    pub fn read_at(&self, buf: &mut [u8], addr: u64) -> io::Result<usize> {
        FileExt::read_at(&self.0, buf, addr)
    }

    pub fn read_exact_at(&self, buf: &mut [u8], addr: u64) -> io::Result<()> {
        FileExt::read_exact_at(&self.0, buf, addr)
    }

    pub fn write_all_at(&self, buf: &[u8], addr: u64) -> io::Result<()> {
        FileExt::write_all_at(&self.0, buf, addr)
    }
}

/// Reading and writing a file at an offset, on every OS (std's `FileExt` differs between Unix
/// and Windows).
pub trait FileAt {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()>;
    fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()>;
}

#[cfg(unix)]
impl FileAt for File {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        FileExt::read_exact_at(self, buf, offset)
    }

    fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        FileExt::write_all_at(self, buf, offset)
    }
}

#[cfg(windows)]
impl FileAt for File {
    fn read_exact_at(&self, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
        use std::os::windows::fs::FileExt;
        while !buf.is_empty() {
            match self.seek_read(buf, offset)? {
                0 => return Err(io::ErrorKind::UnexpectedEof.into()),
                n => {
                    buf = &mut buf[n..];
                    offset += n as u64;
                }
            }
        }
        Ok(())
    }

    fn write_all_at(&self, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
        use std::os::windows::fs::FileExt;
        while !buf.is_empty() {
            match self.seek_write(buf, offset)? {
                0 => return Err(io::ErrorKind::WriteZero.into()),
                n => {
                    buf = &buf[n..];
                    offset += n as u64;
                }
            }
        }
        Ok(())
    }
}
