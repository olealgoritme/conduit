//! The service-key knobs whose ABSENT value is the shipped configuration, not the pre-feature
//! behaviour. A fresh install has none of these values in
//! `HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd_render`; what the driver does then is
//! decided here, and every reader in `kmd_render` (and the UMD's mirror of `RedirVram`) takes its
//! default from this module. An explicit value, 0 included, still wins.
//!
//! Why these values: the release installer (`packaging/windows/install.ps1`) puts DWM on NVK
//! (`HKLM\SOFTWARE\Helios!DwmIcd=nvk`) unless `-VenusDesktop` is given. The NVK DWM's swap-chain
//! buffers are foreign (NVK-on-RM) allocations, and only the configuration below shows and copies
//! them correctly:
//!
//! * `ForeignFlip` 0 leaves the NVK DWM's primaries on the `SET_SCANOUT_BLOB` path, which scans
//!   out the KMD's own LINEAR backing of the allocation instead of the block-linear RM memory NVK
//!   rendered into: the host gets an ARGB8888 LINEAR buffer with alpha 0 and the viewer shows
//!   black, while a guest screenshot (taken through DWM) looks normal.
//! * `ForeignCopy` 0 makes the KMD skip (`FcOff`) every Present Blt whose source is a foreign
//!   resource: windowed NVK content (the shell follows DWM onto NVK) is not copied into its
//!   window surface.
//! * `BltAsync` 0 makes each such Blt a CPU wait in `DxgkDdiPresent`.
//! * `RedirVram` 0 leaves CDD's CPU-written GDI surfaces out of the aperture segment and the UMD
//!   copying windowed NVK frames itself (`NvkSkipBltCopy` follows `RedirVram`).
//! * `MsiMode` 0 follows the INF's `MSISupported` = 0 (INTx). 2 asks PnP for MSI-X at the next
//!   start; the boot-loop breaker and the delivery latch still put INTx back on a failure.
//!
//! On a Venus desktop (`-VenusDesktop`) the foreign arms see no foreign allocation and do nothing.

/// `ForeignFlip` when the service key has none: the KMD flips the NVK DWM's buffers itself.
pub const FOREIGN_FLIP: u32 = 1;
/// `ForeignCopy` when the service key has none: the explicit-modifier copy of a foreign source.
pub const FOREIGN_COPY: u32 = 1;
/// `BltAsync` when the service key has none: the composed foreign Blt does not wait on the CPU.
pub const BLT_ASYNC: u32 = 1;
/// `RedirVram` when the service key has none ([`crate::rm_vidmem::KNOB_ON`]).
pub const REDIR_VRAM: u32 = crate::rm_vidmem::KNOB_ON;
/// `MsiMode` when the service key has none ([`crate::msi::Mode::ForceMsi`]'s code).
pub const MSI_MODE: u32 = 2;

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;

    #[test]
    fn the_shipped_defaults_are_the_proven_configuration() {
        assert_eq!(FOREIGN_FLIP, 1);
        assert_eq!(FOREIGN_COPY, 1);
        assert_eq!(BLT_ASYNC, 1);
        assert!(crate::rm_vidmem::knob_on(REDIR_VRAM));
        assert_eq!(crate::msi::Mode::from_knob(MSI_MODE), crate::msi::Mode::ForceMsi);
    }

    #[test]
    fn foreign_flip_on_by_default_turns_its_dependents_on() {
        // `FlipAnnForeign` and `FfAsyncWin` derive their own defaults from `ForeignFlip`.
        assert_eq!(crate::flip_retire::ann_foreign_default(FOREIGN_FLIP), 1);
    }

    #[test]
    fn every_reader_takes_its_default_from_here() {
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let render = manifest.join("../kmd_render/src");
        if !render.exists() {
            return;
        }
        let read = |p: &str| std::fs::read_to_string(render.join(p)).unwrap();
        let ff = read("virtio/foreign_flip.rs");
        assert!(ff.contains("sd::FOREIGN_FLIP"), "foreign_flip.rs");
        let ann = read("ddi/flip_announce.rs");
        assert!(ann.contains("shipping_defaults::FOREIGN_FLIP"), "flip_announce.rs");
        let adapter = read("adapter/mod.rs");
        assert!(adapter.contains("knobs::FOREIGN_COPY, sd::FOREIGN_COPY"), "adapter FOREIGN_COPY");
        assert!(adapter.contains("knobs::REDIR_VRAM, sd::REDIR_VRAM"), "adapter REDIR_VRAM");
        let ba = read("ddi/blt_async.rs");
        assert!(ba.contains("sd::BLT_ASYNC"), "blt_async.rs");
        let vm = read("virtio/rm_client/vidmem.rs");
        assert!(vm.contains("knobs::REDIR_VRAM, sd::REDIR_VRAM"), "vidmem.rs");
        let msi = read("virtio/msi.rs");
        assert!(msi.contains("knobs::MSI_MODE, sd::MSI_MODE"), "msi.rs");
    }
}
