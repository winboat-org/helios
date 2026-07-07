#include <dxgi1_6.h>
#include <windows.h>

#include <cstdio>

static const char *fmt_name(DXGI_FORMAT fmt) {
  switch (fmt) {
  case DXGI_FORMAT_R8G8B8A8_UNORM: return "R8G8B8A8_UNORM";
  case DXGI_FORMAT_B8G8R8A8_UNORM: return "B8G8R8A8_UNORM";
  case DXGI_FORMAT_R10G10B10A2_UNORM: return "R10G10B10A2_UNORM";
  case DXGI_FORMAT_R16G16B16A16_FLOAT: return "R16G16B16A16_FLOAT";
  default: return "unknown";
  }
}

static void dump_modes(IDXGIOutput *out, DXGI_FORMAT fmt, UINT flags) {
  UINT count = 0;
  HRESULT hr = out->GetDisplayModeList(fmt, flags, &count, nullptr);
  std::printf("    GetDisplayModeList fmt=%s flags=0x%x count hr=0x%08lx count=%u\n",
      fmt_name(fmt), flags, (unsigned long)hr, count);
  if (FAILED(hr) || count == 0)
    return;

  DXGI_MODE_DESC *modes = new DXGI_MODE_DESC[count];
  UINT count2 = count;
  hr = out->GetDisplayModeList(fmt, flags, &count2, modes);
  std::printf("      fill hr=0x%08lx count=%u\n", (unsigned long)hr, count2);
  for (UINT i = 0; SUCCEEDED(hr) && i < count2 && i < 12; ++i) {
    const DXGI_MODE_DESC &m = modes[i];
    std::printf("      [%u] %ux%u %.3fHz fmt=%u scan=%u scale=%u\n", i,
        m.Width, m.Height,
        m.RefreshRate.Denominator ? (double)m.RefreshRate.Numerator / m.RefreshRate.Denominator : 0.0,
        (unsigned)m.Format, (unsigned)m.ScanlineOrdering, (unsigned)m.Scaling);
  }
  delete[] modes;
}

static void dump_modes1(IDXGIOutput *out, DXGI_FORMAT fmt, UINT flags) {
  IDXGIOutput1 *out1 = nullptr;
  HRESULT qhr = out->QueryInterface(__uuidof(IDXGIOutput1), (void **)&out1);
  std::printf("    QI IDXGIOutput1 hr=0x%08lx out1=%p\n", (unsigned long)qhr, out1);
  if (FAILED(qhr) || !out1)
    return;

  UINT count = 0;
  HRESULT hr = out1->GetDisplayModeList1(fmt, flags, &count, nullptr);
  std::printf("    GetDisplayModeList1 fmt=%s flags=0x%x count hr=0x%08lx count=%u\n",
      fmt_name(fmt), flags, (unsigned long)hr, count);
  if (SUCCEEDED(hr) && count) {
    DXGI_MODE_DESC1 *modes = new DXGI_MODE_DESC1[count];
    UINT count2 = count;
    hr = out1->GetDisplayModeList1(fmt, flags, &count2, modes);
    std::printf("      fill1 hr=0x%08lx count=%u\n", (unsigned long)hr, count2);
    for (UINT i = 0; SUCCEEDED(hr) && i < count2 && i < 12; ++i) {
      const DXGI_MODE_DESC1 &m = modes[i];
      std::printf("      1[%u] %ux%u %.3fHz fmt=%u scan=%u scale=%u stereo=%u\n", i,
          m.Width, m.Height,
          m.RefreshRate.Denominator ? (double)m.RefreshRate.Numerator / m.RefreshRate.Denominator : 0.0,
          (unsigned)m.Format, (unsigned)m.ScanlineOrdering, (unsigned)m.Scaling, (unsigned)m.Stereo);
    }
    delete[] modes;
  }
  out1->Release();
}

int main() {
  std::printf("dxgi_output_modes_probe pid=%lu session=%lu\n",
      GetCurrentProcessId(), WTSGetActiveConsoleSessionId());

  IDXGIFactory1 *factory = nullptr;
  HRESULT hr = CreateDXGIFactory1(__uuidof(IDXGIFactory1), (void **)&factory);
  std::printf("CreateDXGIFactory1 hr=0x%08lx factory=%p\n", (unsigned long)hr, factory);
  if (FAILED(hr))
    return 1;

  for (UINT ai = 0;; ++ai) {
    IDXGIAdapter1 *adapter = nullptr;
    hr = factory->EnumAdapters1(ai, &adapter);
    if (hr == DXGI_ERROR_NOT_FOUND)
      break;
    std::printf("Adapter[%u] EnumAdapters1 hr=0x%08lx adapter=%p\n",
        ai, (unsigned long)hr, adapter);
    if (FAILED(hr))
      break;

    DXGI_ADAPTER_DESC1 ad = {};
    adapter->GetDesc1(&ad);
    wprintf(L"  desc='%ls' vendor=0x%04x device=0x%04x luid=%08lx:%08lx flags=0x%x\n",
        ad.Description, ad.VendorId, ad.DeviceId,
        (unsigned long)ad.AdapterLuid.HighPart, (unsigned long)ad.AdapterLuid.LowPart,
        (unsigned)ad.Flags);

    for (UINT oi = 0;; ++oi) {
      IDXGIOutput *out = nullptr;
      hr = adapter->EnumOutputs(oi, &out);
      if (hr == DXGI_ERROR_NOT_FOUND)
        break;
      std::printf("  Output[%u] EnumOutputs hr=0x%08lx output=%p\n",
          oi, (unsigned long)hr, out);
      if (FAILED(hr))
        break;

      DXGI_OUTPUT_DESC od = {};
      hr = out->GetDesc(&od);
      wprintf(L"    GetDesc hr=0x%08lx name='%ls' attached=%u desktop=(%ld,%ld)-(%ld,%ld)\n",
          (unsigned long)hr, od.DeviceName, od.AttachedToDesktop,
          od.DesktopCoordinates.left, od.DesktopCoordinates.top,
          od.DesktopCoordinates.right, od.DesktopCoordinates.bottom);

      dump_modes(out, DXGI_FORMAT_R8G8B8A8_UNORM, 0);
      dump_modes(out, DXGI_FORMAT_B8G8R8A8_UNORM, 0);
      dump_modes(out, DXGI_FORMAT_R10G10B10A2_UNORM, 0);
      dump_modes(out, DXGI_FORMAT_R16G16B16A16_FLOAT, 0);
      dump_modes(out, DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_ENUM_MODES_INTERLACED);
      dump_modes1(out, DXGI_FORMAT_B8G8R8A8_UNORM, 0);
      out->Release();
    }
    adapter->Release();
  }
  factory->Release();
  return 0;
}
