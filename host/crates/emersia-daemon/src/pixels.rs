//! Frame storage that avoids copying the capture buffer.
//!
//! The compositor writes a frame into a shared-memory file; reading it back
//! with `read()` and then writing it into ffmpeg's stdin copied every frame
//! twice on the CPU (~1 GB/s per 1080p60 stream). A [`Pixels::Mapped`] frame
//! is the capture buffer itself, mapped read-only, and the encoder `vmsplice`s
//! those pages into the pipe, so ffmpeg's own read is the only copy left.
//! Every capture uses a fresh buffer, so nothing overwrites pages that are
//! still referenced by the pipe.

use std::ops::Deref;
use std::os::fd::AsRawFd as _;

/// A read-only mapping of a capture buffer.
pub struct Mapping {
    ptr: *mut libc::c_void,
    len: usize,
}

// SAFETY: the mapping is read-only and never aliased mutably; it can be read
// from any thread and unmapped from any thread.
unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}

impl Mapping {
    /// Map the first `len` bytes of `file` read-only.
    pub fn new(file: &std::fs::File, len: usize) -> std::io::Result<Self> {
        if len == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "empty capture buffer",
            ));
        }
        // SAFETY: valid fd and length; the result is checked against MAP_FAILED.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self { ptr, len })
    }
}

impl Deref for Mapping {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        // SAFETY: ptr/len describe a live read-only mapping owned by self.
        unsafe { std::slice::from_raw_parts(self.ptr.cast::<u8>(), self.len) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: unmapping exactly what mmap returned. Pages already
        // vmspliced into a pipe stay referenced by the kernel.
        unsafe {
            libc::munmap(self.ptr, self.len);
        }
    }
}

/// Packed pixels: owned, or the mapped capture buffer itself.
pub enum Pixels {
    Owned(Vec<u8>),
    Mapped(Mapping),
}

impl Pixels {
    /// An owned copy (for the rare paths that rearrange pixels).
    pub fn into_vec(self) -> Vec<u8> {
        match self {
            Self::Owned(v) => v,
            Self::Mapped(m) => m.to_vec(),
        }
    }

    pub fn is_mapped(&self) -> bool {
        matches!(self, Self::Mapped(_))
    }
}

impl Deref for Pixels {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            Self::Owned(v) => v,
            Self::Mapped(m) => m,
        }
    }
}

impl From<Vec<u8>> for Pixels {
    fn from(v: Vec<u8>) -> Self {
        Self::Owned(v)
    }
}

impl std::fmt::Debug for Pixels {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Pixels::{}({} bytes)",
            if self.is_mapped() { "Mapped" } else { "Owned" },
            self.len()
        )
    }
}

impl PartialEq for Pixels {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}
impl Eq for Pixels {}

impl Clone for Pixels {
    fn clone(&self) -> Self {
        Self::Owned(self.to_vec())
    }
}

/// Write all of `data` into the pipe `fd` with `vmsplice`, so the kernel
/// references the pages instead of copying them. Falls back to the caller on
/// error (e.g. `fd` is not a pipe).
pub fn vmsplice_all(fd: std::os::fd::RawFd, mut data: &[u8]) -> std::io::Result<()> {
    while !data.is_empty() {
        let iov = libc::iovec {
            iov_base: data.as_ptr() as *mut libc::c_void,
            iov_len: data.len(),
        };
        // SAFETY: iov describes readable memory that outlives the call; the
        // kernel takes page references (no SPLICE_F_GIFT: pages are not given
        // away, and the buffer is never written again).
        let n = unsafe { libc::vmsplice(fd, &iov, 1, 0) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        data = &data[n as usize..];
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};

    #[test]
    fn mapping_reads_the_file_without_copying_it_in() {
        let mut f = tempfile::tempfile().unwrap();
        f.write_all(&[7u8; 8192]).unwrap();
        let m = Mapping::new(&f, 8192).unwrap();
        assert_eq!(m.len(), 8192);
        assert!(m.iter().all(|&b| b == 7));
        let p = Pixels::Mapped(m);
        assert!(p.is_mapped());
        assert_eq!(p.clone().into_vec(), vec![7u8; 8192]);
    }

    #[test]
    fn vmsplice_delivers_every_byte_through_a_pipe() {
        let mut fds = [0; 2];
        // SAFETY: plain pipe creation into a 2-int array.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let expected = data.clone();
        let reader = std::thread::spawn(move || {
            // SAFETY: we own the read end.
            let mut r = unsafe { <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(fds[0]) };
            let mut got = Vec::new();
            r.read_to_end(&mut got).unwrap();
            got
        });
        vmsplice_all(fds[1], &data).unwrap();
        // SAFETY: closing our write end so the reader sees EOF.
        unsafe { libc::close(fds[1]) };
        assert_eq!(reader.join().unwrap(), expected);
    }
}
