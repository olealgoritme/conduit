//! Where an NVK swap chain's frame goes: DWM composition or scanout 0.
//!
//! The D3D12 driver (`umd12/src/forward12/present12.rs`, `Nvk12Present`) asks
//! [`route`] once per present. Pure and portable, so the policy is tested on
//! the Linux host.
//!
//! The two ways an NVK back buffer reaches the screen:
//!
//! * **Composed**: the WDDM present names the back buffer's allocation, which
//!   adopted the KMD resource id NVK minted for its memory. DWM opens that id
//!   (a DWM on NVK natively, a Venus DWM with `ForeignImport=1`) and composes
//!   the window like any other.
//! * **Scanout 0**: the frame is shown zero-copy through the KMD's foreign
//!   scanout source; the desktop's own flushes are withheld while the source
//!   is live. Correct only when the frame covers the whole output: a
//!   window-sized frame is stretched over the whole screen, and DWM's flips
//!   show through between the app's frames.

/// Where one present goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// The WDDM present; DWM composes the back buffer by resource id.
    Composed,
    /// NVK's scanout 0 (zero-copy flip of the whole output).
    Scanout,
}

/// Why [`route`] chose what it chose (the log line says it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// The knob forced scanout 0 (`1`).
    ForcedScanout,
    /// The knob forced the WDDM present (`2`).
    ForcedComposed,
    /// The back buffer has no KMD resource id: nothing outside this process
    /// can read it, so only scanout 0 shows it.
    NoResourceId,
    /// No DWM that imports NVK surfaces (no DWM on NVK, no `ForeignImport`).
    DwmCannotImport,
    /// The back buffer covers the whole output (a primary, or at least the
    /// desktop's size): scanout 0 shows exactly what composition would.
    CoversOutput,
    /// A window: DWM composes it.
    Window,
}

impl Reason {
    /// Short text for the log line.
    pub const fn as_str(self) -> &'static str {
        match self {
            Reason::ForcedScanout => "forced scanout (1)",
            Reason::ForcedComposed => "forced WDDM present (2)",
            Reason::NoResourceId => "no KMD resource id",
            Reason::DwmCannotImport => "DWM does not import NVK surfaces",
            Reason::CoversOutput => "covers the output",
            Reason::Window => "a window",
        }
    }
}

/// What [`route`] decides on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Inputs {
    /// The present-mode knob: 0 automatic, 1 scanout 0, 2 WDDM present.
    /// Values above 2 count as 2.
    pub mode: u32,
    /// The back buffer's allocation adopted a KMD resource id.
    pub has_resource_id: bool,
    /// DWM imports NVK surfaces by resource id: a DWM on NVK (its marker
    /// event) or a Venus DWM with `ForeignImport=1`.
    pub dwm_imports: bool,
    /// The back buffer covers the whole output ([`covers_output`]).
    pub covers_output: bool,
}

/// The route of one present, and why.
///
/// Automatic (mode 0): composed when DWM can import the buffer and the buffer
/// is a window; scanout 0 when it covers the output (zero-copy, nothing to
/// compose around it), when DWM cannot import NVK surfaces, or when the buffer
/// has no resource id.
pub const fn route(i: Inputs) -> (Route, Reason) {
    match i.mode {
        1 => (Route::Scanout, Reason::ForcedScanout),
        0 => {
            if !i.has_resource_id {
                (Route::Scanout, Reason::NoResourceId)
            } else if !i.dwm_imports {
                (Route::Scanout, Reason::DwmCannotImport)
            } else if i.covers_output {
                (Route::Scanout, Reason::CoversOutput)
            } else {
                (Route::Composed, Reason::Window)
            }
        }
        _ => (Route::Composed, Reason::ForcedComposed),
    }
}

/// Does a `width` x `height` back buffer cover the whole output?
///
/// `primary`: the runtime declared it a primary (D3D12 `HEAP_FLAG_PRIMARY`,
/// fullscreen exclusive). `desktop`: the desktop's size in physical pixels, or
/// `None` when it could not be read; then only a primary counts, so an unknown
/// size never sends a window to scanout 0.
pub const fn covers_output(
    width: u64,
    height: u32,
    primary: bool,
    desktop: Option<(u32, u32)>,
) -> bool {
    if primary {
        return true;
    }
    match desktop {
        Some((w, h)) if w != 0 && h != 0 => width >= w as u64 && height >= h,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auto(has_resource_id: bool, dwm_imports: bool, covers_output: bool) -> (Route, Reason) {
        route(Inputs {
            mode: 0,
            has_resource_id,
            dwm_imports,
            covers_output,
        })
    }

    #[test]
    fn a_window_under_a_dwm_that_imports_is_composed() {
        assert_eq!(auto(true, true, false), (Route::Composed, Reason::Window));
    }

    #[test]
    fn a_buffer_covering_the_output_stays_on_scanout() {
        assert_eq!(
            auto(true, true, true),
            (Route::Scanout, Reason::CoversOutput)
        );
    }

    #[test]
    fn without_an_importing_dwm_every_buffer_takes_scanout() {
        assert_eq!(
            auto(true, false, false),
            (Route::Scanout, Reason::DwmCannotImport)
        );
        assert_eq!(
            auto(true, false, true),
            (Route::Scanout, Reason::DwmCannotImport)
        );
    }

    #[test]
    fn a_buffer_without_a_resource_id_takes_scanout() {
        assert_eq!(
            auto(false, true, false),
            (Route::Scanout, Reason::NoResourceId)
        );
    }

    #[test]
    fn the_knob_overrides_everything() {
        for (has_id, dwm, covers) in [
            (true, true, false),
            (false, false, true),
            (true, true, true),
        ] {
            let forced = |mode| {
                route(Inputs {
                    mode,
                    has_resource_id: has_id,
                    dwm_imports: dwm,
                    covers_output: covers,
                })
            };
            assert_eq!(forced(1), (Route::Scanout, Reason::ForcedScanout));
            assert_eq!(forced(2), (Route::Composed, Reason::ForcedComposed));
            assert_eq!(forced(7), (Route::Composed, Reason::ForcedComposed));
        }
    }

    #[test]
    fn covers_output_needs_the_desktop_size_or_a_primary() {
        assert!(covers_output(5120, 1440, false, Some((5120, 1440))));
        assert!(covers_output(3840, 2160, false, Some((1920, 1080))));
        assert!(!covers_output(1920, 1080, false, Some((5120, 1440))));
        assert!(!covers_output(5120, 1000, false, Some((5120, 1440))));
        assert!(!covers_output(640, 480, false, None));
        assert!(!covers_output(5120, 1440, false, Some((0, 0))));
        assert!(covers_output(640, 480, true, None));
        assert!(covers_output(640, 480, true, Some((5120, 1440))));
    }
}
