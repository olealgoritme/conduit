// d3d11_binding_probe.cpp: does every draw sample the texture it bound, with
// the texture coordinates its vertex streams carry, while textures, views and
// samplers are created on other threads and freed again, the way a game
// streams a map in?
//
// Worker threads keep creating small textures, each filled with a colour that
// encodes its id, plus a shader resource view (some restricted to a mip range)
// and a sampler with a varied description. The render thread retires old ones
// and draws one quad per grid cell into a render target: the cell's texture
// bound, positions from vertex stream 0 and texture coordinates from stream 1
// (R16G16_FLOAT, at a non-zero buffer offset, with the stride varied per
// draw), an element in slot 2 that no buffer is bound to (reads zero), and
// draws recorded either on the immediate context or on deferred contexts and
// executed as command lists. Every frame the target is read back and each
// cell must hold its texture's colour: a mismatch names the id it got instead.
//
// Build (host):
//   x86_64-w64-mingw32-g++ -O2 -static -o d3d11_binding_probe.exe d3d11_binding_probe.cpp
//       -ld3d11 -ldxgi -ld3dcompiler_47 -ldxguid -luuid
// Run (guest): d3d11_binding_probe.exe [helios|warp] [seconds] [workers]
// Exit code 0 when every cell of every frame matches.
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <d3d11.h>
#include <dxgi1_2.h>
#include <d3dcompiler.h>
#include <atomic>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <cwchar>
#include <deque>
#include <mutex>
#include <thread>
#include <vector>

static IDXGIAdapter1* HeliosAdapter() {
  IDXGIFactory1* f = nullptr;
  if (FAILED(CreateDXGIFactory1(__uuidof(IDXGIFactory1), (void**)&f))) return nullptr;
  IDXGIAdapter1* a = nullptr; IDXGIAdapter1* chosen = nullptr;
  for (UINT i = 0; f->EnumAdapters1(i, &a) != DXGI_ERROR_NOT_FOUND; ++i) {
    DXGI_ADAPTER_DESC1 d{}; a->GetDesc1(&d);
    if (!chosen && wcsstr(d.Description, L"Helios")) { chosen = a; continue; }
    a->Release();
  }
  f->Release();
  return chosen;
}

static const char* kVS =
  "struct V { float3 p : POSITION; float2 uv : TEXCOORD0; float4 z : COLOR0; };\n"
  "struct O { float4 pos : SV_Position; float2 uv : TEXCOORD0; float4 z : COLOR0; };\n"
  "O main(V v) { O o; o.pos = float4(v.p, 1); o.uv = v.uv; o.z = v.z; return o; }\n";
// The texture is sampled at the quad's interpolated coordinate (a fixed
// texel: every texel of a texture holds its colour); the slot-2 element must
// read zero, so adding it keeps the colour.
static const char* kPS =
  "Texture2D t : register(t0); SamplerState s : register(s0);\n"
  "float4 main(float4 pos : SV_Position, float2 uv : TEXCOORD0, float4 z : COLOR0) : SV_Target {\n"
  "  return t.SampleLevel(s, uv, 0) + z; }\n";

static ID3DBlob* Compile(const char* src, const char* target) {
  ID3DBlob* b = nullptr; ID3DBlob* e = nullptr;
  if (FAILED(D3DCompile(src, strlen(src), nullptr, nullptr, nullptr, "main", target, 0, 0, &b, &e))) {
    printf("compile: %s\n", e ? (const char*)e->GetBufferPointer() : "?"); exit(3);
  }
  return b;
}

static uint32_t ColorOf(uint32_t id) { return 0xff000000u | (0x5au << 16) | ((id >> 8 & 0xff) << 8) | (id & 0xff); }

struct Item {
  uint32_t id;
  ID3D11Texture2D* tex;
  ID3D11ShaderResourceView* srv;
  ID3D11SamplerState* smp;
};

static uint16_t Half(float f) {
  uint32_t x; memcpy(&x, &f, 4);
  uint32_t s = (x >> 16) & 0x8000, e = ((x >> 23) & 0xff), m = x & 0x7fffff;
  if (e == 0) return (uint16_t)s;
  int he = (int)e - 127 + 15;
  if (he <= 0) return (uint16_t)s;
  if (he >= 31) return (uint16_t)(s | 0x7c00);
  return (uint16_t)(s | (he << 10) | (m >> 13));
}

int main(int argc, char** argv) {
  setvbuf(stdout, nullptr, _IONBF, 0);
  const char* sel = argc > 1 ? argv[1] : "helios";
  const int seconds = argc > 2 ? atoi(argv[2]) : 20;
  const int workers = argc > 3 ? atoi(argv[3]) : 4;
  const bool warp = strcmp(sel, "warp") == 0;
  IDXGIAdapter1* adapter = warp ? nullptr : HeliosAdapter();
  D3D_DRIVER_TYPE dt = warp ? D3D_DRIVER_TYPE_WARP : adapter ? D3D_DRIVER_TYPE_UNKNOWN : D3D_DRIVER_TYPE_HARDWARE;
  D3D_FEATURE_LEVEL fl = D3D_FEATURE_LEVEL_11_0;
  ID3D11Device* dev = nullptr; ID3D11DeviceContext* ctx = nullptr;
  if (FAILED(D3D11CreateDevice(adapter, dt, nullptr, 0, &fl, 1, D3D11_SDK_VERSION, &dev, nullptr, &ctx))) {
    printf("no device\n"); return 2;
  }
  D3D11_FEATURE_DATA_THREADING th{};
  dev->CheckFeatureSupport(D3D11_FEATURE_THREADING, &th, sizeof(th));
  printf("device %s %ds workers=%d driver command lists=%d concurrent creates=%d\n",
         warp ? "WARP" : (adapter ? "Helios" : "default"), seconds, workers, th.DriverCommandLists, th.DriverConcurrentCreates);

  ID3DBlob* vsb = Compile(kVS, "vs_5_0"); ID3DBlob* psb = Compile(kPS, "ps_5_0");
  ID3D11VertexShader* vs; ID3D11PixelShader* ps;
  dev->CreateVertexShader(vsb->GetBufferPointer(), vsb->GetBufferSize(), nullptr, &vs);
  dev->CreatePixelShader(psb->GetBufferPointer(), psb->GetBufferSize(), nullptr, &ps);
  D3D11_INPUT_ELEMENT_DESC ie[] = {
    {"POSITION", 0, DXGI_FORMAT_R32G32B32_FLOAT, 0, 0, D3D11_INPUT_PER_VERTEX_DATA, 0},
    {"TEXCOORD", 0, DXGI_FORMAT_R16G16_FLOAT, 1, 4, D3D11_INPUT_PER_VERTEX_DATA, 0},
    {"COLOR", 0, DXGI_FORMAT_R8G8B8A8_UNORM, 2, 0, D3D11_INPUT_PER_VERTEX_DATA, 0},
  };
  ID3D11InputLayout* il; dev->CreateInputLayout(ie, 3, vsb->GetBufferPointer(), vsb->GetBufferSize(), &il);

  // Grid of cells; the vertex data of every cell lives in two big buffers.
  const int G = 32, RT = 1024, CELL = RT / G;
  const int cells = G * G;
  std::vector<float> pos; std::vector<uint8_t> uvs;
  // stream 1: per vertex [4 bytes padding][R16G16 uv][pad to stride]; strides
  // 8, 12, 16 by cell column to vary the binding stride
  std::vector<uint32_t> uvOffset(cells), uvStride(cells);
  for (int c = 0; c < cells; c++) {
    int cx = c % G, cy = c / G;
    float x0 = -1 + 2.0f * (cx * CELL + 2) / RT, x1 = -1 + 2.0f * ((cx + 1) * CELL - 2) / RT;
    float y0 = 1 - 2.0f * (cy * CELL + 2) / RT, y1 = 1 - 2.0f * ((cy + 1) * CELL - 2) / RT;
    float q[6][3] = {{x0, y0, 0}, {x1, y0, 0}, {x0, y1, 0}, {x0, y1, 0}, {x1, y0, 0}, {x1, y1, 0}};
    for (auto& v : q) for (float f : v) pos.push_back(f);
    uint32_t stride = 8 + 4 * (cx % 3);
    uint32_t off = (uint32_t)uvs.size();
    uvOffset[c] = off; uvStride[c] = stride;
    for (int v = 0; v < 6; v++) {
      size_t base = uvs.size(); uvs.resize(base + stride, 0xcd);
      uint16_t u = Half(0.5f), w = Half(0.5f);  // texel centre region of the 8x8 texture
      memcpy(&uvs[base + 4], &u, 2); memcpy(&uvs[base + 6], &w, 2);
    }
    while (uvs.size() % 16) uvs.push_back(0xcd);
  }
  D3D11_BUFFER_DESC bd{}; bd.Usage = D3D11_USAGE_IMMUTABLE; bd.BindFlags = D3D11_BIND_VERTEX_BUFFER;
  bd.ByteWidth = (UINT)(pos.size() * 4); D3D11_SUBRESOURCE_DATA sd{pos.data(), 0, 0};
  ID3D11Buffer* vbPos; dev->CreateBuffer(&bd, &sd, &vbPos);
  bd.ByteWidth = (UINT)uvs.size(); sd.pSysMem = uvs.data();
  ID3D11Buffer* vbUv; dev->CreateBuffer(&bd, &sd, &vbUv);

  D3D11_TEXTURE2D_DESC rtd{}; rtd.Width = RT; rtd.Height = RT; rtd.MipLevels = 1; rtd.ArraySize = 1;
  rtd.Format = DXGI_FORMAT_R8G8B8A8_UNORM; rtd.SampleDesc.Count = 1; rtd.BindFlags = D3D11_BIND_RENDER_TARGET;
  ID3D11Texture2D* rt; dev->CreateTexture2D(&rtd, nullptr, &rt);
  ID3D11RenderTargetView* rtv; dev->CreateRenderTargetView(rt, nullptr, &rtv);
  rtd.Usage = D3D11_USAGE_STAGING; rtd.BindFlags = 0; rtd.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
  ID3D11Texture2D* st; dev->CreateTexture2D(&rtd, nullptr, &st);
  D3D11_RASTERIZER_DESC rd{}; rd.FillMode = D3D11_FILL_SOLID; rd.CullMode = D3D11_CULL_NONE; rd.DepthClipEnable = TRUE;
  ID3D11RasterizerState* rs; dev->CreateRasterizerState(&rd, &rs);

  // Producers: textures with their colour in every texel of every level.
  std::mutex qm; std::deque<Item> fresh; std::atomic<bool> stop{false}; std::atomic<uint32_t> nextId{1};
  std::atomic<uint64_t> created{0};
  auto producer = [&](int w) {
    uint32_t rng = 777u * (w + 1);
    while (!stop.load()) {
      uint32_t id = nextId.fetch_add(1) & 0xffff;
      if (id == 0) continue;
      rng = rng * 1664525u + 1013904223u;
      const UINT size = 8u << (rng >> 30);          // 8..64
      const UINT mips = 1 + ((rng >> 20) & 3);      // 1..4
      D3D11_TEXTURE2D_DESC td{}; td.Width = size; td.Height = size; td.MipLevels = mips; td.ArraySize = 1;
      td.Format = DXGI_FORMAT_R8G8B8A8_UNORM; td.SampleDesc.Count = 1; td.BindFlags = D3D11_BIND_SHADER_RESOURCE;
      std::vector<uint32_t> px(size * size, ColorOf(id));
      D3D11_SUBRESOURCE_DATA init[4];
      for (UINT m = 0; m < mips; m++) init[m] = {px.data(), (size >> m) * 4, 0};
      Item it{id, nullptr, nullptr, nullptr};
      if (FAILED(dev->CreateTexture2D(&td, init, &it.tex))) continue;
      D3D11_SHADER_RESOURCE_VIEW_DESC vd{}; vd.Format = td.Format; vd.ViewDimension = D3D11_SRV_DIMENSION_TEXTURE2D;
      vd.Texture2D.MostDetailedMip = mips > 1 ? ((rng >> 8) % mips) : 0;
      vd.Texture2D.MipLevels = (UINT)-1;
      dev->CreateShaderResourceView(it.tex, (rng >> 4) & 1 ? &vd : nullptr, &it.srv);
      D3D11_SAMPLER_DESC smd{}; smd.Filter = (rng >> 12) & 1 ? D3D11_FILTER_MIN_MAG_MIP_POINT : D3D11_FILTER_MIN_MAG_MIP_LINEAR;
      smd.AddressU = smd.AddressV = smd.AddressW = (D3D11_TEXTURE_ADDRESS_MODE)(1 + ((rng >> 14) & 3));
      smd.MaxAnisotropy = 1; smd.ComparisonFunc = D3D11_COMPARISON_NEVER; smd.MaxLOD = D3D11_FLOAT32_MAX;
      smd.MipLODBias = (float)((rng >> 16) & 3) * 0.25f;
      dev->CreateSamplerState(&smd, &it.smp);
      { std::lock_guard<std::mutex> l(qm); fresh.push_back(it); }
      created++;
      while (!stop.load()) { { std::lock_guard<std::mutex> l(qm); if (fresh.size() < 2048) break; } Sleep(1); }
    }
  };
  std::vector<std::thread> threads;
  for (int w = 0; w < workers; w++) threads.emplace_back(producer, w);

  // Deferred contexts (when the driver records command lists itself, and also
  // when the runtime emulates them).
  ID3D11DeviceContext* dctx[2] = {nullptr, nullptr};
  for (auto& d : dctx) dev->CreateDeferredContext(0, &d);

  std::vector<Item> live;  // the cells' items
  std::vector<Item> retire;
  uint64_t frames = 0, bad = 0, badFrames = 0, fromOther = 0;
  uint32_t rng = 4242;
  const DWORD end = GetTickCount() + seconds * 1000;
  const float clear[4] = {0, 0, 0, 0};
  while (GetTickCount() < end) {
    // replace a quarter of the cells with fresh items
    {
      std::lock_guard<std::mutex> l(qm);
      while (live.size() < (size_t)cells && !fresh.empty()) { live.push_back(fresh.front()); fresh.pop_front(); }
      for (int k = 0; k < cells / 4 && !fresh.empty(); k++) {
        rng = rng * 1664525u + 1013904223u;
        size_t c = (rng >> 8) % live.size();
        retire.push_back(live[c]); live[c] = fresh.front(); fresh.pop_front();
      }
    }
    if (live.size() < (size_t)cells) { Sleep(1); continue; }
    const bool deferred = (frames % 3) == 2 && dctx[0] && dctx[1];
    auto record = [&](ID3D11DeviceContext* c, int first, int count) {
      D3D11_VIEWPORT vp{0, 0, (float)RT, (float)RT, 0, 1};
      c->RSSetViewports(1, &vp); c->RSSetState(rs);
      c->OMSetRenderTargets(1, &rtv, nullptr);
      c->IASetInputLayout(il); c->IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
      c->VSSetShader(vs, nullptr, 0); c->PSSetShader(ps, nullptr, 0);
      for (int i = first; i < first + count; i++) {
        ID3D11Buffer* vbs[3] = {vbPos, vbUv, nullptr};
        UINT strides[3] = {12, uvStride[i], 4}, offs[3] = {(UINT)(i * 6 * 12), uvOffset[i], 0};
        c->IASetVertexBuffers(0, 3, vbs, strides, offs);
        c->PSSetShaderResources(0, 1, &live[i].srv); c->PSSetSamplers(0, 1, &live[i].smp);
        c->Draw(6, 0);
      }
    };
    ctx->ClearRenderTargetView(rtv, clear);
    if (deferred) {
      ID3D11CommandList* cl[2] = {nullptr, nullptr};
      std::thread t1([&] { record(dctx[0], 0, cells / 2); dctx[0]->FinishCommandList(FALSE, &cl[0]); });
      std::thread t2([&] { record(dctx[1], cells / 2, cells / 2); dctx[1]->FinishCommandList(FALSE, &cl[1]); });
      t1.join(); t2.join();
      for (auto* l : cl) if (l) { ctx->ExecuteCommandList(l, FALSE); l->Release(); }
    } else {
      record(ctx, 0, cells);
    }
    ctx->CopyResource(st, rt);
    D3D11_MAPPED_SUBRESOURCE m{};
    if (FAILED(ctx->Map(st, 0, D3D11_MAP_READ, 0, &m))) { printf("map failed\n"); return 4; }
    unsigned frameBad = 0;
    for (int c = 0; c < cells; c++) {
      int cx = c % G, cy = c / G;
      uint32_t got = ((const uint32_t*)((const uint8_t*)m.pData + (size_t)(cy * CELL + CELL / 2) * m.RowPitch))[cx * CELL + CELL / 2];
      uint32_t want = ColorOf(live[c].id);
      if (got != want) {
        if (frameBad < 4 && badFrames < 8)
          printf("frame %llu%s cell %d: id %u (0x%08x) got 0x%08x%s\n", (unsigned long long)frames, deferred ? " (deferred)" : "",
                 c, live[c].id, want, got, (got & 0xffff0000u) == 0xff5a0000u ? " = another texture's colour" : "");
        if ((got & 0xffff0000u) == 0xff5a0000u) fromOther++;
        frameBad++;
      }
    }
    ctx->Unmap(st, 0);
    if (frameBad) { bad += frameBad; badFrames++; }
    frames++;
    // free what this frame no longer uses (after the readback, so not in use)
    for (auto& it : retire) { it.smp->Release(); it.srv->Release(); it.tex->Release(); }
    retire.clear();
  }
  stop = true;
  for (auto& t : threads) t.join();
  printf("frames %llu, textures created %llu, wrong cells %llu in %llu frames (%llu showing another texture)\n",
         (unsigned long long)frames, (unsigned long long)created.load(), (unsigned long long)bad,
         (unsigned long long)badFrames, (unsigned long long)fromOther);
  printf("RESULT %s\n", bad ? "FAIL" : "PASS");
  return bad ? 1 : 0;
}
