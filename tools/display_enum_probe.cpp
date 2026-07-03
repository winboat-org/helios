#include <windows.h>
#include <stdio.h>

static void print_display_settings(const wchar_t* name) {
  DEVMODEW dm = {};
  dm.dmSize = sizeof(dm);
  if (EnumDisplaySettingsW(name, ENUM_CURRENT_SETTINGS, &dm)) {
    wprintf(L"  current %ux%u %ubpp %uHz pos=(%ld,%ld) flags=0x%lx\n",
            dm.dmPelsWidth, dm.dmPelsHeight, dm.dmBitsPerPel,
            dm.dmDisplayFrequency, dm.dmPosition.x, dm.dmPosition.y,
            dm.dmDisplayFlags);
  } else {
    wprintf(L"  EnumDisplaySettings current failed gle=%lu\n", GetLastError());
  }
}

int wmain() {
  for (DWORD i = 0;; ++i) {
    DISPLAY_DEVICEW dd = {};
    dd.cb = sizeof(dd);
    if (!EnumDisplayDevicesW(nullptr, i, &dd, 0))
      break;

    wprintf(L"adapter[%lu] name=%ls string=%ls state=0x%08lx id=%ls key=%ls\n",
            i, dd.DeviceName, dd.DeviceString, dd.StateFlags, dd.DeviceID,
            dd.DeviceKey);
    print_display_settings(dd.DeviceName);
    for (DWORD m = 0; m < 80; ++m) {
      DEVMODEW mode = {};
      mode.dmSize = sizeof(mode);
      if (!EnumDisplaySettingsW(dd.DeviceName, m, &mode)) {
        wprintf(L"  mode[%lu] unavailable gle=%lu\n", m, GetLastError());
        break;
      }
      wprintf(L"  mode[%lu] %ux%u %ubpp %uHz fields=0x%lx\n",
              m, mode.dmPelsWidth, mode.dmPelsHeight, mode.dmBitsPerPel,
              mode.dmDisplayFrequency, mode.dmFields);
    }

    for (DWORD j = 0;; ++j) {
      DISPLAY_DEVICEW mon = {};
      mon.cb = sizeof(mon);
      if (!EnumDisplayDevicesW(dd.DeviceName, j, &mon, 0))
        break;
      wprintf(L"  monitor[%lu] name=%ls string=%ls state=0x%08lx id=%ls key=%ls\n",
              j, mon.DeviceName, mon.DeviceString, mon.StateFlags,
              mon.DeviceID, mon.DeviceKey);
    }
  }
  return 0;
}
