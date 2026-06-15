//! A tiny, safe memory-mapping wrapper built from scratch over the libc
//! `mmap`/`munmap` syscalls (no `memmap2` dependency — std already links libc).
//!
//! Used to load the index without heap-copying the whole file: the OS maps the
//! file's pages into our address space and pages them in on demand, so the
//! decode reads straight from the mapping. (The postings are still decoded into
//! owned in-memory structures — fully lazy, per-term zero-copy decode that keeps
//! the file mapped is the segmented-index follow-up noted in PLAN.md.)
//!
//! `unsafe` is confined to this module behind a safe `Mmap` RAII type that
//! unmaps on drop.

#[cfg(unix)]
mod imp {
    use std::ffi::c_void;
    use std::fs::File;
    use std::io;
    use std::os::unix::io::AsRawFd;
    use std::path::Path;
    use std::slice;

    const PROT_READ: i32 = 0x1;
    const MAP_PRIVATE: i32 = 0x2;

    extern "C" {
        fn mmap(
            addr: *mut c_void,
            len: usize,
            prot: i32,
            flags: i32,
            fd: i32,
            offset: i64,
        ) -> *mut c_void;
        fn munmap(addr: *mut c_void, len: usize) -> i32;
    }

    /// A read-only memory map of a file. Unmaps on drop.
    pub struct Mmap {
        ptr: *mut c_void,
        len: usize,
    }

    // SAFETY: the mapping is read-only and immutable for its whole lifetime, so
    // sharing `&Mmap` (and moving it) across threads is sound. Needed because the
    // server shares `Arc<Index>` (which owns mmapped segments) across threads.
    unsafe impl Send for Mmap {}
    unsafe impl Sync for Mmap {}

    impl Mmap {
        /// Map `path` read-only. A zero-length file maps to an empty slice.
        pub fn open(path: &Path) -> io::Result<Mmap> {
            let file = File::open(path)?;
            let len = file.metadata()?.len() as usize;
            if len == 0 {
                return Ok(Mmap {
                    ptr: std::ptr::null_mut(),
                    len: 0,
                });
            }
            // SAFETY: fd is valid for the duration of the call; the mapping keeps
            // its own reference to the file, so it's fine to drop `file` after.
            let ptr = unsafe {
                mmap(
                    std::ptr::null_mut(),
                    len,
                    PROT_READ,
                    MAP_PRIVATE,
                    file.as_raw_fd(),
                    0,
                )
            };
            if ptr == usize::MAX as *mut c_void {
                // MAP_FAILED
                return Err(io::Error::last_os_error());
            }
            Ok(Mmap { ptr, len })
        }

        /// The mapped bytes.
        pub fn as_slice(&self) -> &[u8] {
            if self.len == 0 {
                return &[];
            }
            // SAFETY: ptr/len describe a valid read-only mapping for `&self`'s life.
            unsafe { slice::from_raw_parts(self.ptr as *const u8, self.len) }
        }
    }

    impl Drop for Mmap {
        fn drop(&mut self) {
            if self.len > 0 && !self.ptr.is_null() {
                // SAFETY: ptr/len came from a successful mmap and are unmapped once.
                unsafe {
                    munmap(self.ptr, self.len);
                }
            }
        }
    }
}

#[cfg(unix)]
pub use imp::Mmap;
