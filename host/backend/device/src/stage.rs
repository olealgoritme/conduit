//! Frame stage stamps (docs/TRACING.md "Frame stage timing"): the backend's
//! stamp sites, over `conduit_venus::stage`. Stage timing follows Windows
//! guests' frames, which need Venus; without the `venus` feature every
//! function here is empty and [`on`] is a constant false.

#[cfg(feature = "venus")]
pub use conduit_venus::stage::{
    H_DECODED, H_DISPLAY, H_IRQ, H_KICK, H_USED, Rec, dump, init_from_env, kick, last_kick, now_ns,
    on, set_on, stamp,
};

/// Tests that turn stamping on and drain the process-wide ring take this,
/// so one does not drain another's stamps.
#[cfg(all(test, feature = "venus"))]
pub static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Stamp a flip stage for `seq` now, if stamping is on.
#[cfg(feature = "venus")]
#[inline]
pub fn flip(stage: u8, seq: u64) {
    if on() {
        stamp(Rec::flip(stage, seq, now_ns()));
    }
}

/// A flip's decode: the kick that brought it, then now.
#[cfg(feature = "venus")]
#[inline]
pub fn flip_decoded(seq: u64) {
    if on() {
        let kick = last_kick();
        if kick != 0 {
            stamp(Rec::flip(H_KICK, seq, kick));
        }
        stamp(Rec::flip(H_DECODED, seq, now_ns()));
    }
}

#[cfg(not(feature = "venus"))]
mod off {
    pub const H_DISPLAY: u8 = 39;
    pub const H_IRQ: u8 = 38;
    pub const H_USED: u8 = 37;
    #[inline(always)]
    pub fn on() -> bool {
        false
    }
    #[inline(always)]
    pub fn kick() {}
    #[inline(always)]
    pub fn flip(_stage: u8, _seq: u64) {}
    #[inline(always)]
    pub fn flip_decoded(_seq: u64) {}
    pub fn init_from_env() -> bool {
        false
    }
}
#[cfg(not(feature = "venus"))]
pub use off::*;
