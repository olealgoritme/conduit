//! Safe wrapper over csrc/gpu.h. Every type here is `!Send`: the EGL context
//! is current on the thread that opened it, so the whole GPU side lives on
//! one thread (the pipeline thread, see pipeline.rs).

use anyhow::{anyhow, Result};
use std::ffi::{c_char, c_int, CStr, CString};
use std::marker::PhantomData;
use std::os::fd::RawFd;

#[repr(C)]
struct CsGpu {
    _p: [u8; 0],
}
#[repr(C)]
struct CsEnc {
    _p: [u8; 0],
}
#[repr(C)]
struct CsDec {
    _p: [u8; 0],
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Codec {
    H264 = 0,
    Hevc = 1,
    Av1 = 2,
}

impl Codec {
    pub fn name(self) -> &'static str {
        match self {
            Codec::H264 => "H.264",
            Codec::Hevc => "HEVC",
            Codec::Av1 => "AV1",
        }
    }
    pub fn from_u32(v: u32) -> Option<Self> {
        Some(match v {
            0 => Codec::H264,
            1 => Codec::Hevc,
            2 => Codec::Av1,
            _ => return None,
        })
    }
}

impl std::str::FromStr for Codec {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "h264" | "h.264" | "avc" => Ok(Codec::H264),
            "hevc" | "h265" | "h.265" => Ok(Codec::Hevc),
            "av1" => Ok(Codec::Av1),
            _ => Err(format!("unknown codec {s:?} (h264, hevc or av1)")),
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EncParams {
    pub codec: u32,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub chroma444: u32,
    pub lossless: u32,
    pub colorspace: u32,
    pub full_range: u32,
    pub preset: u32,
    pub max_ref_frames: u32,
    pub slices: u32,
    pub intra_refresh: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FrameOpts {
    pub render: i32,
    pub force_idr: i32,
    pub cursor_on: i32,
    pub cursor_x: i32,
    pub cursor_y: i32,
}

#[repr(C)]
struct CsPacket {
    data: *const u8,
    len: usize,
    frame_index: u64,
    idr: i32,
    encode_us: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct DecParams {
    pub codec: u32,
    pub width: u32,
    pub height: u32,
    pub chroma444: u32,
    pub lossless: u32,
    pub colorspace: u32,
    pub full_range: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct DecFrame {
    pub fd: c_int,
    pub id: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub offset: u32,
    pub fourcc: u32,
    pub modifier: u64,
}

extern "C" {
    fn cs_gpu_open(node: *const c_char, err: *mut c_char, errlen: usize) -> *mut CsGpu;
    fn cs_gpu_close(g: *mut CsGpu);
    fn cs_gpu_modifiers(g: *mut CsGpu, fourcc: u32, out: *mut u64, max: usize) -> usize;
    fn cs_gpu_codec_caps(g: *mut CsGpu, codec: c_int) -> u32;
    fn cs_gpu_set_frame(
        g: *mut CsGpu,
        fd: c_int,
        id: u64,
        w: u32,
        h: u32,
        stride: u32,
        offset: u32,
        fourcc: u32,
        modifier: u64,
        err: *mut c_char,
        errlen: usize,
    ) -> c_int;
    fn cs_gpu_frame_size(g: *mut CsGpu, w: *mut u32, h: *mut u32);
    fn cs_gpu_set_cursor(
        g: *mut CsGpu,
        fd: c_int,
        id: u64,
        w: u32,
        h: u32,
        stride: u32,
        offset: u32,
        fourcc: u32,
        modifier: u64,
        hot_x: u32,
        hot_y: u32,
        err: *mut c_char,
        errlen: usize,
    ) -> c_int;
    fn cs_enc_open(
        g: *mut CsGpu,
        p: *const EncParams,
        err: *mut c_char,
        errlen: usize,
    ) -> *mut CsEnc;
    fn cs_enc_encode(
        e: *mut CsEnc,
        o: *const FrameOpts,
        out: *mut CsPacket,
        err: *mut c_char,
        errlen: usize,
    ) -> c_int;
    fn cs_enc_invalidate(e: *mut CsEnc, first: u64, last: u64) -> c_int;
    fn cs_enc_close(e: *mut CsEnc);
    fn cs_dec_open(
        g: *mut CsGpu,
        p: *const DecParams,
        mods: *const u64,
        n: usize,
        err: *mut c_char,
        errlen: usize,
    ) -> *mut CsDec;
    fn cs_dec_decode(
        d: *mut CsDec,
        data: *const u8,
        len: usize,
        out: *mut DecFrame,
        err: *mut c_char,
        errlen: usize,
    ) -> c_int;
    fn cs_dec_close(d: *mut CsDec);
}

struct ErrBuf([c_char; 512]);
impl ErrBuf {
    fn new() -> Self {
        ErrBuf([0; 512])
    }
    fn ptr(&mut self) -> *mut c_char {
        self.0.as_mut_ptr()
    }
    fn get(&self) -> String {
        // SAFETY: the C side always NUL-terminates (vsnprintf), and the buffer
        // starts zeroed.
        unsafe { CStr::from_ptr(self.0.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    }
}

/// What NVENC can do for one codec (cs_gpu_codec_caps).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CodecCaps {
    pub supported: bool,
    pub yuv444: bool,
    pub lossless: bool,
    pub rfi: bool,
    pub ten_bit: bool,
    pub max_width: u32,
}

impl CodecCaps {
    pub fn from_bits(v: u32) -> Self {
        CodecCaps {
            supported: v & 1 != 0,
            yuv444: v & 2 != 0,
            lossless: v & 4 != 0,
            rfi: v & 8 != 0,
            ten_bit: v & 16 != 0,
            max_width: (v >> 16) * 16,
        }
    }
}

pub struct Gpu {
    g: *mut CsGpu,
    _not_send: PhantomData<*mut ()>,
}

/// A dma-buf plane as the broker protocol describes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BufDesc {
    pub id: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub offset: u32,
    pub fourcc: u32,
    pub modifier: u64,
}

impl Gpu {
    pub fn open(render_node: Option<&str>) -> Result<Gpu> {
        let node = render_node.map(|s| CString::new(s).unwrap());
        let mut e = ErrBuf::new();
        // SAFETY: plain FFI; the pointer is owned by the returned Gpu.
        let g = unsafe {
            cs_gpu_open(
                node.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
                e.ptr(),
                512,
            )
        };
        if g.is_null() {
            return Err(anyhow!("GPU: {}", e.get()));
        }
        Ok(Gpu {
            g,
            _not_send: PhantomData,
        })
    }

    /// Modifiers EGL imports for `fourcc` (LINEAR included if supported).
    pub fn modifiers(&self, fourcc: u32) -> Vec<u64> {
        let mut v = vec![0u64; 128];
        // SAFETY: valid handle, buffer of 128.
        let n = unsafe { cs_gpu_modifiers(self.g, fourcc, v.as_mut_ptr(), v.len()) };
        v.truncate(n);
        v
    }

    pub fn codec_caps(&self, c: Codec) -> CodecCaps {
        // SAFETY: valid handle.
        CodecCaps::from_bits(unsafe { cs_gpu_codec_caps(self.g, c as c_int) })
    }

    pub fn set_frame(&self, fd: RawFd, d: &BufDesc) -> Result<()> {
        let mut e = ErrBuf::new();
        // SAFETY: fd is a live descriptor for the duration of the call; the C
        // side does not keep it (EGL dups what it needs).
        let r = unsafe {
            cs_gpu_set_frame(
                self.g,
                fd,
                d.id,
                d.width,
                d.height,
                d.stride,
                d.offset,
                d.fourcc,
                d.modifier,
                e.ptr(),
                512,
            )
        };
        if r != 0 {
            return Err(anyhow!("{}", e.get()));
        }
        Ok(())
    }

    pub fn frame_size(&self) -> (u32, u32) {
        let (mut w, mut h) = (0, 0);
        // SAFETY: valid handle and out-pointers.
        unsafe { cs_gpu_frame_size(self.g, &mut w, &mut h) };
        (w, h)
    }

    /// `None` hides the cursor.
    pub fn set_cursor(&self, c: Option<(RawFd, BufDesc, u32, u32)>) -> Result<()> {
        let mut e = ErrBuf::new();
        let r = match c {
            // SAFETY: as set_frame.
            Some((fd, d, hx, hy)) => unsafe {
                cs_gpu_set_cursor(
                    self.g,
                    fd,
                    d.id,
                    d.width,
                    d.height,
                    d.stride,
                    d.offset,
                    d.fourcc,
                    d.modifier,
                    hx,
                    hy,
                    e.ptr(),
                    512,
                )
            },
            // SAFETY: fd -1 = hide.
            None => unsafe {
                cs_gpu_set_cursor(self.g, -1, 0, 0, 0, 0, 0, 0, 0, 0, 0, e.ptr(), 512)
            },
        };
        if r != 0 {
            return Err(anyhow!("{}", e.get()));
        }
        Ok(())
    }

    pub fn encoder(&self, p: &EncParams) -> Result<Encoder<'_>> {
        let mut e = ErrBuf::new();
        // SAFETY: p outlives the call; the encoder borrows the Gpu.
        let enc = unsafe { cs_enc_open(self.g, p, e.ptr(), 512) };
        if enc.is_null() {
            return Err(anyhow!("encoder: {}", e.get()));
        }
        Ok(Encoder {
            e: enc,
            params: *p,
            _gpu: PhantomData,
        })
    }

    pub fn decoder(&self, p: &DecParams, modifiers: &[u64]) -> Result<Decoder<'_>> {
        let mut e = ErrBuf::new();
        // SAFETY: slices outlive the call.
        let d =
            unsafe { cs_dec_open(self.g, p, modifiers.as_ptr(), modifiers.len(), e.ptr(), 512) };
        if d.is_null() {
            return Err(anyhow!("decoder: {}", e.get()));
        }
        Ok(Decoder {
            d,
            _gpu: PhantomData,
        })
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        // SAFETY: owned handle, closed once.
        unsafe { cs_gpu_close(self.g) }
    }
}

pub struct Packet<'a> {
    pub data: &'a [u8],
    pub frame_index: u64,
    pub idr: bool,
    pub encode_us: u32,
}

pub struct Encoder<'g> {
    e: *mut CsEnc,
    pub params: EncParams,
    _gpu: PhantomData<&'g Gpu>,
}

impl Encoder<'_> {
    pub fn encode(&mut self, o: &FrameOpts) -> Result<Packet<'_>> {
        let mut e = ErrBuf::new();
        let mut p = CsPacket {
            data: std::ptr::null(),
            len: 0,
            frame_index: 0,
            idr: 0,
            encode_us: 0,
        };
        // SAFETY: valid encoder; the packet data stays valid until the next
        // call, which the &mut borrow on the returned Packet enforces.
        let r = unsafe { cs_enc_encode(self.e, o, &mut p, e.ptr(), 512) };
        if r != 0 {
            return Err(anyhow!("{}", e.get()));
        }
        Ok(Packet {
            // SAFETY: data/len describe the encoder's own output buffer.
            data: unsafe { std::slice::from_raw_parts(p.data, p.len) },
            frame_index: p.frame_index,
            idr: p.idr != 0,
            encode_us: p.encode_us,
        })
    }

    /// Reference-frame invalidation; false = send an IDR instead.
    pub fn invalidate(&mut self, first: u64, last: u64) -> bool {
        // SAFETY: valid encoder.
        unsafe { cs_enc_invalidate(self.e, first, last) == 0 }
    }
}

impl Drop for Encoder<'_> {
    fn drop(&mut self) {
        // SAFETY: owned handle, closed once.
        unsafe { cs_enc_close(self.e) }
    }
}

pub struct Decoder<'g> {
    d: *mut CsDec,
    _gpu: PhantomData<&'g Gpu>,
}

impl Decoder<'_> {
    pub fn decode(&mut self, data: &[u8]) -> Result<Option<DecFrame>> {
        let mut e = ErrBuf::new();
        let mut f = DecFrame::default();
        // SAFETY: valid decoder, slice outlives the call.
        let r = unsafe { cs_dec_decode(self.d, data.as_ptr(), data.len(), &mut f, e.ptr(), 512) };
        match r {
            1 => Ok(Some(f)),
            0 => Ok(None),
            _ => Err(anyhow!("decode: {}", e.get())),
        }
    }
}

impl Drop for Decoder<'_> {
    fn drop(&mut self) {
        // SAFETY: owned handle, closed once.
        unsafe { cs_dec_close(self.d) }
    }
}
