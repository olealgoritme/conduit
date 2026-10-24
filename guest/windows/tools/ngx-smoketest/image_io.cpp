// ngx-smoketest: reading the --image input and writing PNG files through
// the Windows Imaging Component (PNG, BMP, JPEG, TIFF, GIF in; PNG out).
#include "smoketest.h"

#include <objbase.h>
#include <wincodec.h>

static IWICImagingFactory *wic()
{
    static IWICImagingFactory *factory;
    if (!factory) {
        HRESULT hr = CoInitializeEx(NULL, COINIT_MULTITHREADED);
        if (FAILED(hr) && hr != RPC_E_CHANGED_MODE)
            return NULL;
        if (FAILED(CoCreateInstance(CLSID_WICImagingFactory, NULL, CLSCTX_INPROC_SERVER, IID_IWICImagingFactory,
                                    (void **)&factory)))
            factory = NULL;
    }
    return factory;
}

bool image_load(const wchar_t *path, uint8_t **rgba, unsigned *w, unsigned *h)
{
    IWICImagingFactory *f = wic();
    if (!f) {
        fail("Windows Imaging Component not available");
        return false;
    }
    IWICBitmapDecoder *dec = NULL;
    IWICBitmapFrameDecode *frame = NULL;
    IWICFormatConverter *conv = NULL;
    bool good = false;
    UINT fw = 0, fh = 0;
    HRESULT hr = f->CreateDecoderFromFilename(path, NULL, GENERIC_READ, WICDecodeMetadataCacheOnDemand, &dec);
    if (SUCCEEDED(hr))
        hr = dec->GetFrame(0, &frame);
    if (SUCCEEDED(hr))
        hr = f->CreateFormatConverter(&conv);
    if (SUCCEEDED(hr))
        hr = conv->Initialize(frame, GUID_WICPixelFormat32bppRGBA, WICBitmapDitherTypeNone, NULL, 0.0,
                              WICBitmapPaletteTypeCustom);
    if (SUCCEEDED(hr))
        hr = conv->GetSize(&fw, &fh);
    if (SUCCEEDED(hr) && (fw == 0 || fh == 0 || fw > 16384 || fh > 16384))
        hr = E_INVALIDARG;
    if (SUCCEEDED(hr)) {
        *rgba = (uint8_t *)malloc((size_t)fw * fh * 4);
        hr = *rgba ? conv->CopyPixels(NULL, fw * 4, fw * fh * 4, *rgba) : E_OUTOFMEMORY;
        if (SUCCEEDED(hr)) {
            // The input is opaque colour: alpha is not upscaled.
            for (size_t i = 0; i < (size_t)fw * fh; i++)
                (*rgba)[i * 4 + 3] = 255;
            *w = fw;
            *h = fh;
            good = true;
        } else {
            free(*rgba);
            *rgba = NULL;
        }
    }
    if (!good)
        fail("cannot read %ls (HRESULT 0x%08lx)", path, (unsigned long)hr);
    if (conv) conv->Release();
    if (frame) frame->Release();
    if (dec) dec->Release();
    return good;
}

bool image_write_png(const wchar_t *path, const uint8_t *rgba, unsigned w, unsigned h, size_t pitch)
{
    IWICImagingFactory *f = wic();
    if (!f)
        return false;
    // WIC's PNG encoder takes BGRA.
    uint8_t *bgra = (uint8_t *)malloc((size_t)w * h * 4);
    if (!bgra)
        return false;
    for (unsigned y = 0; y < h; y++)
        for (unsigned x = 0; x < w; x++) {
            const uint8_t *s = rgba + (size_t)y * pitch + x * 4;
            uint8_t *d = bgra + ((size_t)y * w + x) * 4;
            d[0] = s[2];
            d[1] = s[1];
            d[2] = s[0];
            d[3] = 255;
        }
    IWICStream *stream = NULL;
    IWICBitmapEncoder *enc = NULL;
    IWICBitmapFrameEncode *frame = NULL;
    WICPixelFormatGUID fmt = GUID_WICPixelFormat32bppBGRA;
    HRESULT hr = f->CreateStream(&stream);
    if (SUCCEEDED(hr))
        hr = stream->InitializeFromFilename(path, GENERIC_WRITE);
    if (SUCCEEDED(hr))
        hr = f->CreateEncoder(GUID_ContainerFormatPng, NULL, &enc);
    if (SUCCEEDED(hr))
        hr = enc->Initialize(stream, WICBitmapEncoderNoCache);
    if (SUCCEEDED(hr))
        hr = enc->CreateNewFrame(&frame, NULL);
    if (SUCCEEDED(hr))
        hr = frame->Initialize(NULL);
    if (SUCCEEDED(hr))
        hr = frame->SetSize(w, h);
    if (SUCCEEDED(hr))
        hr = frame->SetPixelFormat(&fmt);
    if (SUCCEEDED(hr) && !IsEqualGUID(fmt, GUID_WICPixelFormat32bppBGRA))
        hr = E_FAIL;
    if (SUCCEEDED(hr))
        hr = frame->WritePixels(h, w * 4, w * h * 4, bgra);
    if (SUCCEEDED(hr))
        hr = frame->Commit();
    if (SUCCEEDED(hr))
        hr = enc->Commit();
    if (frame) frame->Release();
    if (enc) enc->Release();
    if (stream) stream->Release();
    free(bgra);
    return SUCCEEDED(hr);
}

void bilinear_upscale(const uint8_t *in, unsigned iw, unsigned ih, size_t ipitch, uint8_t *out, unsigned ow,
                      unsigned oh)
{
    for (unsigned y = 0; y < oh; y++) {
        float sy = ((float)y + 0.5f) * (float)ih / (float)oh - 0.5f;
        if (sy < 0.0f)
            sy = 0.0f;
        unsigned y0 = (unsigned)sy, y1 = y0 + 1 < ih ? y0 + 1 : ih - 1;
        float fy = sy - (float)y0;
        for (unsigned x = 0; x < ow; x++) {
            float sx = ((float)x + 0.5f) * (float)iw / (float)ow - 0.5f;
            if (sx < 0.0f)
                sx = 0.0f;
            unsigned x0 = (unsigned)sx, x1 = x0 + 1 < iw ? x0 + 1 : iw - 1;
            float fx = sx - (float)x0;
            const uint8_t *a = in + (size_t)y0 * ipitch + x0 * 4, *b = in + (size_t)y0 * ipitch + x1 * 4;
            const uint8_t *c = in + (size_t)y1 * ipitch + x0 * 4, *d = in + (size_t)y1 * ipitch + x1 * 4;
            uint8_t *o = out + ((size_t)y * ow + x) * 4;
            for (int k = 0; k < 4; k++) {
                float top = a[k] + (b[k] - a[k]) * fx, bot = c[k] + (d[k] - c[k]) * fx;
                o[k] = (uint8_t)(top + (bot - top) * fy + 0.5f);
            }
        }
    }
}
