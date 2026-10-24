// d3d12_bc_upload: BC1/BC3/BC7 buffer<->texture copies through the normal D3D12
// API, checked byte for byte per block.
//
// The question it settles: are the Physical* extents of a D3D12 DDI
// PHYSICAL_PITCHED copy location (what the runtime turns a
// D3D12_PLACED_SUBRESOURCE_FOOTPRINT into) texels or blocks? A UMD that reads
// them the wrong way copies the wrong area for partial copies (too much or too
// little), so every case uploads known data with CopyTextureRegion, reads the
// subresources back through a readback buffer and compares each 4x4 block with
// a CPU model of what the texture must hold. Full-subresource copies use the
// footprints from GetCopyableFootprints; partial ones use footprints built here
// (smaller than the texture, at offsets, with source boxes, at mip tails where
// a mip is smaller than a block, and on a non-power-of-two chain whose mips
// are not multiples of 4).
//
// Usage:
//   d3d12_bc_upload.exe [--adapter N] [--warp] [--debug] [--timeout-ms N]
//                       [--only SUBSTR]
//   --adapter N   DXGI adapter index (default: the first that is not WARP)
//   --warp        run on WARP, the control arm: proves the test itself
//   --debug       enable the D3D12 debug layer (needs Graphics Tools)
//   --timeout-ms  per fence wait, default 10000
//   --only        run only cases whose name contains SUBSTR
//
// Output: one "PASS <case>" or "FAIL <case>: ..." line per case ("SKIP" when
// the format cannot be used that way), then
//   RESULT PASS|FAIL cases=N failed=M
// Exit code: 0 all passed, 1 a case failed, 2 setup error, 3 a fence wait timed
// out or the device was removed (the run stops there).
//
// Build (MinGW-w64 on Linux):
//   x86_64-w64-mingw32-g++ -std=c++17 -O2 -static d3d12_bc_upload.cpp
//       -o d3d12_bc_upload.exe -ldxgi -luuid
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <d3d12.h>
#include <d3d12sdklayers.h>
#include <dxgi1_4.h>

#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <functional>
#include <string>
#include <vector>

typedef HRESULT(WINAPI* PfnCreateDevice)(IUnknown*, D3D_FEATURE_LEVEL, REFIID, void**);
typedef HRESULT(WINAPI* PfnGetDebugInterface)(REFIID, void**);

static int g_cases = 0, g_failed = 0, g_skipped = 0;

static void finish(int code, const char* why) {
  std::printf("RESULT %s cases=%d failed=%d\n", (code == 0) ? "PASS" : "FAIL", g_cases, g_failed);
  if (why) std::printf("aborted: %s\n", why);
  if (g_skipped) std::printf("skipped=%d\n", g_skipped);
  std::fflush(stdout);
  std::exit(code);
}

#define CHECK(expr)                                                                        \
  do {                                                                                     \
    HRESULT hr_ = (expr);                                                                  \
    if (FAILED(hr_)) {                                                                     \
      std::printf("ERROR %s:%d %s -> 0x%08lx\n", __FILE__, __LINE__, #expr, (unsigned long)hr_); \
      finish(2, "setup error");                                                            \
    }                                                                                      \
  } while (0)

static uint64_t mix(uint64_t x) {
  x += 0x9e3779b97f4a7c15ull;
  x = (x ^ (x >> 30)) * 0xbf58476d1ce4e5b9ull;
  x = (x ^ (x >> 27)) * 0x94d049bb133111ebull;
  return x ^ (x >> 31);
}

// The bytes of one block: a function of everything that identifies it.
static void gen_block(uint8_t* out, UINT bpb, uint64_t seed, UINT x, UINT y, UINT z) {
  uint64_t h = mix(seed ^ mix((uint64_t)x | ((uint64_t)y << 20) | ((uint64_t)z << 40)));
  for (UINT i = 0; i < bpb; i += 8) {
    h = mix(h + i);
    std::memcpy(out + i, &h, 8);
  }
}

static const uint8_t kSentinel = 0xCD;

struct Box { UINT l, t, f, r, b, bk; };

struct Tex {
  const char* label;
  ID3D12Resource* res = nullptr;
  D3D12_RESOURCE_DESC desc = {};
  UINT bpb = 0;
  UINT mips = 0, slices = 0;
  // Per subresource: the blocks the texture must hold (row-major, then depth).
  std::vector<std::vector<uint8_t>> model;
  UINT sub_count() const { return mips * slices; }
  UINT mip_of(UINT sub) const { return sub % mips; }
  UINT w(UINT sub) const { UINT v = (UINT)(desc.Width >> mip_of(sub)); return v ? v : 1; }
  UINT h(UINT sub) const { UINT v = desc.Height >> mip_of(sub); return v ? v : 1; }
  UINT d(UINT sub) const {
    if (desc.Dimension != D3D12_RESOURCE_DIMENSION_TEXTURE3D) return 1;
    UINT v = desc.DepthOrArraySize >> mip_of(sub);
    return v ? v : 1;
  }
  UINT bw(UINT sub) const { return (w(sub) + 3) / 4; }
  UINT bh(UINT sub) const { return (h(sub) + 3) / 4; }
  uint8_t* block(UINT sub, UINT x, UINT y, UINT z) {
    return &model[sub][(((size_t)z * bh(sub) + y) * bw(sub) + x) * bpb];
  }
};

struct ReadCheck {
  UINT sub;
  UINT64 offset;  // in the readback buffer
  UINT row_pitch, rows_per_slice;
  Box blocks;     // block range of the subresource this footprint holds, from (0,0,0)
};

struct Ctx {
  ID3D12Device* dev = nullptr;
  ID3D12CommandQueue* queue = nullptr;
  ID3D12CommandAllocator* alloc = nullptr;
  ID3D12GraphicsCommandList* list = nullptr;
  ID3D12Fence* fence = nullptr;
  HANDLE event = nullptr;
  UINT64 value = 0;
  DWORD timeout_ms = 10000;
  ID3D12Resource* up = nullptr;
  ID3D12Resource* rb = nullptr;
  uint8_t* up_ptr = nullptr;
  UINT64 up_size = 0, up_used = 0;
  UINT64 rb_size = 0, rb_used = 0;
  std::vector<std::function<void()>> pending_reads;
  std::vector<ReadCheck> checks;
  std::string only;
  bool overflow = false;
};

static UINT64 align(UINT64 v, UINT64 a) { return (v + a - 1) / a * a; }

static D3D12_RESOURCE_BARRIER transition(ID3D12Resource* r, D3D12_RESOURCE_STATES a,
                                         D3D12_RESOURCE_STATES b) {
  D3D12_RESOURCE_BARRIER t = {};
  t.Type = D3D12_RESOURCE_BARRIER_TYPE_TRANSITION;
  t.Transition.pResource = r;
  t.Transition.Subresource = D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES;
  t.Transition.StateBefore = a;
  t.Transition.StateAfter = b;
  return t;
}

static ID3D12Resource* make_buffer(ID3D12Device* dev, UINT64 size, D3D12_HEAP_TYPE type,
                                   D3D12_RESOURCE_STATES state) {
  D3D12_HEAP_PROPERTIES hp = {};
  hp.Type = type;
  D3D12_RESOURCE_DESC rd = {};
  rd.Dimension = D3D12_RESOURCE_DIMENSION_BUFFER;
  rd.Width = size;
  rd.Height = 1;
  rd.DepthOrArraySize = 1;
  rd.MipLevels = 1;
  rd.SampleDesc.Count = 1;
  rd.Layout = D3D12_TEXTURE_LAYOUT_ROW_MAJOR;
  ID3D12Resource* r = nullptr;
  CHECK(dev->CreateCommittedResource(&hp, D3D12_HEAP_FLAG_NONE, &rd, state, nullptr,
                                     __uuidof(ID3D12Resource), (void**)&r));
  return r;
}

static UINT64 up_alloc(Ctx& c, UINT64 size) {
  UINT64 off = align(c.up_used, D3D12_TEXTURE_DATA_PLACEMENT_ALIGNMENT);
  if (off + size > c.up_size) { c.overflow = true; return 0; }
  c.up_used = off + size;
  return off;
}

static UINT64 rb_alloc(Ctx& c, UINT64 size) {
  UINT64 off = align(c.rb_used, D3D12_TEXTURE_DATA_PLACEMENT_ALIGNMENT);
  if (off + size > c.rb_size) { c.overflow = true; return 0; }
  c.rb_used = off + size;
  return off;
}

static D3D12_TEXTURE_COPY_LOCATION sub_loc(Tex& t, UINT sub) {
  D3D12_TEXTURE_COPY_LOCATION l = {};
  l.pResource = t.res;
  l.Type = D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX;
  l.SubresourceIndex = sub;
  return l;
}

static D3D12_TEXTURE_COPY_LOCATION buf_loc(ID3D12Resource* b, const D3D12_PLACED_SUBRESOURCE_FOOTPRINT& fp) {
  D3D12_TEXTURE_COPY_LOCATION l = {};
  l.pResource = b;
  l.Type = D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT;
  l.PlacedFootprint = fp;
  return l;
}

// Fill a footprint in the upload buffer with blocks of `seed`.
static void fill_footprint(Ctx& c, Tex& t, UINT64 off, UINT row_pitch, UINT rows, UINT bcols,
                           UINT depth, uint64_t seed) {
  for (UINT z = 0; z < depth; z++)
    for (UINT y = 0; y < rows; y++)
      for (UINT x = 0; x < bcols; x++)
        gen_block(c.up_ptr + off + (UINT64)z * row_pitch * rows + (UINT64)y * row_pitch + (UINT64)x * t.bpb,
                  t.bpb, seed, x, y, z);
}

// The whole subresource, with the runtime's own footprint.
static void upload_full(Ctx& c, Tex& t, UINT sub, uint64_t seed) {
  D3D12_PLACED_SUBRESOURCE_FOOTPRINT fp = {};
  UINT rows = 0;
  UINT64 row_size = 0, total = 0;
  c.dev->GetCopyableFootprints(&t.desc, sub, 1, 0, &fp, &rows, &row_size, &total);
  UINT64 off = up_alloc(c, total);
  if (c.overflow) return;
  fp.Offset = off;
  const UINT depth = fp.Footprint.Depth;
  fill_footprint(c, t, off, fp.Footprint.RowPitch, rows, t.bw(sub), depth, seed);
  for (UINT z = 0; z < t.d(sub); z++)
    for (UINT y = 0; y < t.bh(sub); y++)
      for (UINT x = 0; x < t.bw(sub); x++)
        std::memcpy(t.block(sub, x, y, z),
                    c.up_ptr + off + (UINT64)z * fp.Footprint.RowPitch * rows +
                        (UINT64)y * fp.Footprint.RowPitch + (UINT64)x * t.bpb,
                    t.bpb);
  D3D12_TEXTURE_COPY_LOCATION dst = sub_loc(t, sub), src = buf_loc(c.up, fp);
  c.list->CopyTextureRegion(&dst, 0, 0, 0, &src, nullptr);
}

// A footprint of fw x fh x fd texels (multiples of 4 in x and y) built here;
// `box` selects part of it (nullptr: all of it), copied to (dx, dy, dz).
static void upload_region(Ctx& c, Tex& t, UINT sub, UINT dx, UINT dy, UINT dz, UINT fw, UINT fh,
                          UINT fd, const Box* box, uint64_t seed) {
  const UINT bcols = (fw + 3) / 4, rows = (fh + 3) / 4;
  const UINT pitch = (UINT)align((UINT64)bcols * t.bpb, D3D12_TEXTURE_DATA_PITCH_ALIGNMENT);
  UINT64 off = up_alloc(c, (UINT64)pitch * rows * fd);
  if (c.overflow) return;
  fill_footprint(c, t, off, pitch, rows, bcols, fd, seed);
  D3D12_PLACED_SUBRESOURCE_FOOTPRINT fp = {};
  fp.Offset = off;
  fp.Footprint.Format = t.desc.Format;
  fp.Footprint.Width = fw;
  fp.Footprint.Height = fh;
  fp.Footprint.Depth = fd;
  fp.Footprint.RowPitch = pitch;
  const Box all = { 0, 0, 0, fw, fh, fd };
  const Box b = box ? *box : all;
  // The model: block (i, j, k) of the box lands at the destination's block.
  const UINT bl = b.l / 4, bt = b.t / 4, br = (b.r + 3) / 4, bb = (b.b + 3) / 4;
  for (UINT k = 0; k < b.bk - b.f; k++)
    for (UINT j = 0; j < bb - bt; j++)
      for (UINT i = 0; i < br - bl; i++) {
        const UINT tx = dx / 4 + i, ty = dy / 4 + j, tz = dz + k;
        if (tx >= t.bw(sub) || ty >= t.bh(sub) || tz >= t.d(sub)) continue;
        std::memcpy(t.block(sub, tx, ty, tz),
                    c.up_ptr + off + (UINT64)(b.f + k) * pitch * rows + (UINT64)(bt + j) * pitch +
                        (UINT64)(bl + i) * t.bpb,
                    t.bpb);
      }
  D3D12_TEXTURE_COPY_LOCATION dst = sub_loc(t, sub), src = buf_loc(c.up, fp);
  if (box) {
    D3D12_BOX db = { b.l, b.t, b.f, b.r, b.b, b.bk };
    c.list->CopyTextureRegion(&dst, dx, dy, dz, &src, &db);
  } else {
    c.list->CopyTextureRegion(&dst, dx, dy, dz, &src, nullptr);
  }
}

// Read the whole subresource back with the runtime's own footprint.
static void read_full(Ctx& c, Tex& t, UINT sub) {
  D3D12_PLACED_SUBRESOURCE_FOOTPRINT fp = {};
  UINT rows = 0;
  UINT64 row_size = 0, total = 0;
  c.dev->GetCopyableFootprints(&t.desc, sub, 1, 0, &fp, &rows, &row_size, &total);
  UINT64 off = rb_alloc(c, total);
  if (c.overflow) return;
  fp.Offset = off;
  c.checks.push_back({ sub, off, fp.Footprint.RowPitch, rows, { 0, 0, 0, t.bw(sub), t.bh(sub), t.d(sub) } });
  Tex* tp = &t;
  c.pending_reads.push_back([&c, tp, sub, fp]() {
    D3D12_TEXTURE_COPY_LOCATION dst = buf_loc(c.rb, fp), src = sub_loc(*tp, sub);
    c.list->CopyTextureRegion(&dst, 0, 0, 0, &src, nullptr);
  });
}

// Read texel box `b` of the subresource into a footprint built here.
static void read_region(Ctx& c, Tex& t, UINT sub, Box b) {
  const UINT fw = (UINT)align(b.r - b.l, 4), fh = (UINT)align(b.b - b.t, 4), fd = b.bk - b.f;
  const UINT bcols = fw / 4, rows = fh / 4;
  const UINT pitch = (UINT)align((UINT64)bcols * t.bpb, D3D12_TEXTURE_DATA_PITCH_ALIGNMENT);
  UINT64 off = rb_alloc(c, (UINT64)pitch * rows * fd);
  if (c.overflow) return;
  D3D12_PLACED_SUBRESOURCE_FOOTPRINT fp = {};
  fp.Offset = off;
  fp.Footprint.Format = t.desc.Format;
  fp.Footprint.Width = fw;
  fp.Footprint.Height = fh;
  fp.Footprint.Depth = fd;
  fp.Footprint.RowPitch = pitch;
  c.checks.push_back({ sub, off, pitch, rows, { b.l / 4, b.t / 4, b.f, (b.r + 3) / 4, (b.b + 3) / 4, b.bk } });
  Tex* tp = &t;
  c.pending_reads.push_back([&c, tp, sub, fp, b]() {
    D3D12_TEXTURE_COPY_LOCATION dst = buf_loc(c.rb, fp), src = sub_loc(*tp, sub);
    D3D12_BOX sb = { b.l, b.t, b.f, b.r, b.b, b.bk };
    c.list->CopyTextureRegion(&dst, 0, 0, 0, &src, &sb);
  });
}

static void submit_and_wait(Ctx& c, const char* name) {
  CHECK(c.list->Close());
  ID3D12CommandList* lists[] = { c.list };
  c.queue->ExecuteCommandLists(1, lists);
  CHECK(c.queue->Signal(c.fence, ++c.value));
  if (c.fence->GetCompletedValue() < c.value) {
    CHECK(c.fence->SetEventOnCompletion(c.value, c.event));
    if (WaitForSingleObject(c.event, c.timeout_ms) != WAIT_OBJECT_0) {
      g_cases++;
      g_failed++;
      std::printf("FAIL %s: fence %llu not reached in %lu ms (completed %llu)\n", name,
                  (unsigned long long)c.value, (unsigned long)c.timeout_ms,
                  (unsigned long long)c.fence->GetCompletedValue());
      finish(3, "fence timeout");
    }
  }
  HRESULT removed = c.dev->GetDeviceRemovedReason();
  if (FAILED(removed)) {
    g_cases++;
    g_failed++;
    std::printf("FAIL %s: device removed 0x%08lx\n", name, (unsigned long)removed);
    finish(3, "device removed");
  }
  CHECK(c.alloc->Reset());
  CHECK(c.list->Reset(c.alloc, nullptr));
}

// `background` (full uploads) runs and completes first, so the partial copies
// `record` adds cannot race it: two copies writing one subresource without a
// barrier between them have no defined order. `record` then uploads and queues
// the readbacks, which run after a COPY_DEST -> COPY_SOURCE barrier.
static void run_case(Ctx& c, Tex& t, const std::string& name, const std::function<void()>& background,
                     const std::function<void()>& record) {
  if (!c.only.empty() && name.find(c.only) == std::string::npos) return;
  c.up_used = c.rb_used = 0;
  c.overflow = false;
  c.pending_reads.clear();
  c.checks.clear();
  if (background) {
    background();
    if (c.overflow) {
      std::printf("ERROR %s: staging buffers too small\n", name.c_str());
      finish(2, "setup error");
    }
    submit_and_wait(c, name.c_str());
  }
  record();
  if (c.overflow) {
    std::printf("ERROR %s: staging buffers too small\n", name.c_str());
    finish(2, "setup error");
  }
  // The sentinel tells "never written by the copy" apart from "wrong data".
  void* rbp = nullptr;
  CHECK(c.rb->Map(0, nullptr, &rbp));
  std::memset(rbp, kSentinel, (size_t)c.rb_used);
  c.rb->Unmap(0, nullptr);
  D3D12_RESOURCE_BARRIER b = transition(t.res, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COPY_SOURCE);
  c.list->ResourceBarrier(1, &b);
  for (auto& r : c.pending_reads) r();
  b = transition(t.res, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_COPY_DEST);
  c.list->ResourceBarrier(1, &b);
  submit_and_wait(c, name.c_str());

  D3D12_RANGE rr = { 0, (SIZE_T)c.rb_used };
  CHECK(c.rb->Map(0, &rr, &rbp));
  const uint8_t* rbb = (const uint8_t*)rbp;
  UINT64 bad = 0, untouched = 0, total = 0;
  char first[256] = "";
  for (const ReadCheck& k : c.checks) {
    for (UINT z = k.blocks.f; z < k.blocks.bk; z++)
      for (UINT y = k.blocks.t; y < k.blocks.b; y++)
        for (UINT x = k.blocks.l; x < k.blocks.r; x++) {
          total++;
          const uint8_t* got = rbb + k.offset + (UINT64)(z - k.blocks.f) * k.row_pitch * k.rows_per_slice +
                               (UINT64)(y - k.blocks.t) * k.row_pitch + (UINT64)(x - k.blocks.l) * t.bpb;
          const uint8_t* want = t.block(k.sub, x, y, z);
          if (std::memcmp(got, want, t.bpb) == 0) continue;
          bad++;
          bool sentinel = true;
          for (UINT i = 0; i < t.bpb; i++) sentinel = sentinel && got[i] == kSentinel;
          if (sentinel) untouched++;
          if (!first[0]) {
            unsigned long long g8 = 0, w8 = 0;
            std::memcpy(&g8, got, 8);
            std::memcpy(&w8, want, 8);
            std::snprintf(first, sizeof(first),
                          "first: sub %u (mip %u, %ux%u) block (%u,%u,%u) got %016llx want %016llx",
                          k.sub, t.mip_of(k.sub), t.w(k.sub), t.h(k.sub), x, y, z, g8, w8);
          }
        }
  }
  D3D12_RANGE wr = { 0, 0 };
  c.rb->Unmap(0, &wr);
  g_cases++;
  if (bad) {
    g_failed++;
    std::printf("FAIL %s: %llu of %llu blocks differ (%llu never written); %s\n", name.c_str(),
                (unsigned long long)bad, (unsigned long long)total, (unsigned long long)untouched, first);
  } else {
    std::printf("PASS %s (%llu blocks)\n", name.c_str(), (unsigned long long)total);
  }
}

static bool make_tex(Ctx& c, Tex& t, const char* label, DXGI_FORMAT fmt, UINT bpb,
                     D3D12_RESOURCE_DIMENSION dim, UINT w, UINT h, UINT depth_or_array, UINT mips) {
  t = Tex();
  t.label = label;
  t.bpb = bpb;
  t.desc.Dimension = dim;
  t.desc.Width = w;
  t.desc.Height = h;
  t.desc.DepthOrArraySize = (UINT16)depth_or_array;
  t.desc.MipLevels = (UINT16)mips;
  t.desc.Format = fmt;
  t.desc.SampleDesc.Count = 1;
  t.desc.Layout = D3D12_TEXTURE_LAYOUT_UNKNOWN;
  t.mips = mips;
  t.slices = (dim == D3D12_RESOURCE_DIMENSION_TEXTURE3D) ? 1 : depth_or_array;
  D3D12_HEAP_PROPERTIES hp = {};
  hp.Type = D3D12_HEAP_TYPE_DEFAULT;
  HRESULT hr = c.dev->CreateCommittedResource(&hp, D3D12_HEAP_FLAG_NONE, &t.desc, D3D12_RESOURCE_STATE_COPY_DEST,
                                              nullptr, __uuidof(ID3D12Resource), (void**)&t.res);
  if (FAILED(hr)) {
    std::printf("ERROR create %s: 0x%08lx\n", label, (unsigned long)hr);
    finish(2, "setup error");
  }
  t.model.resize(t.sub_count());
  for (UINT s = 0; s < t.sub_count(); s++) t.model[s].assign((size_t)t.bw(s) * t.bh(s) * t.d(s) * bpb, 0);
  return true;
}

static void free_tex(Tex& t) {
  if (t.res) t.res->Release();
  t.res = nullptr;
  t.model.clear();
}

static bool supports(Ctx& c, DXGI_FORMAT fmt, D3D12_FORMAT_SUPPORT1 bit) {
  D3D12_FEATURE_DATA_FORMAT_SUPPORT fs = {};
  fs.Format = fmt;
  if (FAILED(c.dev->CheckFeatureSupport(D3D12_FEATURE_FORMAT_SUPPORT, &fs, sizeof(fs)))) return false;
  return (fs.Support1 & bit) != 0;
}

static void skip(Ctx& c, const std::string& name, const char* why) {
  if (!c.only.empty() && name.find(c.only) == std::string::npos) return;
  g_skipped++;
  std::printf("SKIP %s: %s\n", name.c_str(), why);
}

struct Fmt { const char* name; DXGI_FORMAT fmt; UINT bpb; };

static void run_format(Ctx& c, const Fmt& f) {
  const std::string F = f.name;
  Tex t;
  uint64_t s = mix((uint64_t)f.fmt * 1000003ull);
  auto seed = [&s]() { return s = mix(s); };

  // 4096x4096, 13 mips: mip 11 is 2x2 and mip 12 is 1x1, smaller than a block.
  make_tex(c, t, "4096", f.fmt, f.bpb, D3D12_RESOURCE_DIMENSION_TEXTURE2D, 4096, 4096, 1, 13);
  run_case(c, t, F + " 4096 full upload+readback, 13 mips", nullptr, [&]() {
    for (UINT m = 0; m < 13; m++) upload_full(c, t, m, seed());
    for (UINT m = 0; m < 13; m++) read_full(c, t, m);
  });
  auto bg = [&](std::vector<UINT> subs) {
    return [&c, &t, &seed, subs]() { for (UINT sub : subs) upload_full(c, t, sub, seed()); };
  };
  run_case(c, t, F + " 4096 mip0 256x256 footprint at (1024,512)", bg({ 0 }), [&]() {
    upload_region(c, t, 0, 1024, 512, 0, 256, 256, 1, nullptr, seed());
    read_full(c, t, 0);
  });
  run_case(c, t, F + " 4096 mip0 52x28 (13x7 blocks) at (148,364)", bg({ 0 }), [&]() {
    upload_region(c, t, 0, 148, 364, 0, 52, 28, 1, nullptr, seed());
    read_full(c, t, 0);
  });
  run_case(c, t, F + " 4096 mip0 box (64,128)-(192,192) of 512x512 footprint to (2048,3072)", bg({ 0 }), [&]() {
    const Box b = { 64, 128, 0, 192, 192, 1 };
    upload_region(c, t, 0, 2048, 3072, 0, 512, 512, 1, &b, seed());
    read_full(c, t, 0);
  });
  run_case(c, t, F + " 4096 mip3 64x64 at (448,448), bottom-right corner", bg({ 3 }), [&]() {
    upload_region(c, t, 3, 448, 448, 0, 64, 64, 1, nullptr, seed());
    read_full(c, t, 3);
  });
  run_case(c, t, F + " 4096 mip tail 4x4/2x2/1x1 with edge boxes", bg({ 10, 11, 12 }), [&]() {
    const Box b10 = { 0, 0, 0, 4, 4, 1 }, b11 = { 0, 0, 0, 2, 2, 1 }, b12 = { 0, 0, 0, 1, 1, 1 };
    upload_region(c, t, 10, 0, 0, 0, 4, 4, 1, &b10, seed());
    upload_region(c, t, 11, 0, 0, 0, 4, 4, 1, &b11, seed());
    upload_region(c, t, 12, 0, 0, 0, 4, 4, 1, &b12, seed());
    for (UINT m = 10; m < 13; m++) read_full(c, t, m);
    read_region(c, t, 11, b11);
    read_region(c, t, 12, b12);
  });
  run_case(c, t, F + " 4096 partial readbacks (mip0 260x100 at (1000,2000), corner, mip5)", nullptr, [&]() {
    upload_full(c, t, 0, seed());
    upload_full(c, t, 5, seed());
    read_region(c, t, 0, { 1000, 2000, 0, 1260, 2100, 1 });
    read_region(c, t, 0, { 4092, 4088, 0, 4096, 4096, 1 });
    read_region(c, t, 5, { 64, 8, 0, 128, 128, 1 });
  });
  free_tex(t);

  // 1000x600, 10 mips: 500x300, 250x150, 125x75, 62x37, 31x18, 15x9, 7x4, 3x2, 1x1.
  make_tex(c, t, "1000x600", f.fmt, f.bpb, D3D12_RESOURCE_DIMENSION_TEXTURE2D, 1000, 600, 1, 10);
  run_case(c, t, F + " 1000x600 full upload+readback, 10 mips", nullptr, [&]() {
    for (UINT m = 0; m < 10; m++) upload_full(c, t, m, seed());
    for (UINT m = 0; m < 10; m++) read_full(c, t, m);
  });
  run_case(c, t, F + " 1000x600 partial uploads and readbacks at non-multiple-of-4 mips",
           bg({ 2, 3, 4, 7, 8 }), [&]() {
    const Box off = { 8, 4, 0, 60, 32, 1 };  // 52x28 out of a 64x64 footprint
    upload_region(c, t, 2, 100, 40, 0, 64, 64, 1, &off, seed());
    const Box edge3 = { 0, 0, 0, 5, 11, 1 };  // reaches x=125, y=75: the edge of mip 3
    upload_region(c, t, 3, 120, 64, 0, 8, 12, 1, &edge3, seed());
    const Box m7 = { 0, 0, 0, 7, 4, 1 }, m8 = { 0, 0, 0, 3, 2, 1 };
    upload_region(c, t, 7, 0, 0, 0, 8, 4, 1, &m7, seed());
    upload_region(c, t, 8, 0, 0, 0, 4, 4, 1, &m8, seed());
    for (UINT m : { 2u, 3u, 7u, 8u }) read_full(c, t, m);
    read_region(c, t, 3, { 120, 64, 0, 125, 75, 1 });
    read_region(c, t, 4, { 56, 32, 0, 62, 37, 1 });
    read_region(c, t, 7, m7);
  });
  free_tex(t);

  // A 2-slice array, 1024x1024, 11 mips (subresource = mip + 11 * slice).
  make_tex(c, t, "array2", f.fmt, f.bpb, D3D12_RESOURCE_DIMENSION_TEXTURE2D, 1024, 1024, 2, 11);
  run_case(c, t, F + " 2-slice array 1024 full upload+readback, 11 mips", nullptr, [&]() {
    for (UINT sub = 0; sub < 22; sub++) upload_full(c, t, sub, seed());
    for (UINT sub = 0; sub < 22; sub++) read_full(c, t, sub);
  });
  run_case(c, t, F + " 2-slice array slice 1 partial (mip0 128x64 at (512,256), mip9 2x2)",
           bg({ 0, 9, 11, 20 }), [&]() {
    upload_region(c, t, 11, 512, 256, 0, 128, 64, 1, nullptr, seed());
    const Box b = { 0, 0, 0, 2, 2, 1 };
    upload_region(c, t, 20, 0, 0, 0, 4, 4, 1, &b, seed());
    for (UINT sub : { 0u, 9u, 11u, 20u }) read_full(c, t, sub);
    read_region(c, t, 11, { 500, 248, 0, 660, 340, 1 });
  });
  free_tex(t);

  // A 3D texture two slices deep: one copy location carries both (slice pitch).
  const std::string n3 = F + " 3D 256x256x2";
  if (!supports(c, f.fmt, D3D12_FORMAT_SUPPORT1_TEXTURE3D)) {
    skip(c, n3 + " full", "format has no TEXTURE3D support");
    skip(c, n3 + " partial", "format has no TEXTURE3D support");
    return;
  }
  make_tex(c, t, "3d", f.fmt, f.bpb, D3D12_RESOURCE_DIMENSION_TEXTURE3D, 256, 256, 2, 1);
  run_case(c, t, n3 + " full upload+readback", nullptr, [&]() {
    upload_full(c, t, 0, seed());
    read_full(c, t, 0);
  });
  run_case(c, t, n3 + " partial 64x32x2 at (128,64,0) and box readback", bg({ 0 }), [&]() {
    upload_region(c, t, 0, 128, 64, 0, 64, 32, 2, nullptr, seed());
    read_full(c, t, 0);
    read_region(c, t, 0, { 32, 16, 0, 160, 80, 2 });
  });
  free_tex(t);
}

int main(int argc, char** argv) {
  setvbuf(stdout, nullptr, _IONBF, 0);
  Ctx c;
  int adapter_index = -1;
  bool warp = false, debug = false;
  for (int i = 1; i < argc; i++) {
    if (!std::strcmp(argv[i], "--warp")) warp = true;
    else if (!std::strcmp(argv[i], "--debug")) debug = true;
    else if (!std::strcmp(argv[i], "--adapter") && i + 1 < argc) adapter_index = std::atoi(argv[++i]);
    else if (!std::strcmp(argv[i], "--timeout-ms") && i + 1 < argc) c.timeout_ms = (DWORD)std::atoi(argv[++i]);
    else if (!std::strcmp(argv[i], "--only") && i + 1 < argc) c.only = argv[++i];
    else {
      std::printf("usage: d3d12_bc_upload [--adapter N] [--warp] [--debug] [--timeout-ms N] [--only SUBSTR]\n");
      return 2;
    }
  }

  HMODULE d3d12 = LoadLibraryA("d3d12.dll");
  if (!d3d12) { std::printf("ERROR d3d12.dll did not load (%lu)\n", GetLastError()); finish(2, "setup error"); }
  char path[MAX_PATH] = {};
  GetModuleFileNameA(d3d12, path, MAX_PATH);
  std::printf("d3d12: %s\n", path);
  auto create_device = (PfnCreateDevice)(void*)GetProcAddress(d3d12, "D3D12CreateDevice");
  auto get_debug = (PfnGetDebugInterface)(void*)GetProcAddress(d3d12, "D3D12GetDebugInterface");
  if (!create_device) { std::printf("ERROR no D3D12CreateDevice\n"); finish(2, "setup error"); }
  if (debug) {
    ID3D12Debug* dbg = nullptr;
    if (get_debug && SUCCEEDED(get_debug(__uuidof(ID3D12Debug), (void**)&dbg))) {
      dbg->EnableDebugLayer();
      std::printf("debug layer on\n");
    } else {
      std::printf("debug layer unavailable\n");
    }
  }

  IDXGIFactory4* factory = nullptr;
  IDXGIAdapter1* adapter = nullptr;
  HRESULT fhr = CreateDXGIFactory2(0, __uuidof(IDXGIFactory4), (void**)&factory);
  if (FAILED(fhr)) {
    // Without DXGI the runtime's default adapter is the only choice.
    if (warp || adapter_index >= 0) CHECK(fhr);
    std::printf("adapter: default (CreateDXGIFactory2 0x%08lx)\n", (unsigned long)fhr);
  } else {
    if (warp) {
      CHECK(factory->EnumWarpAdapter(__uuidof(IDXGIAdapter1), (void**)&adapter));
    } else if (adapter_index >= 0) {
      CHECK(factory->EnumAdapters1((UINT)adapter_index, &adapter));
    } else {
      for (UINT i = 0; factory->EnumAdapters1(i, &adapter) == S_OK; i++) {
        DXGI_ADAPTER_DESC1 d = {};
        adapter->GetDesc1(&d);
        if (!(d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE)) break;
        adapter->Release();
        adapter = nullptr;
      }
      if (!adapter) { std::printf("ERROR no hardware adapter\n"); finish(2, "setup error"); }
    }
    DXGI_ADAPTER_DESC1 ad = {};
    adapter->GetDesc1(&ad);
    LARGE_INTEGER umd = {};
    if (SUCCEEDED(adapter->CheckInterfaceSupport(__uuidof(IDXGIDevice), &umd)))
      std::printf("adapter: %ls vendor=0x%04x device=0x%04x driver=%u.%u.%u.%u\n", ad.Description, ad.VendorId,
                  ad.DeviceId, (unsigned)HIWORD(umd.HighPart), (unsigned)LOWORD(umd.HighPart),
                  (unsigned)HIWORD(umd.LowPart), (unsigned)LOWORD(umd.LowPart));
    else
      std::printf("adapter: %ls vendor=0x%04x device=0x%04x\n", ad.Description, ad.VendorId, ad.DeviceId);
  }

  CHECK(create_device(adapter, D3D_FEATURE_LEVEL_11_0, __uuidof(ID3D12Device), (void**)&c.dev));
  D3D12_COMMAND_QUEUE_DESC qd = {};
  qd.Type = D3D12_COMMAND_LIST_TYPE_DIRECT;
  CHECK(c.dev->CreateCommandQueue(&qd, __uuidof(ID3D12CommandQueue), (void**)&c.queue));
  CHECK(c.dev->CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT, __uuidof(ID3D12CommandAllocator),
                                      (void**)&c.alloc));
  CHECK(c.dev->CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, c.alloc, nullptr,
                                 __uuidof(ID3D12GraphicsCommandList), (void**)&c.list));
  CHECK(c.dev->CreateFence(0, D3D12_FENCE_FLAG_NONE, __uuidof(ID3D12Fence), (void**)&c.fence));
  c.event = CreateEventA(nullptr, FALSE, FALSE, nullptr);
  if (!c.event) { std::printf("ERROR CreateEvent\n"); finish(2, "setup error"); }

  // The largest case is a full BC7 4096 chain (~22 MiB) plus small regions.
  c.up_size = c.rb_size = 48ull << 20;
  c.up = make_buffer(c.dev, c.up_size, D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ);
  c.rb = make_buffer(c.dev, c.rb_size, D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_STATE_COPY_DEST);
  D3D12_RANGE none = { 0, 0 };
  CHECK(c.up->Map(0, &none, (void**)&c.up_ptr));

  const Fmt fmts[] = {
    { "BC1", DXGI_FORMAT_BC1_UNORM, 8 },
    { "BC3", DXGI_FORMAT_BC3_UNORM, 16 },
    { "BC7", DXGI_FORMAT_BC7_UNORM, 16 },
  };
  for (const Fmt& f : fmts) run_format(c, f);

  if (g_cases == 0) finish(2, "no case ran");
  finish(g_failed ? 1 : 0, nullptr);
  return 0;
}
