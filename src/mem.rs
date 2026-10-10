//! Big buffers (pictures, shared buffers for the compositor) as their own
//! memory mappings, so they go back to the system the moment they're
//! dropped, instead of staying in the heap and keeping the tray app big.
//!
//! This is one of the few places with `unsafe`: it holds raw mappings and
//! hands them out as slices tied to the borrow of the `Mapping`.

use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::ptr::NonNull;

use rustix::fs::{MemfdFlags, SealFlags};
use rustix::mm::{MapFlags, ProtFlags};

/// The largest buffer handed out: a 16K x 16K picture at 4 bytes a pixel.
pub const MAX_LEN: usize = 1 << 30;

pub struct Mapping {
    ptr: NonNull<u8>,
    len: usize,
    fd: Option<OwnedFd>,
}

// SAFETY: a Mapping owns its memory exclusively; shared access goes through
// &self (read-only slices) and mutation through &mut self, so the usual
// borrow rules make sending and sharing it across threads sound.
unsafe impl Send for Mapping {}
// SAFETY: see above; &Mapping only ever gives out shared slices.
unsafe impl Sync for Mapping {}

impl Mapping {
    /// Private memory, zeroed. `populate` faults it all in now (do that
    /// while waiting for something else, so it costs nothing later).
    pub fn anonymous(len: usize, populate: bool) -> io::Result<Mapping> {
        let len = check_len(len)?;
        let mut flags = MapFlags::PRIVATE | MapFlags::NORESERVE;
        if populate {
            flags |= MapFlags::POPULATE;
        }
        // SAFETY: a fresh anonymous mapping aliases nothing.
        let ptr = unsafe { rustix::mm::mmap_anonymous(std::ptr::null_mut(), len, ProtFlags::READ | ProtFlags::WRITE, flags) }.map_err(io::Error::from)?;
        // (No MADV_HUGEPAGE: with the usual THP setting, defrag=madvise, a
        // fault in such a mapping compacts memory first, synchronously, and
        // a 4K picture took 130 ms to copy instead of a few.)
        Ok(Mapping { ptr: NonNull::new(ptr.cast()).ok_or_else(|| io::Error::other("mmap gave null"))?, len, fd: None })
    }

    /// Memory in a memfd, to share with the compositor (a wl_shm pool) or
    /// another process. Sealed against shrinking, so nobody holding the fd
    /// can pull the pages out from under us.
    pub fn memfd(name: &str, len: usize, populate: bool) -> io::Result<Mapping> {
        let len = check_len(len)?;
        let fd = rustix::fs::memfd_create(name, MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING).map_err(io::Error::from)?;
        rustix::fs::ftruncate(&fd, len as u64).map_err(io::Error::from)?;
        rustix::fs::fcntl_add_seals(&fd, SealFlags::SHRINK | SealFlags::SEAL).map_err(io::Error::from)?;
        let mut flags = MapFlags::SHARED;
        if populate {
            flags |= MapFlags::POPULATE;
        }
        // SAFETY: mapping a memfd of exactly `len` bytes that we just made.
        let ptr = unsafe { rustix::mm::mmap(std::ptr::null_mut(), len, ProtFlags::READ | ProtFlags::WRITE, flags, &fd, 0) }.map_err(io::Error::from)?;
        Ok(Mapping { ptr: NonNull::new(ptr.cast()).ok_or_else(|| io::Error::other("mmap gave null"))?, len, fd: Some(fd) })
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: ptr..ptr+len is our mapping, readable and alive while &self is.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: as above, and &mut self makes this the only access.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }

    pub fn fd(&self) -> Option<BorrowedFd<'_>> {
        self.fd.as_ref().map(|f| f.as_fd())
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: unmapping exactly what we mapped; no slice outlives &self.
        let _ = unsafe { rustix::mm::munmap(self.ptr.as_ptr().cast(), self.len) };
    }
}

fn check_len(len: usize) -> io::Result<usize> {
    if len == 0 || len > MAX_LEN {
        return Err(io::Error::other(format!("buffer of {len} bytes refused")));
    }
    Ok(len)
}

/// Make freed heap memory go back to the system promptly. Pictures use
/// their own mappings already; this keeps glibc from hoarding the rest.
pub fn tune_allocator() {
    #[cfg(target_env = "gnu")]
    {
        const M_TRIM_THRESHOLD: libc::c_int = -1;
        const M_MMAP_THRESHOLD: libc::c_int = -3;
        // SAFETY: mallopt only adjusts glibc's allocator settings.
        unsafe {
            libc::mallopt(M_MMAP_THRESHOLD, 256 * 1024);
            libc::mallopt(M_TRIM_THRESHOLD, 512 * 1024);
        }
    }
}

/// Return what glibc kept of freed memory (after a capture).
pub fn trim() {
    #[cfg(target_env = "gnu")]
    // SAFETY: malloc_trim only releases free memory.
    unsafe {
        libc::malloc_trim(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps() {
        let mut m = Mapping::anonymous(1 << 20, true).unwrap();
        assert!(m.as_slice().iter().all(|&b| b == 0));
        m.as_mut_slice()[123] = 7;
        assert_eq!(m.as_slice()[123], 7);
        let mut f = Mapping::memfd("test", 4096, false).unwrap();
        f.as_mut_slice()[0] = 1;
        assert!(f.fd().is_some());
        assert!(Mapping::anonymous(0, false).is_err());
        assert!(Mapping::anonymous(MAX_LEN + 1, false).is_err());
    }
}
