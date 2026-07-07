// ccd_adapter_probe.cpp — map the active desktop display path(s) to the
// source/target adapter LUID + GDI device name via QueryDisplayConfig, and
// cross-list every D3DKMT adapter LUID via EnumAdapters2. Answers "which
// adapter owns the visible output" vs "which LUIDs are phantom render nodes".
//   g++ -O2 -o ccd_adapter_probe.exe ccd_adapter_probe.cpp -lgdi32 -luser32
#include <windows.h>
#include <cstdio>

static void dump(UINT32 flags, const char* label) {
  printf("==== %s (flags=0x%x) ====\n", label, flags);
  UINT32 nPath = 0, nMode = 0;
  LONG r = GetDisplayConfigBufferSizes(flags, &nPath, &nMode);
  printf("GetDisplayConfigBufferSizes r=%ld paths=%u modes=%u\n", r, nPath, nMode);
  if (r != ERROR_SUCCESS || nPath == 0) { printf("\n"); return; }

  DISPLAYCONFIG_PATH_INFO* paths = new DISPLAYCONFIG_PATH_INFO[nPath];
  DISPLAYCONFIG_MODE_INFO* modes = new DISPLAYCONFIG_MODE_INFO[nMode];
  r = QueryDisplayConfig(flags, &nPath, paths, &nMode, modes, nullptr);
  printf("QueryDisplayConfig r=%ld paths=%u modes=%u\n\n", r, nPath, nMode);
  if (r != ERROR_SUCCESS) { delete[] paths; delete[] modes; return; }

  for (UINT32 i = 0; i < nPath; ++i) {
    const auto& p = paths[i];
    bool active = (p.flags & DISPLAYCONFIG_PATH_ACTIVE) != 0;
    printf("path[%u] active=%d\n", i, active);
    printf("  SOURCE adapterLuid=%08lx:%08lx id=%u\n",
           (unsigned long)p.sourceInfo.adapterId.HighPart,
           (unsigned long)p.sourceInfo.adapterId.LowPart, p.sourceInfo.id);
    printf("  TARGET adapterLuid=%08lx:%08lx id=%u\n",
           (unsigned long)p.targetInfo.adapterId.HighPart,
           (unsigned long)p.targetInfo.adapterId.LowPart, p.targetInfo.id);

    // Resolve source GDI device name (e.g. \\.\DISPLAY6).
    DISPLAYCONFIG_SOURCE_DEVICE_NAME sn = {};
    sn.header.type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
    sn.header.size = sizeof(sn);
    sn.header.adapterId = p.sourceInfo.adapterId;
    sn.header.id = p.sourceInfo.id;
    if (DisplayConfigGetDeviceInfo(&sn.header) == ERROR_SUCCESS)
      wprintf(L"  source GDI name=%ls\n", sn.viewGdiDeviceName);

    // Resolve target friendly name.
    DISPLAYCONFIG_TARGET_DEVICE_NAME tn = {};
    tn.header.type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
    tn.header.size = sizeof(tn);
    tn.header.adapterId = p.targetInfo.adapterId;
    tn.header.id = p.targetInfo.id;
    if (DisplayConfigGetDeviceInfo(&tn.header) == ERROR_SUCCESS)
      wprintf(L"  target friendly=%ls\n", tn.monitorFriendlyDeviceName);
    printf("\n");
  }
  delete[] paths; delete[] modes;
}

int main() {
  dump(QDC_ONLY_ACTIVE_PATHS, "ONLY_ACTIVE_PATHS");
  dump(QDC_ALL_PATHS, "ALL_PATHS");
  dump(QDC_ONLY_ACTIVE_PATHS | QDC_VIRTUAL_MODE_AWARE, "ACTIVE|VIRTUAL_MODE_AWARE");
  return 0;
}
