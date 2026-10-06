#![no_std]
pub struct WdkAllocator;
unsafe impl core::alloc::GlobalAlloc for WdkAllocator {
    unsafe fn alloc(&self, _l: core::alloc::Layout) -> *mut u8 { core::ptr::null_mut() }
    unsafe fn dealloc(&self, _p: *mut u8, _l: core::alloc::Layout) {}
}
