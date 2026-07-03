#include <windows.h>
#include <stdio.h>

static void print_counts(UINT32 flags) {
  UINT32 paths = 0;
  UINT32 modes = 0;
  LONG ret = GetDisplayConfigBufferSizes(flags, &paths, &modes);
  printf("GetDisplayConfigBufferSizes flags=0x%x ret=%ld paths=%u modes=%u gle=%lu\n",
    flags, ret, paths, modes, GetLastError());
}

int wmain(int argc, wchar_t** argv) {
  printf("display_config_probe pid=%lu session-check\n", GetCurrentProcessId());
  print_counts(QDC_ONLY_ACTIVE_PATHS);
  print_counts(QDC_ALL_PATHS);
  print_counts(QDC_DATABASE_CURRENT);

  if (argc > 1 && wcscmp(argv[1], L"extend") == 0) {
    LONG ret = SetDisplayConfig(0, nullptr, 0, nullptr, SDC_APPLY | SDC_TOPOLOGY_EXTEND);
    printf("SetDisplayConfig EXTEND ret=%ld gle=%lu\n", ret, GetLastError());
    print_counts(QDC_ONLY_ACTIVE_PATHS);
    print_counts(QDC_ALL_PATHS);
    print_counts(QDC_DATABASE_CURRENT);
  }

  return 0;
}
