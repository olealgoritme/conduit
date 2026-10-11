//! Record encoding for the threaded front end: handle snapshots and packed
//! argument arrays (`guest/windows/docs/d3d11-threaded-frontend.md` §2).
//!
//! A DDI handle is a pointer to a driver-private region whose first word is
//! the object's identity (a COM or `Box` pointer). A record stores that word,
//! not the region, because the runtime may free the region as soon as the
//! app's call returns. On replay the forward function gets a pseudo-handle:
//! a pointer to the stored word. A null handle ("unbind") is stored as
//! [`NULL_WORD`] and replays as a null pointer; an empty region (word 0) stays
//! an empty region.
//!
//! Arrays go into the record's trailing bytes, each starting 8-byte aligned,
//! written by [`Pack`] and read back in the same order by [`Unpack`].
//!
//! Pure `core`: builds and tests on the Linux host.

use core::ffi::c_void;
use core::mem::{size_of, MaybeUninit};

/// The snapshot of a null handle. Identity words are aligned pointers or 0,
/// never odd.
pub const NULL_WORD: usize = 1;

/// Most handles one bind call carries (128 shader-resource slots).
pub const MAX_HANDLES: usize = 128;

/// The identity word behind a handle's `pDrvPrivate`, or [`NULL_WORD`].
///
/// # Safety
/// A non-null `p` points at a readable region starting with its word.
#[inline(always)]
pub unsafe fn snapshot_word(p: *mut c_void) -> usize {
    if p.is_null() {
        NULL_WORD
    } else {
        unsafe { *(p as *const usize) }
    }
}

/// The `pDrvPrivate` to replay a snapshot word with: null for
/// [`NULL_WORD`], else the word's own address.
///
/// # Safety
/// `w` points at a live snapshot word that outlives the replayed call.
#[inline(always)]
pub unsafe fn pseudo_handle(w: *const usize) -> *mut c_void {
    if unsafe { *w } == NULL_WORD {
        core::ptr::null_mut()
    } else {
        w as *mut c_void
    }
}

const fn round8(n: usize) -> usize {
    (n + 7) & !7
}

/// Bytes a packed array of `n` `T`s occupies.
pub const fn packed<T>(n: usize) -> usize {
    round8(n * size_of::<T>())
}

/// Writes arrays into a record's trailing bytes.
pub struct Pack<'a> {
    b: &'a mut [u8],
    o: usize,
}

impl<'a> Pack<'a> {
    pub fn new(b: &'a mut [u8]) -> Self {
        assert_eq!(b.as_ptr() as usize % 8, 0, "record extra bytes are 8-byte aligned");
        Self { b, o: 0 }
    }

    /// The snapshot words of `n` handles, given as their `pDrvPrivate`
    /// pointers (all [`NULL_WORD`] for a null array).
    ///
    /// # Safety
    /// A non-null `src` points at `n` handle pointers, each null or valid for
    /// [`snapshot_word`].
    #[inline(always)]
    pub unsafe fn handles(&mut self, src: *const *mut c_void, n: usize) {
        let need = packed::<usize>(n);
        assert!(self.o + need <= self.b.len());
        let dst = unsafe { self.b.as_mut_ptr().add(self.o) } as *mut usize;
        for i in 0..n {
            let w = if src.is_null() {
                NULL_WORD
            } else {
                unsafe { snapshot_word(*src.add(i)) }
            };
            unsafe { dst.add(i).write(w) };
        }
        self.o += need;
    }

    /// `n` plain values (zeros for a null array).
    ///
    /// # Safety
    /// A non-null `src` points at `n` readable values.
    #[inline(always)]
    pub unsafe fn copy<T: Copy>(&mut self, src: *const T, n: usize) {
        let need = packed::<T>(n);
        assert!(self.o + need <= self.b.len());
        let bytes = n * size_of::<T>();
        if !src.is_null() && bytes != 0 {
            unsafe { core::ptr::copy_nonoverlapping(src as *const u8, self.b.as_mut_ptr().add(self.o), bytes) };
        }
        self.o += need;
    }

    /// Bytes written so far.
    pub fn len(&self) -> usize {
        self.o
    }

    pub fn is_empty(&self) -> bool {
        self.o == 0
    }
}

/// Reads back what [`Pack`] wrote.
pub struct Unpack<'a> {
    b: &'a [u8],
    o: usize,
}

impl<'a> Unpack<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Self { b, o: 0 }
    }

    /// The next `n` snapshot words.
    #[inline(always)]
    pub fn words(&mut self, n: usize) -> *const usize {
        let p = self.b[self.o..].as_ptr() as *const usize;
        self.o += packed::<usize>(n);
        debug_assert!(self.o <= self.b.len());
        p
    }

    /// The next array of `n` `T`s, or null when the original pointer was null.
    #[inline(always)]
    pub fn array<T>(&mut self, n: usize, was_null: bool) -> *const T {
        let p = self.b[self.o..].as_ptr() as *const T;
        self.o += packed::<T>(n);
        debug_assert!(self.o <= self.b.len());
        if was_null {
            core::ptr::null()
        } else {
            p
        }
    }
}

/// Run `f` with `n` pseudo-handles (as `pDrvPrivate` pointers) over `n`
/// snapshot words, or with null when the original array was null.
///
/// # Safety
/// `words` points at `n` snapshot words that outlive the call; `n` is at most
/// [`MAX_HANDLES`].
#[inline(always)]
pub unsafe fn with_pseudo_handles<R>(
    words: *const usize,
    n: usize,
    was_null: bool,
    f: impl FnOnce(*const *mut c_void) -> R,
) -> R {
    if was_null {
        return f(core::ptr::null());
    }
    let mut arr: [MaybeUninit<*mut c_void>; MAX_HANDLES] = [const { MaybeUninit::uninit() }; MAX_HANDLES];
    let n = n.min(MAX_HANDLES);
    for (i, slot) in arr.iter_mut().enumerate().take(n) {
        slot.write(unsafe { pseudo_handle(words.add(i)) });
    }
    f(arr.as_ptr() as *const *mut c_void)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[repr(align(8))]
    struct Aligned([u8; 512]);

    #[test]
    fn handle_snapshots_round_trip_null_empty_and_live() {
        // Three "regions": a live identity word, an empty (failed create) one.
        let mut live: usize = 0x1234_5670;
        let mut empty: usize = 0;
        let handles: [*mut c_void; 3] = [
            &mut live as *mut usize as *mut c_void,
            core::ptr::null_mut(),
            &mut empty as *mut usize as *mut c_void,
        ];
        let mut buf = Aligned([0; 512]);
        let mut p = Pack::new(&mut buf.0);
        unsafe { p.handles(handles.as_ptr(), 3) };
        assert_eq!(p.len(), packed::<usize>(3));
        // The region may be freed and reused after the call: the record must not care.
        live = 0xdead;
        empty = 0xbeef;
        let _ = (live, empty);
        let words = Unpack::new(&buf.0).words(3);
        unsafe {
            with_pseudo_handles(words, 3, false, |hs| {
                let h0 = *hs;
                let h1 = *hs.add(1);
                let h2 = *hs.add(2);
                assert_eq!(*(h0 as *const usize), 0x1234_5670);
                assert!(h1.is_null(), "null handle replays as null");
                assert!(!h2.is_null(), "empty region replays as a region");
                assert_eq!(*(h2 as *const usize), 0);
            });
        }
    }

    #[test]
    fn null_arrays_replay_as_null() {
        let mut buf = Aligned([0xff; 512]);
        let mut p = Pack::new(&mut buf.0);
        unsafe {
            p.handles(core::ptr::null(), 4);
            p.copy(core::ptr::null::<u32>(), 4);
        }
        let mut u = Unpack::new(&buf.0);
        let words = u.words(4);
        for i in 0..4 {
            assert_eq!(unsafe { *words.add(i) }, NULL_WORD);
        }
        assert!(u.array::<u32>(4, true).is_null());
        unsafe { with_pseudo_handles(words, 4, true, |hs| assert!(hs.is_null())) };
    }

    #[test]
    fn mixed_arrays_keep_order_and_alignment() {
        #[derive(Clone, Copy, PartialEq, Debug)]
        struct Viewport {
            x: f32,
            y: f32,
            w: f32,
            h: f32,
            min: f32,
            max: f32,
        }
        let mut a: usize = 0x1000;
        let handles = [&mut a as *mut usize as *mut c_void];
        let strides = [12u32, 16, 20];
        let vps = [Viewport { x: 1.0, y: 2.0, w: 3.0, h: 4.0, min: 0.0, max: 1.0 }];
        let mut buf = Aligned([0; 512]);
        let mut p = Pack::new(&mut buf.0);
        unsafe {
            p.handles(handles.as_ptr(), 1);
            p.copy(strides.as_ptr(), 3); // 12 bytes -> 16 with padding
            p.copy(vps.as_ptr(), 1);
        }
        assert_eq!(p.len(), 8 + 16 + 24);
        let mut u = Unpack::new(&buf.0);
        let w = u.words(1);
        let s = u.array::<u32>(3, false);
        let v = u.array::<Viewport>(1, false);
        unsafe {
            assert_eq!(*w, 0x1000);
            assert_eq!(core::slice::from_raw_parts(s, 3), &strides);
            assert_eq!(*v, vps[0]);
            assert_eq!(s as usize % 8, 0);
            assert_eq!(v as usize % 8, 0);
        }
    }

    #[test]
    fn packed_sizes_are_eight_byte_multiples() {
        assert_eq!(packed::<u32>(0), 0);
        assert_eq!(packed::<u32>(1), 8);
        assert_eq!(packed::<u32>(2), 8);
        assert_eq!(packed::<u32>(3), 16);
        assert_eq!(packed::<usize>(MAX_HANDLES), MAX_HANDLES * size_of::<usize>());
    }
}
