// d3d11_frontend_probe.cpp: does every draw see exactly the data the app gave
// it, in the call order the app gave it, when the immediate context's calls
// are replayed later on another thread (the UMD's threaded front end)?
//
// One quad per cell of a 16x16 grid per frame; each cell's colour is built
// from four independent sources, each written right before its draw:
//   R  a dynamic constant buffer mapped WRITE_DISCARD for every draw (b0);
//   G  a default constant buffer updated with UpdateSubresource (b1), every
//      other draw with UpdateSubresource1 and D3D11_COPY_DISCARD when the
//      runtime has ID3D11DeviceContext1;
//   B  one of 8 small textures, Load()ed at texel (0,0). Texture f%8 is
//      rewritten with UpdateSubresource in the middle of frame f, texture
//      (f+4)%8 through a staging texture (Map WRITE + CopySubresourceRegion) a
//      quarter in: draws before the write must see the old value, after it the
//      new one. Every texture's view is recreated each frame and the old view
//      released while queued draws still use it;
//   A  a per-vertex value from a dynamic vertex buffer used as a ring:
//      appended with WRITE_NO_OVERWRITE, WRITE_DISCARD when it wraps.
// With `dc`, the last quarter of the cells is recorded on a deferred context
// in two command lists per frame (same input layout in both; a dynamic
// constant buffer mapped WRITE_DISCARD on the deferred context per draw) and
// executed on the immediate context.
// With `threads=N`, N threads create and release textures with initial data,
// views and samplers the whole time (a game streaming its map in).
// Every frame an event query is ended and polled (DONOTFLUSH, bounded) the
// next frame; the target is copied to a staging texture, mapped READ, and
// every cell's centre pixel compared with the expected colour.
//
// Build (host, MinGW-w64):
//   x86_64-w64-mingw32-g++ -O2 -static -o d3d11_frontend_probe.exe d3d11_frontend_probe.cpp
//       -ld3d11 -ldxgi -ld3dcompiler_47 -ldxguid -luuid
// Run (guest): d3d11_frontend_probe.exe [helios|warp] [seconds=20] [threads=2] [dc]
// Prints per second: frames, cells checked, mismatches (first ones named by
// cell, channel, got and expected). Exit code 0 when every cell of every
// frame matched, 1 otherwise.
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <d3d11_1.h>
#include <dxgi1_2.h>
#include <d3dcompiler.h>
#include <atomic>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <cwchar>
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
  "struct V { float2 p : POSITION; float a : COLOR0; };\n"
  "struct O { float4 pos : SV_Position; float a : COLOR0; };\n"
  "O main(V v) { O o; o.pos = float4(v.p, 0, 1); o.a = v.a; return o; }\n";
static const char* kPS =
  "cbuffer Dyn : register(b0) { float4 dyn; };\n"
  "cbuffer Def : register(b1) { float4 def; };\n"
  "Texture2D<float4> t : register(t0);\n"
  "float4 main(float4 pos : SV_Position, float a : COLOR0) : SV_Target {\n"
  "  return float4(dyn.x, def.x, t.Load(int3(0, 0, 0)).x, a); }\n";

static ID3DBlob* Compile(const char* src, const char* target) {
  ID3DBlob* b = nullptr; ID3DBlob* e = nullptr;
  if (FAILED(D3DCompile(src, strlen(src), nullptr, nullptr, nullptr, "main", target, 0, 0, &b, &e))) {
    printf("compile: %s\n", e ? (const char*)e->GetBufferPointer() : "?"); exit(3);
  }
  return b;
}

static void Check(HRESULT hr, const char* what) {
  if (FAILED(hr)) { printf("%s failed 0x%08lx\n", what, (unsigned long)hr); exit(2); }
}

struct Vtx { float x, y, a; };

static const int G = 16, RT = 256, CELL = RT / G, CELLS = G * G, NTEX = 8;
static const int TEX = 4;                         // texture size (texel (0,0) is read)
static const UINT RING_VERTS = CELLS * 6 * 3;     // the vertex ring wraps every ~3 frames

static void Quad(int c, float a, Vtx* out) {
  int cx = c % G, cy = c / G;
  float x0 = -1 + 2.0f * (cx * CELL + 2) / RT, x1 = -1 + 2.0f * ((cx + 1) * CELL - 2) / RT;
  float y0 = 1 - 2.0f * (cy * CELL + 2) / RT, y1 = 1 - 2.0f * ((cy + 1) * CELL - 2) / RT;
  Vtx q[6] = {{x0, y0, a}, {x1, y0, a}, {x0, y1, a}, {x0, y1, a}, {x1, y0, a}, {x1, y1, a}};
  memcpy(out, q, sizeof(q));
}

static float U(uint32_t v) { return (float)(v & 255) / 255.0f; }

int main(int argc, char** argv) {
  setvbuf(stdout, nullptr, _IONBF, 0);
  const char* sel = "helios"; int seconds = 20, nthreads = 2; bool useDc = false;
  int pos = 0;
  for (int i = 1; i < argc; i++) {
    if (!strcmp(argv[i], "dc")) useDc = true;
    else if (!strncmp(argv[i], "threads=", 8)) nthreads = atoi(argv[i] + 8);
    else if (!strcmp(argv[i], "warp") || !strcmp(argv[i], "helios")) sel = argv[i];
    else if (pos++ == 0) seconds = atoi(argv[i]);
  }
  const bool warp = !strcmp(sel, "warp");
  IDXGIAdapter1* adapter = warp ? nullptr : HeliosAdapter();
  D3D_DRIVER_TYPE dt = warp ? D3D_DRIVER_TYPE_WARP : adapter ? D3D_DRIVER_TYPE_UNKNOWN : D3D_DRIVER_TYPE_HARDWARE;
  D3D_FEATURE_LEVEL fls[] = {D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0};
  ID3D11Device* dev = nullptr; ID3D11DeviceContext* ctx = nullptr;
  if (FAILED(D3D11CreateDevice(adapter, dt, nullptr, 0, fls, 2, D3D11_SDK_VERSION, &dev, nullptr, &ctx)) &&
      FAILED(D3D11CreateDevice(adapter, dt, nullptr, 0, fls + 1, 1, D3D11_SDK_VERSION, &dev, nullptr, &ctx))) {
    printf("no device\n"); return 2;
  }
  ID3D11DeviceContext1* ctx1 = nullptr;
  ctx->QueryInterface(__uuidof(ID3D11DeviceContext1), (void**)&ctx1);
  printf("device %s %ds threads=%d dc=%d UpdateSubresource1=%d\n", warp ? "WARP" : (adapter ? "Helios" : "default"),
         seconds, nthreads, useDc, ctx1 != nullptr);

  ID3DBlob* vsb = Compile(kVS, "vs_5_0"); ID3DBlob* psb = Compile(kPS, "ps_5_0");
  ID3D11VertexShader* vs; ID3D11PixelShader* ps;
  Check(dev->CreateVertexShader(vsb->GetBufferPointer(), vsb->GetBufferSize(), nullptr, &vs), "vs");
  Check(dev->CreatePixelShader(psb->GetBufferPointer(), psb->GetBufferSize(), nullptr, &ps), "ps");
  D3D11_INPUT_ELEMENT_DESC ie[] = {
    {"POSITION", 0, DXGI_FORMAT_R32G32_FLOAT, 0, 0, D3D11_INPUT_PER_VERTEX_DATA, 0},
    {"COLOR", 0, DXGI_FORMAT_R32_FLOAT, 0, 8, D3D11_INPUT_PER_VERTEX_DATA, 0},
  };
  ID3D11InputLayout* il; Check(dev->CreateInputLayout(ie, 2, vsb->GetBufferPointer(), vsb->GetBufferSize(), &il), "il");

  D3D11_BUFFER_DESC bd{};
  bd.ByteWidth = 16; bd.Usage = D3D11_USAGE_DYNAMIC; bd.BindFlags = D3D11_BIND_CONSTANT_BUFFER;
  bd.CPUAccessFlags = D3D11_CPU_ACCESS_WRITE;
  ID3D11Buffer* cbDyn; Check(dev->CreateBuffer(&bd, nullptr, &cbDyn), "cbDyn");
  ID3D11Buffer* cbDynDc; Check(dev->CreateBuffer(&bd, nullptr, &cbDynDc), "cbDynDc");
  bd.Usage = D3D11_USAGE_DEFAULT; bd.CPUAccessFlags = 0;
  ID3D11Buffer* cbDef; Check(dev->CreateBuffer(&bd, nullptr, &cbDef), "cbDef");
  bd.ByteWidth = RING_VERTS * sizeof(Vtx); bd.Usage = D3D11_USAGE_DYNAMIC; bd.BindFlags = D3D11_BIND_VERTEX_BUFFER;
  bd.CPUAccessFlags = D3D11_CPU_ACCESS_WRITE;
  ID3D11Buffer* vbRing; Check(dev->CreateBuffer(&bd, nullptr, &vbRing), "vbRing");
  // The deferred context's quads: a static vertex buffer, A = cell index.
  std::vector<Vtx> dcVerts(CELLS * 6);
  for (int c = 0; c < CELLS; c++) Quad(c, U(c * 29 + 7), &dcVerts[c * 6]);
  bd.ByteWidth = (UINT)(dcVerts.size() * sizeof(Vtx)); bd.Usage = D3D11_USAGE_IMMUTABLE; bd.CPUAccessFlags = 0;
  D3D11_SUBRESOURCE_DATA vsd{dcVerts.data(), 0, 0};
  ID3D11Buffer* vbDc; Check(dev->CreateBuffer(&bd, &vsd, &vbDc), "vbDc");

  D3D11_TEXTURE2D_DESC td{}; td.Width = TEX; td.Height = TEX; td.MipLevels = 1; td.ArraySize = 1;
  td.Format = DXGI_FORMAT_R8G8B8A8_UNORM; td.SampleDesc.Count = 1; td.BindFlags = D3D11_BIND_SHADER_RESOURCE;
  ID3D11Texture2D* tex[NTEX]; ID3D11ShaderResourceView* srv[NTEX]; uint32_t texVal[NTEX];
  for (int j = 0; j < NTEX; j++) {
    texVal[j] = (uint32_t)(j * 31 + 5) & 255;
    std::vector<uint32_t> px(TEX * TEX, 0xff000000u | texVal[j]);
    D3D11_SUBRESOURCE_DATA init{px.data(), TEX * 4, 0};
    Check(dev->CreateTexture2D(&td, &init, &tex[j]), "tex");
    Check(dev->CreateShaderResourceView(tex[j], nullptr, &srv[j]), "srv");
  }
  D3D11_TEXTURE2D_DESC sd2 = td; sd2.Usage = D3D11_USAGE_STAGING; sd2.BindFlags = 0;
  sd2.CPUAccessFlags = D3D11_CPU_ACCESS_WRITE;
  ID3D11Texture2D* upload; Check(dev->CreateTexture2D(&sd2, nullptr, &upload), "upload");

  D3D11_TEXTURE2D_DESC rtd{}; rtd.Width = RT; rtd.Height = RT; rtd.MipLevels = 1; rtd.ArraySize = 1;
  rtd.Format = DXGI_FORMAT_R8G8B8A8_UNORM; rtd.SampleDesc.Count = 1; rtd.BindFlags = D3D11_BIND_RENDER_TARGET;
  ID3D11Texture2D* rt; Check(dev->CreateTexture2D(&rtd, nullptr, &rt), "rt");
  ID3D11RenderTargetView* rtv; Check(dev->CreateRenderTargetView(rt, nullptr, &rtv), "rtv");
  rtd.Usage = D3D11_USAGE_STAGING; rtd.BindFlags = 0; rtd.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
  ID3D11Texture2D* readback; Check(dev->CreateTexture2D(&rtd, nullptr, &readback), "readback");
  D3D11_RASTERIZER_DESC rd{}; rd.FillMode = D3D11_FILL_SOLID; rd.CullMode = D3D11_CULL_NONE; rd.DepthClipEnable = TRUE;
  ID3D11RasterizerState* rs; Check(dev->CreateRasterizerState(&rd, &rs), "rs");
  D3D11_QUERY_DESC qd{D3D11_QUERY_EVENT, 0};
  ID3D11Query* ev[2]; Check(dev->CreateQuery(&qd, &ev[0]), "query"); Check(dev->CreateQuery(&qd, &ev[1]), "query");

  ID3D11DeviceContext* dctx = nullptr;
  if (useDc) Check(dev->CreateDeferredContext(0, &dctx), "deferred context");

  // Streaming threads: device-level creates and releases concurrent with the
  // immediate context (and with the front end's worker).
  std::atomic<bool> stop{false};
  std::atomic<uint64_t> streamed{0};
  std::vector<std::thread> threads;
  for (int w = 0; w < nthreads; w++) {
    threads.emplace_back([&, w] {
      uint32_t rng = 977u * (w + 1);
      std::vector<uint32_t> px(64 * 64);
      while (!stop.load()) {
        rng = rng * 1664525u + 1013904223u;
        UINT size = 8u << (rng >> 30);
        D3D11_TEXTURE2D_DESC d = td; d.Width = d.Height = size;
        for (auto& p : px) p = rng;
        D3D11_SUBRESOURCE_DATA init{px.data(), size * 4, 0};
        ID3D11Texture2D* t = nullptr; ID3D11ShaderResourceView* v = nullptr; ID3D11SamplerState* s = nullptr;
        if (SUCCEEDED(dev->CreateTexture2D(&d, &init, &t))) dev->CreateShaderResourceView(t, nullptr, &v);
        D3D11_SAMPLER_DESC smd{}; smd.Filter = D3D11_FILTER_MIN_MAG_MIP_POINT;
        smd.AddressU = smd.AddressV = smd.AddressW = D3D11_TEXTURE_ADDRESS_CLAMP;
        smd.MipLODBias = (float)(rng & 7) * 0.125f; smd.MaxLOD = D3D11_FLOAT32_MAX;
        dev->CreateSamplerState(&smd, &s);
        if (v) v->Release();
        if (t) t->Release();
        if (s) s->Release();
        streamed++;
      }
    });
  }

  D3D11_VIEWPORT vp{0, 0, (float)RT, (float)RT, 0, 1};
  const UINT stride = sizeof(Vtx), zero = 0;
  std::vector<uint32_t> expect(CELLS);
  UINT ringPos = RING_VERTS;  // forces a DISCARD on the first quad
  uint64_t frames = 0, checked = 0, bad = 0, badFrames = 0, discards = 0, ringWraps = 0;
  const int dcFirst = useDc ? CELLS - CELLS / 4 : CELLS;
  DWORD t0 = GetTickCount(), tlast = t0;

  for (uint32_t f = 0; GetTickCount() - t0 < (DWORD)seconds * 1000u; f++) {
    // Last frame's event: poll without a flush, bounded, as a frame limiter.
    if (f > 0) {
      for (int spin = 0; spin < 200 && ctx->GetData(ev[(f - 1) & 1], nullptr, 0, D3D11_ASYNC_GETDATA_DONOTFLUSH) == S_FALSE; spin++)
        Sleep(0);
    }
    float black[4] = {0, 0, 0, 0};
    ctx->ClearRenderTargetView(rtv, black);
    ctx->OMSetRenderTargets(1, &rtv, nullptr);
    ctx->RSSetViewports(1, &vp);
    ctx->RSSetState(rs);
    ctx->IASetInputLayout(il);
    ctx->IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
    ctx->IASetVertexBuffers(0, 1, &vbRing, &stride, &zero);
    ctx->VSSetShader(vs, nullptr, 0);
    ctx->PSSetShader(ps, nullptr, 0);
    ID3D11Buffer* cbs[2] = {cbDyn, cbDef};
    ctx->PSSetConstantBuffers(0, 2, cbs);

    // New views of every texture; the old ones are released after the draws
    // that use them were issued.
    ID3D11ShaderResourceView* oldSrv[NTEX];
    for (int j = 0; j < NTEX; j++) {
      oldSrv[j] = srv[j];
      Check(dev->CreateShaderResourceView(tex[j], nullptr, &srv[j]), "srv churn");
    }

    for (int c = 0; c < dcFirst; c++) {
      if (c == CELLS / 4) {
        // Staging upload of texture (f+4)%8 a quarter in.
        int j = (f + 4) % NTEX;
        texVal[j] = (f * 13 + 101) & 255;
        D3D11_MAPPED_SUBRESOURCE m;
        Check(ctx->Map(upload, 0, D3D11_MAP_WRITE, 0, &m), "map upload");
        for (int y = 0; y < TEX; y++)
          for (int x = 0; x < TEX; x++) ((uint32_t*)((char*)m.pData + y * m.RowPitch))[x] = 0xff000000u | texVal[j];
        ctx->Unmap(upload, 0);
        ctx->CopySubresourceRegion(tex[j], 0, 0, 0, 0, upload, 0, nullptr);
      }
      if (c == CELLS / 2) {
        // UpdateSubresource of texture f%8 halfway.
        int j = f % NTEX;
        texVal[j] = (f * 17 + 3) & 255;
        uint32_t px[TEX * TEX];
        for (auto& p : px) p = 0xff000000u | texVal[j];
        ctx->UpdateSubresource(tex[j], 0, nullptr, px, TEX * 4, 0);
      }
      const uint32_t r = (f * 7 + c * 3 + 1) & 255, g = (f * 5 + c * 11 + 2) & 255, a = (f + c * 13 + 3) & 255;
      const int j = c % NTEX;
      // R: dynamic constant buffer, DISCARD per draw.
      D3D11_MAPPED_SUBRESOURCE m;
      Check(ctx->Map(cbDyn, 0, D3D11_MAP_WRITE_DISCARD, 0, &m), "map cbDyn");
      float vr[4] = {U(r), 0, 0, 0}; memcpy(m.pData, vr, 16);
      ctx->Unmap(cbDyn, 0);
      discards++;
      // G: default constant buffer through UpdateSubresource(1); the app's
      // copy is overwritten right after the call.
      float vg[4] = {U(g), 0, 0, 0};
      if (ctx1 && (c & 1)) ctx1->UpdateSubresource1(cbDef, 0, nullptr, vg, 0, 0, D3D11_COPY_DISCARD);
      else ctx->UpdateSubresource(cbDef, 0, nullptr, vg, 0, 0);
      vg[0] = 1.0f;
      // A: the vertex ring, NO_OVERWRITE appends, DISCARD on wrap.
      D3D11_MAP mt = D3D11_MAP_WRITE_NO_OVERWRITE;
      if (ringPos + 6 > RING_VERTS) { mt = D3D11_MAP_WRITE_DISCARD; ringPos = 0; ringWraps++; }
      Check(ctx->Map(vbRing, 0, mt, 0, &m), "map ring");
      Quad(c, U(a), (Vtx*)m.pData + ringPos);
      ctx->Unmap(vbRing, 0);
      ctx->PSSetShaderResources(0, 1, &srv[j]);
      ctx->Draw(6, ringPos);
      ringPos += 6;
      expect[c] = r | (g << 8) | (texVal[j] << 16) | (a << 24);
    }
    for (int j = 0; j < NTEX; j++) oldSrv[j]->Release();

    if (dctx) {
      // Two command lists per frame on one deferred context: the second has
      // to rebind everything after the first one's Finish cleared the state.
      int per = (CELLS - dcFirst) / 2;
      for (int list = 0; list < 2; list++) {
        dctx->OMSetRenderTargets(1, &rtv, nullptr);
        dctx->RSSetViewports(1, &vp);
        dctx->RSSetState(rs);
        dctx->IASetInputLayout(il);
        dctx->IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        dctx->IASetVertexBuffers(0, 1, &vbDc, &stride, &zero);
        dctx->VSSetShader(vs, nullptr, 0);
        dctx->PSSetShader(ps, nullptr, 0);
        ID3D11Buffer* dcbs[2] = {cbDynDc, cbDef};
        dctx->PSSetConstantBuffers(0, 2, dcbs);
        for (int c = dcFirst + list * per; c < dcFirst + (list + 1) * per; c++) {
          const uint32_t r = (f * 3 + c * 7 + 9) & 255;
          const int j = c % NTEX;
          D3D11_MAPPED_SUBRESOURCE m;
          Check(dctx->Map(cbDynDc, 0, D3D11_MAP_WRITE_DISCARD, 0, &m), "dc map");
          float vr[4] = {U(r), 0, 0, 0}; memcpy(m.pData, vr, 16);
          dctx->Unmap(cbDynDc, 0);
          dctx->PSSetShaderResources(0, 1, &srv[j]);
          dctx->Draw(6, c * 6);
          // G is whatever cbDef holds when the list executes: the last
          // immediate update of the frame.
          expect[c] = r | (((f * 5 + (dcFirst - 1) * 11 + 2) & 255) << 8) | (texVal[j] << 16) |
                      (((c * 29 + 7) & 255) << 24);
        }
        ID3D11CommandList* cl = nullptr;
        Check(dctx->FinishCommandList(FALSE, &cl), "finish");
        ctx->ExecuteCommandList(cl, FALSE);
        cl->Release();
      }
    }
    ctx->End(ev[f & 1]);
    ctx->Flush();

    // Read back and compare every cell's centre.
    ctx->CopyResource(readback, rt);
    D3D11_MAPPED_SUBRESOURCE m;
    Check(ctx->Map(readback, 0, D3D11_MAP_READ, 0, &m), "map readback");
    uint64_t badHere = 0;
    for (int c = 0; c < CELLS; c++) {
      int x = (c % G) * CELL + CELL / 2, y = (c / G) * CELL + CELL / 2;
      uint32_t got = ((uint32_t*)((char*)m.pData + y * m.RowPitch))[x];
      checked++;
      if (got != expect[c]) {
        if (bad + badHere < 24) {
          static const char* ch[4] = {"R dyn-cb", "G update-cb", "B texture", "A vertex-ring"};
          for (int k = 0; k < 4; k++) {
            uint32_t gv = got >> (8 * k) & 255, ev2 = expect[c] >> (8 * k) & 255;
            if (gv != ev2)
              printf("frame %u cell %d %s: got %u expected %u%s\n", f, c, ch[k], gv, ev2, c >= dcFirst ? " (deferred)" : "");
          }
        }
        badHere++;
      }
    }
    ctx->Unmap(readback, 0);
    bad += badHere;
    badFrames += badHere != 0;
    frames++;
    DWORD now = GetTickCount();
    if (now - tlast >= 1000) {
      tlast = now;
      printf("t=%lus frames=%llu cells=%llu mismatches=%llu (frames %llu) discards=%llu ring_wraps=%llu streamed=%llu\n",
             (unsigned long)((now - t0) / 1000), (unsigned long long)frames, (unsigned long long)checked,
             (unsigned long long)bad, (unsigned long long)badFrames, (unsigned long long)discards,
             (unsigned long long)ringWraps, (unsigned long long)streamed.load());
    }
  }
  stop = true;
  for (auto& t : threads) t.join();
  printf("RESULT %s frames=%llu cells=%llu mismatches=%llu bad_frames=%llu\n", bad ? "FAIL" : "PASS",
         (unsigned long long)frames, (unsigned long long)checked, (unsigned long long)bad, (unsigned long long)badFrames);
  return bad ? 1 : 0;
}
