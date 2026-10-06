//! Guest pages as one host address, for guest-memory blobs (docs/VENUS.md
//! "Guest-memory blobs").
//!
//! The backend resolves a guest's page list to runs of the guest RAM file
//! (the memfd QEMU backs the VM's memory with) and sends that file and the
//! runs here. The renderer maps each run, in order, into one span of its own
//! address space, so the span is the guest's pages page for page. vkr then
//! imports the span with `VK_EXT_external_memory_host`, and the GPU writes
//! straight into the pages the guest reads.
//!
//! Every span comes out of one [`Arena`], an address range reserved at first
//! use and used for nothing else. That is what keeps a stale pointer
//! harmless. The render worker is a thread that serves its requests in its
//! own time: a guest can unref a resource while an import of it is still
//! queued there. The import then sees a span that was given back. A freed
//! span is turned back into an inaccessible reservation (which an import
//! fails on), and the arena hands it out again only for guest pages. So a
//! late import reaches this guest's own memory, or nothing. It never reaches
//! the renderer's heap, which a plain `munmap` and a later `mmap` could put
//! at the same address.

use crate::{Error, PageRun, Result};
use std::collections::BTreeMap;
use std::os::fd::{AsRawFd, BorrowedFd};

const PAGE: u64 = 4096;

/// Address space reserved for guest spans. Address space, not memory: a
/// guest blob is pages the guest already has.
pub const ARENA_BYTES: usize = 64 << 30;

/// The most runs one import may carry. Matches the backend's bound.
pub const MAX_RUNS: usize = 4096;

/// The most mappings all live spans may hold together. Each run is one
/// mapping, and the process's map count is bounded (`vm.max_map_count`,
/// 65530 by default), with the driver's own mappings to fit beside these.
pub const MAX_LIVE_RUNS: usize = 32768;

/// One span of guest pages, as mapped by [`Arena::map`].
#[derive(Debug, PartialEq, Eq)]
pub struct Span {
    pub addr: usize,
    pub len: usize,
    runs: usize,
}

/// The reserved range and what is free in it.
#[derive(Debug)]
pub struct Arena {
    base: usize,
    len: usize,
    /// Free ranges, as offset to length. Never adjacent: freeing coalesces.
    free: BTreeMap<usize, usize>,
    live_runs: usize,
}

/// Adjacent runs merged: one mapping where the file is contiguous too.
pub fn coalesce(runs: &[PageRun]) -> Vec<PageRun> {
    let mut out: Vec<PageRun> = Vec::with_capacity(runs.len());
    for r in runs {
        match out.last_mut() {
            Some(last) if last.offset.checked_add(last.len) == Some(r.offset) => last.len += r.len,
            _ => out.push(*r),
        }
    }
    out
}

/// The checks every run list must pass, whoever sent it: nonempty, at most
/// [`MAX_RUNS`], every run whole pages inside the file, the total within
/// `max`. Returns the total.
pub fn check_runs(runs: &[PageRun], file_len: u64, max: u64) -> Result<u64> {
    if runs.is_empty() || runs.len() > MAX_RUNS {
        return Err(Error::Refused(format!("guest pages: {} runs", runs.len())));
    }
    let mut total = 0u64;
    for r in runs {
        let end = r.offset.checked_add(r.len);
        if r.len == 0 || r.offset % PAGE != 0 || r.len % PAGE != 0 || end.is_none_or(|e| e > file_len) {
            return Err(Error::Refused(format!(
                "guest pages: run {:#x}+{:#x} is not whole pages of the {file_len:#x}-byte file",
                r.offset, r.len
            )));
        }
        total = total.saturating_add(r.len);
    }
    if total > max {
        return Err(Error::Refused(format!("guest pages: {total} bytes, more than {max}")));
    }
    Ok(total)
}

fn file_len(fd: BorrowedFd<'_>) -> Result<u64> {
    // SAFETY: fstat into a zeroed struct on a descriptor we hold.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(st.st_size as u64)
}

impl Arena {
    /// Reserve `len` bytes of address space (a multiple of the page size).
    pub fn new(len: usize) -> Result<Self> {
        // SAFETY: a fresh inaccessible reservation; nothing is backed.
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        if p == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(Self { base: p as usize, len, free: BTreeMap::from([(0, len)]), live_runs: 0 })
    }

    /// Mappings held by live spans.
    pub fn live_runs(&self) -> usize {
        self.live_runs
    }

    /// Whether `addr` is inside the arena.
    pub fn contains(&self, addr: usize) -> bool {
        addr >= self.base && addr < self.base + self.len
    }

    /// Map `runs` of `ram`, in order, as one span. Adjacent runs are merged
    /// first. Nothing stays mapped on failure.
    pub fn map(&mut self, ram: BorrowedFd<'_>, runs: &[PageRun]) -> Result<Span> {
        let total = check_runs(runs, file_len(ram)?, self.len as u64)? as usize;
        let runs = coalesce(runs);
        if self.live_runs + runs.len() > MAX_LIVE_RUNS {
            return Err(Error::Refused(format!(
                "guest pages: {} mappings live, {} more would pass {MAX_LIVE_RUNS}",
                self.live_runs,
                runs.len()
            )));
        }
        let at = self
            .take(total)
            .ok_or_else(|| Error::Refused(format!("guest pages: no {total}-byte range left in the arena")))?;
        let addr = self.base + at;
        let mut done = 0usize;
        for r in &runs {
            // SAFETY: the target lies inside the arena range just taken,
            // which nothing else maps; MAP_FIXED replaces only that range.
            let p = unsafe {
                libc::mmap(
                    (addr + done) as *mut libc::c_void,
                    r.len as usize,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED | libc::MAP_FIXED,
                    ram.as_raw_fd(),
                    r.offset as libc::off_t,
                )
            };
            if p == libc::MAP_FAILED {
                let e = std::io::Error::last_os_error();
                self.give_back(at, total);
                return Err(e.into());
            }
            done += r.len as usize;
        }
        self.live_runs += runs.len();
        Ok(Span { addr, len: total, runs: runs.len() })
    }

    /// Give a span back: inaccessible again, and free for guest pages only.
    pub fn unmap(&mut self, span: Span) {
        debug_assert!(self.contains(span.addr));
        self.live_runs = self.live_runs.saturating_sub(span.runs);
        self.give_back(span.addr - self.base, span.len);
    }

    /// Replace `[at, at + len)` with a fresh reservation and free it.
    fn give_back(&mut self, at: usize, len: usize) {
        // SAFETY: the range is inside the arena; MAP_FIXED drops the guest
        // pages' mappings there and leaves an inaccessible reservation.
        let p = unsafe {
            libc::mmap(
                (self.base + at) as *mut libc::c_void,
                len,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE | libc::MAP_FIXED,
                -1,
                0,
            )
        };
        if p == libc::MAP_FAILED {
            // The range keeps the guest's pages and is never handed out
            // again. Only address space is lost.
            eprintln!(
                "conduit-venus: guest pages: resetting {len:#x} bytes of the arena: {}",
                std::io::Error::last_os_error()
            );
            return;
        }
        self.free_range(at, len);
    }

    /// First fit.
    fn take(&mut self, len: usize) -> Option<usize> {
        let (&at, &have) = self.free.iter().find(|&(_, &l)| l >= len)?;
        self.free.remove(&at);
        if have > len {
            self.free.insert(at + len, have - len);
        }
        Some(at)
    }

    fn free_range(&mut self, mut at: usize, mut len: usize) {
        if let Some((&prev, &plen)) = self.free.range(..at).next_back()
            && prev + plen == at
        {
            self.free.remove(&prev);
            at = prev;
            len += plen;
        }
        if let Some(&next_len) = self.free.get(&(at + len)) {
            self.free.remove(&(at + len));
            len += next_len;
        }
        self.free.insert(at, len);
    }

    /// Free ranges, for tests.
    #[cfg(test)]
    fn free_ranges(&self) -> Vec<(usize, usize)> {
        self.free.iter().map(|(&a, &l)| (a, l)).collect()
    }
}

impl Drop for Arena {
    fn drop(&mut self) {
        // SAFETY: the whole reservation, spans included.
        unsafe { libc::munmap(self.base as *mut libc::c_void, self.len) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;

    fn run(offset: u64, len: u64) -> PageRun {
        PageRun { offset, len }
    }

    /// A memfd whose page i starts with the byte i.
    fn ram(pages: u64) -> std::os::fd::OwnedFd {
        let fd = crate::mock::memfd(pages * PAGE).unwrap();
        for i in 0..pages {
            // SAFETY: pwrite of one byte at a page start of our memfd.
            let n = unsafe { libc::pwrite(fd.as_raw_fd(), [i as u8].as_ptr().cast(), 1, (i * PAGE) as i64) };
            assert_eq!(n, 1);
        }
        fd
    }

    fn byte(addr: usize) -> u8 {
        // SAFETY: tests read only inside spans they mapped.
        unsafe { std::ptr::read_volatile(addr as *const u8) }
    }

    #[test]
    fn runs_merge_only_where_the_file_is_contiguous() {
        let r = coalesce(&[run(0, PAGE), run(PAGE, PAGE), run(5 * PAGE, PAGE), run(3 * PAGE, PAGE)]);
        assert_eq!(r, vec![run(0, 2 * PAGE), run(5 * PAGE, PAGE), run(3 * PAGE, PAGE)]);
    }

    #[test]
    fn a_span_is_the_guest_pages_in_order() {
        let fd = ram(16);
        let mut a = Arena::new(1 << 20).unwrap();
        let s = a.map(fd.as_fd(), &[run(7 * PAGE, PAGE), run(2 * PAGE, 2 * PAGE), run(9 * PAGE, PAGE)]).unwrap();
        assert_eq!(s.len, 4 * PAGE as usize);
        let seen: Vec<u8> = (0..4).map(|i| byte(s.addr + i * PAGE as usize)).collect();
        assert_eq!(seen, [7, 2, 3, 9]);
        // Writes go to the guest's pages.
        // SAFETY: inside the span.
        unsafe { std::ptr::write_volatile(s.addr as *mut u8, 0xee) };
        let mut b = [0u8; 1];
        // SAFETY: pread into a local.
        unsafe { libc::pread(fd.as_raw_fd(), b.as_mut_ptr().cast(), 1, (7 * PAGE) as i64) };
        assert_eq!(b[0], 0xee);
        assert_eq!(a.live_runs(), 3);
        a.unmap(s);
        assert_eq!(a.live_runs(), 0);
        assert_eq!(a.free_ranges(), vec![(0, 1 << 20)]);
    }

    #[test]
    fn bad_runs_are_refused_and_leave_nothing() {
        let fd = ram(4);
        let mut a = Arena::new(1 << 20).unwrap();
        for bad in [
            vec![],
            vec![run(1, PAGE)],
            vec![run(0, 100)],
            vec![run(0, 0)],
            vec![run(3 * PAGE, 2 * PAGE)],
            vec![run(u64::MAX - PAGE + 1, PAGE)],
        ] {
            assert!(a.map(fd.as_fd(), &bad).is_err(), "{bad:?}");
        }
        let too_many = vec![run(0, PAGE); MAX_RUNS + 1];
        assert!(a.map(fd.as_fd(), &too_many).is_err());
        assert!(a.map(fd.as_fd(), &[run(0, 4 * PAGE)]).is_ok());
    }

    #[test]
    fn a_freed_span_is_inaccessible_and_reused_for_guest_pages() {
        let fd = ram(8);
        let mut a = Arena::new(64 * PAGE as usize).unwrap();
        let s1 = a.map(fd.as_fd(), &[run(0, 2 * PAGE)]).unwrap();
        let s2 = a.map(fd.as_fd(), &[run(4 * PAGE, PAGE)]).unwrap();
        let addr1 = s1.addr;
        a.unmap(s1);
        // The range is a reservation again: mincore says unmapped pages are
        // ENOMEM, a PROT_NONE reservation is present in the map.
        let mut vec = [0u8; 2];
        // SAFETY: mincore on our own range.
        let rc = unsafe { libc::mincore(addr1 as *mut _, 2 * PAGE as usize, vec.as_mut_ptr()) };
        assert_eq!(rc, 0, "still reserved");
        let s3 = a.map(fd.as_fd(), &[run(6 * PAGE, PAGE)]).unwrap();
        assert_eq!(s3.addr, addr1, "first fit takes the freed range");
        assert_eq!(byte(s3.addr), 6);
        a.unmap(s2);
        a.unmap(s3);
        assert_eq!(a.free_ranges(), vec![(0, 64 * PAGE as usize)]);
    }

    #[test]
    fn the_arena_and_the_mapping_count_are_bounded() {
        let fd = ram(4);
        let mut a = Arena::new(4 * PAGE as usize).unwrap();
        let s = a.map(fd.as_fd(), &[run(0, 4 * PAGE)]).unwrap();
        assert!(a.map(fd.as_fd(), &[run(0, PAGE)]).is_err(), "full");
        a.unmap(s);
        a.live_runs = MAX_LIVE_RUNS;
        assert!(a.map(fd.as_fd(), &[run(0, PAGE)]).is_err(), "too many mappings");
    }
}
