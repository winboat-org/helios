// d3d11_shared_blob_truth_probe.cpp — RAW-MEMORY ground truth for the
// 2026-07-03 shared-surface clears-diverge/copies-propagate class.
//
// Same shape as d3d11_shared_content_probe.cpp (dev1 creates+clears a shared
// BGRA RT, dev2 opens it, clears diverge per image while copies propagate),
// plus: maps the surface's venus BLOB into this process via the Helios
// HELIOS_ESCAPE_MAP_BLOB verb and histograms the raw dwords after every step.
// This discriminates:
//   - WRITE-side divergence: dev1's clear #2 never reaches raw memory
//     (raw histogram stays at clear #1's color) — compression/metadata on the
//     producer image, or the clear rides somewhere else entirely.
//   - READ-side divergence: raw memory DOES flip to clear #2's color while
//     dev2 still reads clear #1 — the consumer image ignores memory.
// The blob region beyond the 256KiB main surface (if alloc_size > 256KiB) is
// histogrammed separately: driver aux/CCS metadata living INSIDE the shared
// allocation shows up there and changes on clears.
//
// Identity source: this process's own UMD log line
//   "DDI open_resource identity: res_id=N alloc_size=M ..."
// (written when dev2 opens the shared handle).
//
// Build (VM, vcvars64):
//   cl /EHsc /W4 Z:\tools\d3d11_shared_blob_truth_probe.cpp /I"Z:\icd\win-build\wdk-include" /link dxgi.lib d3d11.lib gdi32.lib
#include <windows.h>
#include <d3d11_4.h>
#include <dxgi1_6.h>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <cwchar>

#ifndef _NTDEF_
typedef LONG NTSTATUS, *PNTSTATUS;
#endif
#include <d3dkmthk.h>

// ── Helios escape structs (mirror protocol/src/escape.rs) ───────────────────
#define HELIOS_ESCAPE_MAGIC 0x48454C53u /* 'HELS' */
#define HELIOS_ESCAPE_VERSION 1u
#define HELIOS_ESCAPE_MAP_BLOB 0x0005u

struct helios_escape_header {
  UINT magic, cmd_type, version, size;
};
struct helios_escape_map_blob {
  struct helios_escape_header hdr;
  UINT64 out_user_va; // out
  UINT resource_id;   // in
  UINT map_cache;     // in/out
};

static D3DKMT_HANDLE g_adapter, g_device;

static int open_helios_kmt(LUID luid) {
  D3DKMT_OPENADAPTERFROMLUID oa;
  memset(&oa, 0, sizeof(oa));
  oa.AdapterLuid = luid;
  NTSTATUS st = D3DKMTOpenAdapterFromLuid(&oa);
  if (st != 0) { printf("OpenAdapterFromLuid st=0x%08x\n", (unsigned)st); return 1; }
  g_adapter = oa.hAdapter;
  D3DKMT_CREATEDEVICE cd;
  memset(&cd, 0, sizeof(cd));
  cd.hAdapter = g_adapter;
  st = D3DKMTCreateDevice(&cd);
  if (st != 0) { printf("KMTCreateDevice st=0x%08x\n", (unsigned)st); return 1; }
  g_device = cd.hDevice;
  printf("kmt adapter=0x%x device=0x%x\n", (unsigned)g_adapter, (unsigned)g_device);
  return 0;
}

static int escape(void* buf, UINT size) {
  D3DKMT_ESCAPE esc;
  memset(&esc, 0, sizeof(esc));
  esc.hAdapter = g_adapter;
  esc.hDevice = g_device;
  esc.Type = D3DKMT_ESCAPE_DRIVERPRIVATE;
  esc.pPrivateDriverData = buf;
  esc.PrivateDriverDataSize = size;
  NTSTATUS st = D3DKMTEscape(&esc);
  if (st != 0) { printf("D3DKMTEscape st=0x%08x\n", (unsigned)st); return 1; }
  return 0;
}

// Parse "DDI open_resource identity: res_id=N alloc_size=M" (LAST occurrence)
// from this process's UMD log.
static int parse_identity(UINT* res_id, UINT64* alloc_size) {
  char path[MAX_PATH];
  _snprintf_s(path, sizeof(path), _TRUNCATE,
              "C:\\ProgramData\\Helios\\umd-%lu.log", GetCurrentProcessId());
  for (int attempt = 0; attempt < 10; ++attempt) {
    FILE* f = nullptr;
    if (fopen_s(&f, path, "rb") == 0 && f) {
      fseek(f, 0, SEEK_END);
      long len = ftell(f);
      fseek(f, 0, SEEK_SET);
      char* buf = (char*)malloc(len + 1);
      if (buf) {
        fread(buf, 1, len, f);
        buf[len] = 0;
        const char* needle = "open_resource identity: res_id=";
        const char* found = nullptr;
        for (const char* p = buf; (p = strstr(p, needle)) != nullptr; p += 1)
          found = p;
        if (found) {
          unsigned rid = 0;
          unsigned long long asz = 0;
          if (sscanf_s(found + strlen(needle), "%u alloc_size=%llu", &rid, &asz) == 2 && rid) {
            *res_id = rid;
            *alloc_size = asz;
            free(buf);
            fclose(f);
            return 0;
          }
        }
        free(buf);
      }
      fclose(f);
    }
    Sleep(500);
  }
  return 1;
}

// Histogram the top dword values in [base, base+len).
static void hist(const volatile unsigned* base, size_t dwords, const char* label) {
  struct Slot { unsigned val; size_t count; };
  Slot slots[16];
  int nslots = 0;
  size_t other = 0;
  for (size_t i = 0; i < dwords; ++i) {
    unsigned v = base[i];
    int j = 0;
    for (; j < nslots; ++j)
      if (slots[j].val == v) { slots[j].count++; break; }
    if (j == nslots) {
      if (nslots < 16) { slots[nslots].val = v; slots[nslots].count = 1; nslots++; }
      else other++;
    }
  }
  // simple selection sort by count desc
  for (int i = 0; i < nslots; ++i)
    for (int j = i + 1; j < nslots; ++j)
      if (slots[j].count > slots[i].count) { Slot t = slots[i]; slots[i] = slots[j]; slots[j] = t; }
  printf("  raw[%s]:", label);
  int shown = 0;
  for (int i = 0; i < nslots && shown < 5; ++i, ++shown)
    printf(" 0x%08x x%zu", slots[i].val, slots[i].count);
  if (other) printf(" (+%zu uncounted)", other);
  printf("\n");
}

static volatile unsigned* g_blob = nullptr;
static UINT64 g_blob_size = 0;

static void dump_blob(const char* step) {
  if (!g_blob) return;
  const size_t main_dw = (256 * 256 * 4) / 4;
  size_t total_dw = (size_t)(g_blob_size / 4);
  if (total_dw > main_dw) {
    char l1[64], l2[64];
    _snprintf_s(l1, sizeof(l1), _TRUNCATE, "%s main 0..256K", step);
    _snprintf_s(l2, sizeof(l2), _TRUNCATE, "%s tail 256K..%llu", step, (unsigned long long)g_blob_size);
    hist(g_blob, main_dw, l1);
    hist(g_blob + main_dw, total_dw - main_dw, l2);
  } else {
    char l1[64];
    _snprintf_s(l1, sizeof(l1), _TRUNCATE, "%s all 0..%llu", step, (unsigned long long)g_blob_size);
    hist(g_blob, total_dw, l1);
  }
}

static IDXGIAdapter1* find_helios(IDXGIFactory1* factory, LUID* out_luid) {
  IDXGIAdapter1* adapter = nullptr;
  for (UINT i = 0; factory->EnumAdapters1(i, &adapter) != DXGI_ERROR_NOT_FOUND; ++i) {
    DXGI_ADAPTER_DESC1 desc{};
    adapter->GetDesc1(&desc);
    if (wcsstr(desc.Description, L"Helios")) {
      *out_luid = desc.AdapterLuid;
      return adapter;
    }
    adapter->Release();
    adapter = nullptr;
  }
  return nullptr;
}

static HRESULT create_device(IDXGIAdapter1* adapter, ID3D11Device** device,
                             ID3D11DeviceContext** ctx) {
  const D3D_FEATURE_LEVEL levels[] = {
      D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0,
      D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_10_0,
  };
  D3D_FEATURE_LEVEL fl{};
  return D3D11CreateDevice(adapter, D3D_DRIVER_TYPE_UNKNOWN, nullptr,
                           D3D11_CREATE_DEVICE_BGRA_SUPPORT, levels,
                           _countof(levels), D3D11_SDK_VERSION, device, &fl, ctx);
}

int main() {
  IDXGIFactory1* factory = nullptr;
  HRESULT hr = CreateDXGIFactory1(__uuidof(IDXGIFactory1),
                                  reinterpret_cast<void**>(&factory));
  if (FAILED(hr)) { printf("CreateDXGIFactory1 hr=0x%08x\n", (unsigned)hr); return 1; }

  LUID luid{};
  IDXGIAdapter1* helios = find_helios(factory, &luid);
  if (!helios) { printf("Helios adapter not found\n"); return 2; }
  printf("helios luid=%08x:%08x\n", (unsigned)luid.HighPart, (unsigned)luid.LowPart);

  ID3D11Device* dev1 = nullptr;  ID3D11DeviceContext* ctx1 = nullptr;
  ID3D11Device* dev2 = nullptr;  ID3D11DeviceContext* ctx2 = nullptr;
  hr = create_device(helios, &dev1, &ctx1);
  printf("dev1 create hr=0x%08x\n", (unsigned)hr);
  if (FAILED(hr)) return 3;
  hr = create_device(helios, &dev2, &ctx2);
  printf("dev2 create hr=0x%08x\n", (unsigned)hr);
  if (FAILED(hr)) return 4;

  D3D11_TEXTURE2D_DESC td{};
  td.Width = 256; td.Height = 256; td.MipLevels = 1; td.ArraySize = 1;
  td.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
  td.SampleDesc.Count = 1;
  td.Usage = D3D11_USAGE_DEFAULT;
  td.BindFlags = D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE;
  td.MiscFlags = D3D11_RESOURCE_MISC_SHARED_NTHANDLE | D3D11_RESOURCE_MISC_SHARED;

  ID3D11Texture2D* tex = nullptr;
  hr = dev1->CreateTexture2D(&td, nullptr, &tex);
  printf("CreateTexture2D(shared RT) hr=0x%08x\n", (unsigned)hr);
  if (FAILED(hr)) return 5;

  ID3D11RenderTargetView* rtv = nullptr;
  hr = dev1->CreateRenderTargetView(tex, nullptr, &rtv);
  if (FAILED(hr)) return 6;
  const float color[4] = { 0.25f, 0.50f, 0.75f, 1.00f }; // raw dword 0xff407fbf
  ctx1->ClearRenderTargetView(rtv, color);
  ctx1->Flush();

  IDXGIResource1* res1 = nullptr;
  hr = tex->QueryInterface(__uuidof(IDXGIResource1), reinterpret_cast<void**>(&res1));
  if (FAILED(hr)) return 7;
  HANDLE handle = nullptr;
  hr = res1->CreateSharedHandle(nullptr,
                                DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE,
                                nullptr, &handle);
  printf("CreateSharedHandle hr=0x%08x\n", (unsigned)hr);
  if (FAILED(hr) || !handle) return 8;

  ID3D11Device1* dev2_1 = nullptr;
  hr = dev2->QueryInterface(__uuidof(ID3D11Device1), reinterpret_cast<void**>(&dev2_1));
  if (FAILED(hr)) return 9;
  ID3D11Texture2D* opened = nullptr;
  hr = dev2_1->OpenSharedResource1(handle, __uuidof(ID3D11Texture2D),
                                   reinterpret_cast<void**>(&opened));
  printf("OpenSharedResource1 hr=0x%08x\n", (unsigned)hr);
  if (FAILED(hr) || !opened) return 10;

  // ── the new part: map the blob raw ────────────────────────────────────────
  // --nomap: control run — skip the KMT open + MAP_BLOB attempt entirely, so
  // the only difference vs d3d11_shared_content_probe is the readback ORDER
  // (producer-side readback before the consumer's). Discriminates whether the
  // clears-propagate flip comes from the resolve forced by the producer
  // readback (lazy fast-clear model) or from a side effect of the host
  // RESOURCE_MAP_BLOB attempt.
  bool nomap = false;
  for (int ai = 1; ai < __argc; ++ai)
    if (strcmp(__argv[ai], "--nomap") == 0) nomap = true;
  UINT res_id = 0; UINT64 alloc_size = 0;
  if (nomap) {
    printf("--nomap: skipping KMT open + MAP_BLOB (control run)\n");
  } else if (parse_identity(&res_id, &alloc_size)) {
    printf("IDENTITY PARSE FAILED (umd-%lu.log) — raw dumps unavailable\n",
           GetCurrentProcessId());
  } else {
    printf("identity res_id=%u alloc_size=%llu\n", res_id, (unsigned long long)alloc_size);
    if (!open_helios_kmt(luid)) {
      struct helios_escape_map_blob mb;
      memset(&mb, 0, sizeof(mb));
      mb.hdr.magic = HELIOS_ESCAPE_MAGIC;
      mb.hdr.cmd_type = HELIOS_ESCAPE_MAP_BLOB;
      mb.hdr.version = HELIOS_ESCAPE_VERSION;
      mb.hdr.size = sizeof(mb);
      mb.resource_id = res_id;
      if (!escape(&mb, sizeof(mb)) && mb.out_user_va) {
        g_blob = (volatile unsigned*)(UINT_PTR)mb.out_user_va;
        g_blob_size = alloc_size;
        printf("MAP_BLOB ok user_va=0x%llx map_cache=%u\n",
               (unsigned long long)mb.out_user_va, mb.map_cache);
      } else {
        printf("MAP_BLOB failed — raw dumps unavailable\n");
      }
    }
  }

  auto readback = [&](ID3D11Device* dev, ID3D11DeviceContext* ctx,
                      ID3D11Texture2D* src, const char* label) -> unsigned {
    D3D11_TEXTURE2D_DESC sd = td;
    sd.BindFlags = 0; sd.MiscFlags = 0;
    sd.Usage = D3D11_USAGE_STAGING;
    sd.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
    ID3D11Texture2D* staging = nullptr;
    HRESULT h = dev->CreateTexture2D(&sd, nullptr, &staging);
    if (FAILED(h)) { printf("[%s] staging create hr=0x%08x\n", label, (unsigned)h); return 0xEEEEEEEE; }
    ctx->CopyResource(staging, src);
    D3D11_MAPPED_SUBRESOURCE map{};
    h = ctx->Map(staging, 0, D3D11_MAP_READ, 0, &map);
    if (FAILED(h)) { printf("[%s] map hr=0x%08x\n", label, (unsigned)h); staging->Release(); return 0xEEEEEEEE; }
    const unsigned char* c =
        reinterpret_cast<const unsigned char*>(map.pData) + 128 * map.RowPitch + 128 * 4;
    unsigned val = (unsigned)c[0] | ((unsigned)c[1] << 8) | ((unsigned)c[2] << 16) | ((unsigned)c[3] << 24);
    printf("[%s] center=0x%08x\n", label, val);
    ctx->Unmap(staging, 0);
    staging->Release();
    return val;
  };

  // (B) opener-side readback + raw truth after clear #1.
  readback(dev1, ctx1, tex, "A dev1 self");
  readback(dev2, ctx2, opened, "B dev2 opened");
  dump_blob("B(after clear#1)");

  // (C) re-clear on dev1 (expect divergence), raw truth: did clear #2 reach memory?
  const float color2[4] = { 1.00f, 0.25f, 0.50f, 1.00f }; // raw dword 0xffff407f
  ctx1->ClearRenderTargetView(rtv, color2);
  ctx1->Flush();
  Sleep(3000);
  readback(dev1, ctx1, tex, "C dev1 self (expect clear#2)");
  readback(dev2, ctx2, opened, "C dev2 opened (diverges: clear#1)");
  dump_blob("C(after clear#2)");

  // (D) dev2 clears through the alias, raw truth again.
  ID3D11RenderTargetView* rtv2 = nullptr;
  hr = dev2->CreateRenderTargetView(opened, nullptr, &rtv2);
  if (SUCCEEDED(hr)) {
    const float color3[4] = { 0.50f, 1.00f, 0.25f, 1.00f }; // raw dword 0xff7fff40
    ctx2->ClearRenderTargetView(rtv2, color3);
    ctx2->Flush();
    Sleep(3000);
    readback(dev2, ctx2, opened, "D dev2 self (expect clear#3)");
    readback(dev1, ctx1, tex, "D dev1 (diverges: clear#2)");
    dump_blob("D(after dev2 clear#3)");
  }

  // (E) copy-engine write, raw truth: pattern dword 0xff332211.
  {
    static unsigned char pattern[256 * 256 * 4];
    for (size_t i = 0; i < sizeof(pattern); i += 4) {
      pattern[i + 0] = 0x11; pattern[i + 1] = 0x22;
      pattern[i + 2] = 0x33; pattern[i + 3] = 0xFF;
    }
    ctx1->UpdateSubresource(tex, 0, nullptr, pattern, 256 * 4, 0);
    ctx1->Flush();
    Sleep(2000);
    readback(dev1, ctx1, tex, "E0 dev1 self (pattern)");
    readback(dev2, ctx2, opened, "E1 dev2 (propagates: pattern)");
    dump_blob("E(after UpdateSubresource)");
  }

  printf("DONE (colors: c1=0xff407fbf c2=0xffff407f c3=0xff7fff40 pat=0xff332211)\n");
  return 0;
}
