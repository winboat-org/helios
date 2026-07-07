#include <windows.h>
#include <d3dkmthk.h>

#include <cstdio>
#include <cwchar>
#include <cstdlib>

static void dump_for_display(const wchar_t* name)
{
  D3DKMT_OPENADAPTERFROMGDIDISPLAYNAME open = {};
  wcsncpy_s(open.DeviceName, name, _TRUNCATE);
  NTSTATUS st = D3DKMTOpenAdapterFromGdiDisplayName(&open);
  wprintf(L"OpenAdapterFromGdiDisplayName(%ls) st=0x%08x h=0x%08x luid=%08x:%08x source=%u\n",
    name, (unsigned)st, open.hAdapter, open.AdapterLuid.HighPart,
    open.AdapterLuid.LowPart, open.VidPnSourceId);
  if (st != 0)
    return;

  D3DKMT_GETDISPLAYMODELIST get = {};
  get.hAdapter = open.hAdapter;
  get.VidPnSourceId = open.VidPnSourceId;
  st = D3DKMTGetDisplayModeList(&get);
  printf("  GetDisplayModeList count st=0x%08x count=%u\n", (unsigned)st, get.ModeCount);

  if (get.ModeCount) {
    D3DKMT_DISPLAYMODE* modes = (D3DKMT_DISPLAYMODE*)calloc(get.ModeCount, sizeof(D3DKMT_DISPLAYMODE));
    if (modes) {
      get.pModeList = modes;
      st = D3DKMTGetDisplayModeList(&get);
      printf("  GetDisplayModeList fill st=0x%08x count=%u\n", (unsigned)st, get.ModeCount);
      for (UINT i = 0; st == 0 && i < get.ModeCount && i < 32; ++i) {
        const D3DKMT_DISPLAYMODE& m = modes[i];
        UINT flags = 0;
        memcpy(&flags, &m.Flags, sizeof(flags));
        printf("    [%u] %ux%u fmt=%u intHz=%u rat=%u/%u scan=%u rot=%u fixed=%u flags=0x%08x\n",
          i, m.Width, m.Height, (unsigned)m.Format, m.IntegerRefreshRate,
          m.RefreshRate.Numerator, m.RefreshRate.Denominator,
          (unsigned)m.ScanLineOrdering, (unsigned)m.DisplayOrientation,
          m.DisplayFixedOutput, flags);
      }
      free(modes);
    }
  }

  D3DKMT_CLOSEADAPTER close = {};
  close.hAdapter = open.hAdapter;
  D3DKMTCloseAdapter(&close);
}

int wmain()
{
  for (DWORD i = 0;; ++i) {
    DISPLAY_DEVICEW dd = {};
    dd.cb = sizeof(dd);
    if (!EnumDisplayDevicesW(nullptr, i, &dd, 0))
      break;
    wprintf(L"EnumDisplayDevices[%lu] %ls state=0x%08lx string=%ls\n",
      i, dd.DeviceName, dd.StateFlags, dd.DeviceString);
    dump_for_display(dd.DeviceName);
  }
  return 0;
}
