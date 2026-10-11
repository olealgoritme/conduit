//! The hot words of a resource handle, kept in the handle's own private memory.
//!
//! A resource handle's `pDrvPrivate` points into memory the runtime allocated
//! next to its own resource object, sized by `CalcPrivate(Opened)ResourceSize`.
//! Word 0 is the [`crate::slot`] cell: the driver's boxed per-resource state.
//! Reaching the COM pointer through it is two dependent loads, and the box is
//! a separate heap block that is usually cold when a game maps or binds one of
//! its thousands of buffers. The runtime has just read its own object to find
//! `pDrvPrivate`, so words next to the cell are the cheap ones to read.
//!
//! [`ResourcePrivate`] therefore sizes the private memory for the cell plus a
//! read-only copy of the fields the per-draw paths need ([`ResourceHot`]):
//! `Map`/`Unmap`, constant/vertex/index buffer binds. The copy is written once
//! when the state is stored and cleared when it is taken; the fields it copies
//! never change in between (`RotateResourceIdentities` rotates the backing
//! storage, not the COM objects). Everything else still goes through the box.
//!
//! Platform-neutral (no `windows` types) so the layout and accessors are
//! tested on the Linux host.

use core::ffi::c_void;

/// `ResourceHot::flags`: the resource is a D3D11.1 video decoder buffer.
pub const FLAG_DECODER_BUFFER: usize = 1 << 0;

/// The copied fields. All zero for an empty handle.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResourceHot {
    /// The owning `ID3D11Resource` pointer (the box's `com_raw`; the box owns
    /// the reference, this is a copy).
    pub com_raw: usize,
    /// The same object's `ID3D11Buffer` pointer, 0 for a texture.
    pub buffer_raw: usize,
    /// `FLAG_*` bits.
    pub flags: usize,
}

/// Layout of a resource handle's private memory.
#[repr(C)]
pub struct ResourcePrivate {
    /// The slot cell (`Slot<Boxed<ResourceState>>`). Only `slot` touches it.
    pub cell: *mut c_void,
    pub hot: ResourceHot,
}

/// What `CalcPrivateResourceSize` and `CalcPrivateOpenedResourceSize` return.
pub const RESOURCE_PRIVATE_SIZE: usize = core::mem::size_of::<ResourcePrivate>();

// The cell must stay word 0: `Slot::from_priv` reads the handle's first word.
const _: () = assert!(core::mem::offset_of!(ResourcePrivate, cell) == 0);
const _: () = assert!(RESOURCE_PRIVATE_SIZE == 4 * core::mem::size_of::<usize>());

/// The hot words behind a resource handle's `pDrvPrivate`, `None` for a null
/// one.
///
/// # Safety
/// `handle_priv`, when non-null, must point at private memory of at least
/// [`RESOURCE_PRIVATE_SIZE`] bytes that this driver sized for a resource and
/// initialized with [`write`] (or that is zeroed), and no [`write`]/[`clear`]
/// of the same handle may run concurrently (the D3D11 runtime orders a
/// handle's create and destroy around every use of it).
#[inline(always)]
pub unsafe fn get(handle_priv: *mut c_void) -> Option<&'static ResourceHot> {
    if handle_priv.is_null() {
        return None;
    }
    // SAFETY: caller contract above.
    Some(unsafe { &(*handle_priv.cast::<ResourcePrivate>()).hot })
}

/// Store the hot words of a resource whose state was just put in the cell.
///
/// # Safety
/// `handle_priv` must be non-null and satisfy [`get`]'s size contract.
pub unsafe fn write(handle_priv: *mut c_void, hot: ResourceHot) {
    // SAFETY: caller contract above.
    unsafe { core::ptr::addr_of_mut!((*handle_priv.cast::<ResourcePrivate>()).hot).write(hot) }
}

/// Set `flag` in the hot words (a property learned right after the store).
///
/// # Safety
/// As [`write`].
pub unsafe fn set_flag(handle_priv: *mut c_void, flag: usize) {
    // SAFETY: caller contract above.
    unsafe { (*handle_priv.cast::<ResourcePrivate>()).hot.flags |= flag }
}

/// Zero the hot words, when the state is taken out of the cell.
///
/// # Safety
/// As [`write`].
pub unsafe fn clear(handle_priv: *mut c_void) {
    // SAFETY: caller contract above.
    unsafe { write(handle_priv, ResourceHot::default()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn private_memory() -> [usize; 4] {
        [0; 4]
    }

    #[test]
    fn size_is_four_words_and_cell_is_word_zero() {
        assert_eq!(RESOURCE_PRIVATE_SIZE, 4 * core::mem::size_of::<usize>());
        assert_eq!(core::mem::offset_of!(ResourcePrivate, cell), 0);
        assert_eq!(
            core::mem::offset_of!(ResourcePrivate, hot),
            core::mem::size_of::<usize>()
        );
    }

    #[test]
    fn null_handle_has_no_hot_words() {
        assert!(unsafe { get(core::ptr::null_mut()) }.is_none());
    }

    #[test]
    fn zeroed_memory_reads_as_empty() {
        let mut mem = private_memory();
        let p = mem.as_mut_ptr().cast::<c_void>();
        assert_eq!(unsafe { *get(p).unwrap() }, ResourceHot::default());
    }

    #[test]
    fn write_set_flag_clear_round_trip_and_leave_the_cell_alone() {
        let mut mem = private_memory();
        mem[0] = 0x1234_5670; // the slot cell
        let p = mem.as_mut_ptr().cast::<c_void>();
        let hot = ResourceHot {
            com_raw: 0xdead_bee0,
            buffer_raw: 0xdead_bee8,
            flags: 0,
        };
        unsafe { write(p, hot) };
        assert_eq!(unsafe { *get(p).unwrap() }, hot);
        assert_eq!(mem[0], 0x1234_5670);
        assert_eq!(&mem[1..], &[0xdead_bee0, 0xdead_bee8, 0]);

        unsafe { set_flag(p, FLAG_DECODER_BUFFER) };
        assert_eq!(unsafe { get(p).unwrap() }.flags, FLAG_DECODER_BUFFER);
        assert_eq!(unsafe { get(p).unwrap() }.com_raw, 0xdead_bee0);

        unsafe { clear(p) };
        assert_eq!(unsafe { *get(p).unwrap() }, ResourceHot::default());
        assert_eq!(mem[0], 0x1234_5670);
    }
}
