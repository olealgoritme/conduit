//! A thin, antialiased drawing layer over the GDI+ flat API.

use std::ptr::{null, null_mut};
use windows_sys::Win32::Graphics::Gdi::HDC;
use windows_sys::Win32::Graphics::GdiPlus::*;

pub type Rgb = (u8, u8, u8);

/// 0xAARRGGBB.
pub fn argb(a: u8, c: Rgb) -> u32 {
    (a as u32) << 24 | (c.0 as u32) << 16 | (c.1 as u32) << 8 | c.2 as u32
}

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Starts GDI+ for the life of the process.
pub fn startup() -> usize {
    let mut token = 0usize;
    let input = GdiplusStartupInput {
        GdiplusVersion: 1,
        DebugEventCallback: 0,
        SuppressBackgroundThread: 0,
        SuppressExternalCodecs: 1,
    };
    unsafe { GdiplusStartup(&mut token, &input, null_mut()) };
    token
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
}

pub struct Gfx {
    g: *mut GpGraphics,
}

impl Gfx {
    fn setup(g: *mut GpGraphics) -> Option<Gfx> {
        if g.is_null() {
            return None;
        }
        unsafe {
            GdipSetSmoothingMode(g, 4); // anti-alias
            GdipSetPixelOffsetMode(g, 2); // high quality
            GdipSetTextRenderingHint(g, 3); // anti-alias, grid fit
        }
        Some(Gfx { g })
    }

    pub fn from_hdc(hdc: HDC) -> Option<Gfx> {
        let mut g = null_mut();
        unsafe { GdipCreateFromHDC(hdc, &mut g) };
        Gfx::setup(g)
    }

    /// Everything drawn afterwards is in units of `s` pixels.
    pub fn scale(&self, s: f32) {
        unsafe { GdipScaleWorldTransform(self.g, s, s, 0) };
    }

    pub fn clear(&self, color: u32) {
        unsafe { GdipGraphicsClear(self.g, color) };
    }

    fn rrect(&self, x: f32, y: f32, w: f32, h: f32, r: f32) -> *mut GpPath {
        let mut p = null_mut();
        let r = r.min(w / 2.0).min(h / 2.0).max(0.01);
        let d = r * 2.0;
        unsafe {
            GdipCreatePath(0, &mut p);
            GdipAddPathArc(p, x, y, d, d, 180.0, 90.0);
            GdipAddPathArc(p, x + w - d, y, d, d, 270.0, 90.0);
            GdipAddPathArc(p, x + w - d, y + h - d, d, d, 0.0, 90.0);
            GdipAddPathArc(p, x, y + h - d, d, d, 90.0, 90.0);
            GdipClosePathFigure(p);
        }
        p
    }

    pub fn fill_rrect(&self, color: u32, x: f32, y: f32, w: f32, h: f32, r: f32) {
        let p = self.rrect(x, y, w, h, r);
        let mut b = null_mut();
        unsafe {
            GdipCreateSolidFill(color, &mut b);
            GdipFillPath(self.g, b as *mut GpBrush, p);
            GdipDeleteBrush(b as *mut GpBrush);
            GdipDeletePath(p);
        }
    }

    /// Left-to-right gradient fill.
    pub fn fill_rrect_grad(&self, c1: u32, c2: u32, x: f32, y: f32, w: f32, h: f32, r: f32) {
        let p = self.rrect(x, y, w, h, r);
        let (a, b) = (
            PointF { X: x, Y: y },
            PointF {
                X: x + w.max(1.0),
                Y: y,
            },
        );
        let mut br = null_mut();
        unsafe {
            GdipCreateLineBrush(&a, &b, c1, c2, 0, &mut br);
            GdipFillPath(self.g, br as *mut GpBrush, p);
            GdipDeleteBrush(br as *mut GpBrush);
            GdipDeletePath(p);
        }
    }

    pub fn stroke_rrect(&self, color: u32, width: f32, x: f32, y: f32, w: f32, h: f32, r: f32) {
        let p = self.rrect(x, y, w, h, r);
        let mut pen = null_mut();
        unsafe {
            GdipCreatePen1(color, width, 2, &mut pen);
            GdipDrawPath(self.g, pen, p);
            GdipDeletePen(pen);
            GdipDeletePath(p);
        }
    }

    pub fn fill_circle(&self, color: u32, cx: f32, cy: f32, r: f32) {
        let mut b = null_mut();
        unsafe {
            GdipCreateSolidFill(color, &mut b);
            GdipFillEllipse(self.g, b as *mut GpBrush, cx - r, cy - r, r * 2.0, r * 2.0);
            GdipDeleteBrush(b as *mut GpBrush);
        }
    }

    pub fn line(&self, color: u32, width: f32, x1: f32, y1: f32, x2: f32, y2: f32) {
        let mut pen = null_mut();
        unsafe {
            GdipCreatePen1(color, width, 2, &mut pen);
            GdipDrawLine(self.g, pen, x1, y1, x2, y2);
            GdipDeletePen(pen);
        }
    }

    pub fn polyline(&self, color: u32, width: f32, pts: &[(f32, f32)]) {
        if pts.len() < 2 {
            return;
        }
        let v: Vec<PointF> = pts.iter().map(|&(x, y)| PointF { X: x, Y: y }).collect();
        let mut pen = null_mut();
        unsafe {
            GdipCreatePen1(color, width, 2, &mut pen);
            GdipSetPenLineJoin(pen, 2); // round
            GdipDrawLines(self.g, pen, v.as_ptr(), v.len() as i32);
            GdipDeletePen(pen);
        }
    }

    /// `pts` closed, filled with a top-to-bottom gradient between `top` and `bottom`.
    pub fn fill_poly_vertical(&self, top: u32, bottom: u32, y0: f32, y1: f32, pts: &[(f32, f32)]) {
        if pts.len() < 3 {
            return;
        }
        let v: Vec<PointF> = pts.iter().map(|&(x, y)| PointF { X: x, Y: y }).collect();
        let (a, b) = (
            PointF { X: 0.0, Y: y0 },
            PointF {
                X: 0.0,
                Y: y1.max(y0 + 1.0),
            },
        );
        let mut br = null_mut();
        unsafe {
            GdipCreateLineBrush(&a, &b, top, bottom, 0, &mut br);
            GdipFillPolygon(self.g, br as *mut GpBrush, v.as_ptr(), v.len() as i32, 0);
            GdipDeleteBrush(br as *mut GpBrush);
        }
    }

    fn font(&self, px: f32, bold: bool) -> (*mut GpFontFamily, *mut GpFont) {
        let name = wide("Segoe UI");
        let (mut fam, mut font) = (null_mut(), null_mut());
        unsafe {
            GdipCreateFontFamilyFromName(name.as_ptr(), null_mut(), &mut fam);
            if fam.is_null() {
                let generic = wide("Arial");
                GdipCreateFontFamilyFromName(generic.as_ptr(), null_mut(), &mut fam);
            }
            GdipCreateFont(fam, px, bold as i32, 2, &mut font);
        }
        (fam, font)
    }

    fn format(&self, align: Align, vcenter: bool) -> *mut GpStringFormat {
        let mut f = null_mut();
        unsafe {
            // NoWrap | NoClip
            GdipCreateStringFormat(0x1000 | 0x4000, 0, &mut f);
            GdipSetStringFormatAlign(
                f,
                match align {
                    Align::Left => 0,
                    Align::Center => 1,
                    Align::Right => 2,
                },
            );
            GdipSetStringFormatLineAlign(f, if vcenter { 1 } else { 0 });
        }
        f
    }

    /// Draw `text` in the box (x, y, w, h); the box's top is the text's top
    /// unless `vcenter`.
    #[allow(clippy::too_many_arguments)]
    pub fn text(
        &self,
        text: &str,
        color: u32,
        px: f32,
        bold: bool,
        align: Align,
        vcenter: bool,
        (x, y, w, h): (f32, f32, f32, f32),
    ) {
        let t = wide(text);
        let (fam, font) = self.font(px, bold);
        let fmt = self.format(align, vcenter);
        let rect = RectF {
            X: x,
            Y: y,
            Width: w,
            Height: h,
        };
        let mut b = null_mut();
        unsafe {
            GdipCreateSolidFill(color, &mut b);
            GdipDrawString(
                self.g,
                t.as_ptr(),
                (t.len() - 1) as i32,
                font,
                &rect,
                fmt,
                b as *const GpBrush,
            );
            GdipDeleteBrush(b as *mut GpBrush);
            GdipDeleteStringFormat(fmt);
            GdipDeleteFont(font);
            GdipDeleteFontFamily(fam);
        }
    }

    /// The width `text` takes.
    pub fn text_width(&self, text: &str, px: f32, bold: bool) -> f32 {
        let t = wide(text);
        let (fam, font) = self.font(px, bold);
        let fmt = self.format(Align::Left, false);
        let rect = RectF {
            X: 0.0,
            Y: 0.0,
            Width: 10000.0,
            Height: 1000.0,
        };
        let mut bounds = RectF {
            X: 0.0,
            Y: 0.0,
            Width: 0.0,
            Height: 0.0,
        };
        unsafe {
            GdipMeasureString(
                self.g,
                t.as_ptr(),
                (t.len() - 1) as i32,
                font,
                &rect,
                fmt,
                &mut bounds,
                null_mut(),
                null_mut(),
            );
            GdipDeleteStringFormat(fmt);
            GdipDeleteFont(font);
            GdipDeleteFontFamily(fam);
        }
        bounds.Width
    }
}

impl Drop for Gfx {
    fn drop(&mut self) {
        unsafe { GdipDeleteGraphics(self.g) };
    }
}

/// An icon of `size` x `size` pixels drawn with `draw`, with real alpha.
pub fn make_icon(size: i32, draw: impl FnOnce(&Gfx)) -> Option<isize> {
    let mut bmp: *mut GpBitmap = null_mut();
    let mut hicon = 0isize;
    unsafe {
        // PixelFormat32bppPARGB
        GdipCreateBitmapFromScan0(size, size, 0, 0x000E_200B, null(), &mut bmp);
        if bmp.is_null() {
            return None;
        }
        let mut g = null_mut();
        GdipGetImageGraphicsContext(bmp as *mut GpImage, &mut g);
        if let Some(gfx) = Gfx::setup(g) {
            gfx.clear(0);
            draw(&gfx);
            drop(gfx);
            GdipCreateHICONFromBitmap(bmp, &mut hicon as *mut isize as *mut _);
        }
        GdipDisposeImage(bmp as *mut GpImage);
    }
    (hicon != 0).then_some(hicon)
}
