// Verify CPU->GPU texture-upload integrity on the Helios D3D11 UMD.
//
// Motivation (14th session): every SHARED surface dwm imports is probe-verified
// to carry real content, yet dwm's own background/wallpaper brush composes
// BLACK (wallpaper, solid-color mode, and mica backdrops all black). dwm
// renders that brush from an internally-uploaded texture, so the suspect is
// the plain (non-shared) upload path: WIC decode -> initial-data /
// UpdateSubresource / dynamic-map upload -> sample. Small uploads demonstrably
// work (glyph atlases - text renders); this probe checks whether LARGE uploads
// corrupt or zero, across the three upload methods and a range of sizes.
//
// Pattern is position-dependent (B=x, G=y, R=x^y, A=0xFF) so zeroing, shear,
// tiling and offset bugs all show as mismatches with distinct signatures.
//
// Build:
//   cl /EHsc /W4 d3d11_upload_integrity_probe.cpp /link dxgi.lib d3d11.lib

#include <d3d11_4.h>
#include <dxgi1_6.h>
#include <cstdio>
#include <cwchar>
#include <cstdint>
#include <vector>

static IDXGIAdapter1* find_helios(IDXGIFactory1* factory) {
  IDXGIAdapter1* adapter = nullptr;
  for (UINT i = 0; factory->EnumAdapters1(i, &adapter) != DXGI_ERROR_NOT_FOUND; ++i) {
    DXGI_ADAPTER_DESC1 desc{};
    adapter->GetDesc1(&desc);
    if (wcsstr(desc.Description, L"Helios"))
      return adapter;
    adapter->Release();
    adapter = nullptr;
  }
  return nullptr;
}

static void fill_pattern(std::vector<std::uint8_t>& buf, UINT w, UINT h, UINT pitch) {
  for (UINT y = 0; y < h; ++y) {
    std::uint8_t* row = buf.data() + std::size_t(y) * pitch;
    for (UINT x = 0; x < w; ++x) {
      row[x * 4 + 0] = std::uint8_t(x);        // B
      row[x * 4 + 1] = std::uint8_t(y);        // G
      row[x * 4 + 2] = std::uint8_t(x ^ y);    // R
      row[x * 4 + 3] = 0xFF;                   // A
    }
  }
}

// Returns mismatch count; logs the first few mismatches.
static std::size_t verify(const std::uint8_t* data, UINT mapPitch,
                          UINT w, UINT h, const char* tag) {
  std::size_t mismatches = 0, logged = 0, zeroBytes = 0;
  for (UINT y = 0; y < h; ++y) {
    const std::uint8_t* row = data + std::size_t(y) * mapPitch;
    for (UINT x = 0; x < w; ++x) {
      const std::uint8_t exp[4] = {
        std::uint8_t(x), std::uint8_t(y), std::uint8_t(x ^ y), 0xFF };
      for (int c = 0; c < 4; ++c) {
        std::uint8_t got = row[x * 4 + c];
        if (got == 0) ++zeroBytes;
        if (got != exp[c]) {
          ++mismatches;
          if (logged < 4) {
            printf("  %s MISMATCH at x=%u y=%u c=%d exp=%02x got=%02x\n",
                   tag, x, y, c, exp[c], got);
            ++logged;
          }
        }
      }
    }
  }
  if (mismatches)
    printf("  %s zeroBytes=%zu of %zu\n", tag, zeroBytes, std::size_t(w) * h * 4);
  return mismatches;
}

static int run_case(ID3D11Device* dev, ID3D11DeviceContext* ctx,
                    UINT w, UINT h, UINT bind, const char* method) {
  const UINT pitch = w * 4;
  std::vector<std::uint8_t> pattern(std::size_t(pitch) * h);
  fill_pattern(pattern, w, h, pitch);

  D3D11_TEXTURE2D_DESC desc{};
  desc.Width = w;
  desc.Height = h;
  desc.MipLevels = 1;
  desc.ArraySize = 1;
  desc.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
  desc.SampleDesc.Count = 1;
  desc.Usage = D3D11_USAGE_DEFAULT;
  desc.BindFlags = bind;

  ID3D11Texture2D* tex = nullptr;
  HRESULT hr = S_OK;

  if (strcmp(method, "initdata") == 0) {
    D3D11_SUBRESOURCE_DATA init{};
    init.pSysMem = pattern.data();
    init.SysMemPitch = pitch;
    hr = dev->CreateTexture2D(&desc, &init, &tex);
  } else if (strcmp(method, "updatesub") == 0) {
    hr = dev->CreateTexture2D(&desc, nullptr, &tex);
    if (SUCCEEDED(hr) && tex)
      ctx->UpdateSubresource(tex, 0, nullptr, pattern.data(), pitch, 0);
  } else { // dynamic: DYNAMIC texture mapped WRITE_DISCARD, then copied to DEFAULT
    D3D11_TEXTURE2D_DESC dyn = desc;
    dyn.Usage = D3D11_USAGE_DYNAMIC;
    dyn.BindFlags = D3D11_BIND_SHADER_RESOURCE;
    dyn.CPUAccessFlags = D3D11_CPU_ACCESS_WRITE;

    ID3D11Texture2D* dtex = nullptr;
    hr = dev->CreateTexture2D(&dyn, nullptr, &dtex);
    if (FAILED(hr) || !dtex) {
      printf("%-9s %4ux%-4u bind=0x%02x CreateTexture2D(dyn) hr=0x%08x FAIL\n",
             method, w, h, bind, (unsigned)hr);
      return 1;
    }
    D3D11_MAPPED_SUBRESOURCE m{};
    hr = ctx->Map(dtex, 0, D3D11_MAP_WRITE_DISCARD, 0, &m);
    if (FAILED(hr) || !m.pData) {
      printf("%-9s %4ux%-4u bind=0x%02x Map(dyn) hr=0x%08x FAIL\n",
             method, w, h, bind, (unsigned)hr);
      dtex->Release();
      return 1;
    }
    printf("%-9s %4ux%-4u Map(dyn WRITE_DISCARD) rowPitch=%u\n",
           method, w, h, m.RowPitch);
    for (UINT y = 0; y < h; ++y)
      memcpy(static_cast<std::uint8_t*>(m.pData) + std::size_t(y) * m.RowPitch,
             pattern.data() + std::size_t(y) * pitch, pitch);
    ctx->Unmap(dtex, 0);

    hr = dev->CreateTexture2D(&desc, nullptr, &tex);
    if (SUCCEEDED(hr) && tex)
      ctx->CopyResource(tex, dtex);
    dtex->Release();
  }

  if (FAILED(hr) || !tex) {
    printf("%-9s %4ux%-4u bind=0x%02x create/upload hr=0x%08x FAIL\n",
           method, w, h, bind, (unsigned)hr);
    return 1;
  }

  D3D11_TEXTURE2D_DESC st = desc;
  st.BindFlags = 0;
  st.MiscFlags = 0;
  st.Usage = D3D11_USAGE_STAGING;
  st.CPUAccessFlags = D3D11_CPU_ACCESS_READ;

  ID3D11Texture2D* staging = nullptr;
  hr = dev->CreateTexture2D(&st, nullptr, &staging);
  if (FAILED(hr) || !staging) {
    printf("%-9s %4ux%-4u bind=0x%02x staging create hr=0x%08x FAIL\n",
           method, w, h, bind, (unsigned)hr);
    tex->Release();
    return 1;
  }

  ctx->CopyResource(staging, tex);
  ctx->Flush();

  D3D11_MAPPED_SUBRESOURCE mapped{};
  hr = ctx->Map(staging, 0, D3D11_MAP_READ, 0, &mapped);
  int rc = 1;
  if (SUCCEEDED(hr) && mapped.pData) {
    std::size_t bad = verify(static_cast<const std::uint8_t*>(mapped.pData),
                             mapped.RowPitch, w, h, method);
    printf("%-9s %4ux%-4u bind=0x%02x mismatches=%zu %s\n",
           method, w, h, bind, bad, bad ? "FAIL" : "PASS");
    rc = bad ? 1 : 0;
    ctx->Unmap(staging, 0);
  } else {
    printf("%-9s %4ux%-4u bind=0x%02x Map(read) hr=0x%08x FAIL\n",
           method, w, h, bind, (unsigned)hr);
  }

  staging->Release();
  tex->Release();
  return rc;
}

int main() {
  IDXGIFactory1* factory = nullptr;
  HRESULT hr = CreateDXGIFactory1(__uuidof(IDXGIFactory1),
                                  reinterpret_cast<void**>(&factory));
  if (FAILED(hr)) {
    printf("CreateDXGIFactory1 hr=0x%08x\n", (unsigned)hr);
    return 1;
  }

  IDXGIAdapter1* helios = find_helios(factory);
  if (!helios) {
    printf("Helios adapter not found\n");
    factory->Release();
    return 2;
  }

  const D3D_FEATURE_LEVEL levels[] = {
      D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0,
      D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_10_0 };
  ID3D11Device* device = nullptr;
  ID3D11DeviceContext* context = nullptr;
  D3D_FEATURE_LEVEL fl{};
  hr = D3D11CreateDevice(helios, D3D_DRIVER_TYPE_UNKNOWN, nullptr,
                         D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                         levels, _countof(levels), D3D11_SDK_VERSION,
                         &device, &fl, &context);
  printf("D3D11CreateDevice hr=0x%08x fl=0x%04x\n", (unsigned)hr, (unsigned)fl);
  if (FAILED(hr) || !device || !context)
    return 3;

  struct { UINT w, h; } sizes[] = {
    { 64, 64 }, { 256, 256 }, { 1024, 576 }, { 1896, 1030 }, { 1920, 1080 } };
  const char* methods[] = { "initdata", "updatesub", "dynamic" };
  // SRV-only mirrors dwm's wallpaper brush texture; RT|SRV mirrors render
  // targets (different allocation/usage decisions in the UMD).
  UINT binds[] = { D3D11_BIND_SHADER_RESOURCE,
                   D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE };

  int failures = 0;
  for (auto& s : sizes)
    for (auto* m : methods)
      for (UINT b : binds)
        failures += run_case(device, context, s.w, s.h, b, m);

  printf("TOTAL failures=%d\n", failures);

  context->Release();
  device->Release();
  helios->Release();
  factory->Release();
  return failures ? 10 : 0;
}
