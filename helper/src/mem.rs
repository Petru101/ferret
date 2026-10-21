// A game's memory, opened once at attach: every engine reader and scan reads it through this.
// Linux: `/proc/<pid>/mem`; Windows: a process handle (ReadProcessMemory/WriteProcessMemory).

use std::fs::File;
use std::io;
#[cfg(unix)]
use std::os::unix::fs::FileExt;

#[cfg(unix)]
pub struct Mem(File);

#[cfg(unix)]
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

#[cfg(windows)]
pub struct Mem(std::sync::Arc<crate::win32::Handle>);

#[cfg(windows)]
impl Mem {
    /// An open process handle with read, write and operation rights.
    pub fn new(h: crate::win32::Handle) -> Mem {
        Mem(std::sync::Arc::new(h))
    }

    pub fn try_clone(&self) -> io::Result<Mem> {
        Ok(Mem(self.0.clone()))
    }

    fn read_some(&self, buf: &mut [u8], addr: u64) -> usize {
        let mut n = 0;
        unsafe { crate::win32::ReadProcessMemory(self.0 .0, addr as *const _, buf.as_mut_ptr().cast(), buf.len(), &mut n) };
        n
    }

    /// Reads from `addr` on, stopping early at memory that can't be read; an error when its
    /// first page can't be. ReadProcessMemory reads all or nothing across pages, so a failed
    /// read goes on page by page.
    pub fn read_at(&self, buf: &mut [u8], addr: u64) -> io::Result<usize> {
        let n = self.read_some(buf, addr);
        if n == buf.len() {
            return Ok(n);
        }
        let mut done = 0;
        while done < buf.len() {
            let page_end = ((addr + done as u64) | 0xfff) + 1;
            let len = ((page_end - (addr + done as u64)) as usize).min(buf.len() - done);
            if self.read_some(&mut buf[done..done + len], addr + done as u64) != len {
                break;
            }
            done += len;
        }
        if done == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(done)
    }

    pub fn read_exact_at(&self, buf: &mut [u8], addr: u64) -> io::Result<()> {
        match self.read_some(buf, addr) == buf.len() {
            true => Ok(()),
            false => Err(io::Error::last_os_error()),
        }
    }

    pub fn write_all_at(&self, buf: &[u8], addr: u64) -> io::Result<()> {
        let mut n = 0;
        let ok = unsafe { crate::win32::WriteProcessMemory(self.0 .0, addr as *mut _, buf.as_ptr().cast(), buf.len(), &mut n) };
        match ok != 0 && n == buf.len() {
            true => Ok(()),
            false => Err(io::Error::last_os_error()),
        }
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
