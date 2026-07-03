#include <windows.h>
#include <stdio.h>

static void dump_adapter(const wchar_t* name) {
  DISPLAY_DEVICEW dd = {};
  dd.cb = sizeof(dd);
  for (DWORD i = 0; EnumDisplayDevicesW(nullptr, i, &dd, 0); ++i) {
    if (wcscmp(dd.DeviceName, name) == 0) {
      wprintf(L"%ls state=0x%08lx string=%ls id=%ls\n",
              dd.DeviceName, dd.StateFlags, dd.DeviceString, dd.DeviceID);
      break;
    }
    dd = {};
    dd.cb = sizeof(dd);
  }

  DEVMODEW cur = {};
  cur.dmSize = sizeof(cur);
  if (EnumDisplaySettingsW(name, ENUM_CURRENT_SETTINGS, &cur)) {
    wprintf(L"  current %ux%u %ubpp %uHz pos=(%ld,%ld)\n",
            cur.dmPelsWidth, cur.dmPelsHeight, cur.dmBitsPerPel,
            cur.dmDisplayFrequency, cur.dmPosition.x, cur.dmPosition.y);
  } else {
    wprintf(L"  current unavailable gle=%lu\n", GetLastError());
  }
}

int wmain(int argc, wchar_t** argv) {
  const wchar_t* target = argc > 1 ? argv[1] : L"\\\\.\\DISPLAY7";
  DWORD width = argc > 2 ? wcstoul(argv[2], nullptr, 0) : 1920;
  DWORD height = argc > 3 ? wcstoul(argv[3], nullptr, 0) : 1080;
  DWORD hz = argc > 4 ? wcstoul(argv[4], nullptr, 0) : 60;

  wprintf(L"before:\n");
  dump_adapter(target);

  DEVMODEW dm = {};
  dm.dmSize = sizeof(dm);
  dm.dmFields = DM_POSITION | DM_PELSWIDTH | DM_PELSHEIGHT |
                DM_BITSPERPEL | DM_DISPLAYFREQUENCY;
  dm.dmPosition.x = 0;
  dm.dmPosition.y = 0;
  dm.dmPelsWidth = width;
  dm.dmPelsHeight = height;
  dm.dmBitsPerPel = 32;
  dm.dmDisplayFrequency = hz;

  LONG r1 = ChangeDisplaySettingsExW(
      target, &dm, nullptr, CDS_UPDATEREGISTRY | CDS_NORESET | CDS_SET_PRIMARY, nullptr);
  wprintf(L"ChangeDisplaySettingsEx target ret=%ld gle=%lu\n", r1, GetLastError());

  LONG r2 = ChangeDisplaySettingsExW(nullptr, nullptr, nullptr, 0, nullptr);
  wprintf(L"ChangeDisplaySettingsEx apply ret=%ld gle=%lu\n", r2, GetLastError());

  wprintf(L"after:\n");
  dump_adapter(target);
  return 0;
}
