#include <windows.h>
#include <stdio.h>

static void print_target_name(LUID adapter, UINT32 targetId) {
  DISPLAYCONFIG_TARGET_DEVICE_NAME name = {};
  name.header.type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
  name.header.size = sizeof(name);
  name.header.adapterId = adapter;
  name.header.id = targetId;
  LONG ret = DisplayConfigGetDeviceInfo(&name.header);
  wprintf(L"DisplayConfigGetDeviceInfo target ret=%ld gle=%lu flags=0x%x outputTech=%u monitor=%ls path=%ls\n",
          ret, GetLastError(), name.flags.value, name.outputTechnology,
          name.monitorFriendlyDeviceName, name.monitorDevicePath);
}

static void print_source_name(LUID adapter, UINT32 sourceId) {
  DISPLAYCONFIG_SOURCE_DEVICE_NAME name = {};
  name.header.type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
  name.header.size = sizeof(name);
  name.header.adapterId = adapter;
  name.header.id = sourceId;
  LONG ret = DisplayConfigGetDeviceInfo(&name.header);
  wprintf(L"DisplayConfigGetDeviceInfo source[%u] ret=%ld gle=%lu name=%ls\n",
          sourceId, ret, GetLastError(), name.viewGdiDeviceName);
}

static void fill_signal(DISPLAYCONFIG_VIDEO_SIGNAL_INFO& signal, UINT32 width, UINT32 height, UINT32 refresh) {
  signal.vSyncFreq.Numerator = refresh;
  signal.vSyncFreq.Denominator = 1;
  signal.hSyncFreq.Numerator = refresh * height;
  signal.hSyncFreq.Denominator = 1;
  signal.pixelRate = (UINT64)width * height * refresh;
  signal.totalSize.cx = width;
  signal.totalSize.cy = height;
  signal.activeSize.cx = width;
  signal.activeSize.cy = height;
  signal.scanLineOrdering = DISPLAYCONFIG_SCANLINE_ORDERING_PROGRESSIVE;
  signal.AdditionalSignalInfo.vSyncFreqDivider = 1;
  signal.AdditionalSignalInfo.videoStandard = 255;
}

static void try_supplied(LUID adapter, UINT32 sourceId, UINT32 targetId, UINT32 width, UINT32 height, UINT32 refresh,
                         bool virtualAware, bool includeTargetMode, bool saveToDb) {
  DISPLAYCONFIG_PATH_INFO path = {};
  path.sourceInfo.adapterId = adapter;
  path.sourceInfo.id = sourceId;
  path.sourceInfo.statusFlags = DISPLAYCONFIG_SOURCE_IN_USE;
  path.targetInfo.adapterId = adapter;
  path.targetInfo.id = targetId;
  path.targetInfo.outputTechnology = DISPLAYCONFIG_OUTPUT_TECHNOLOGY_HDMI;
  path.targetInfo.rotation = DISPLAYCONFIG_ROTATION_IDENTITY;
  path.targetInfo.scaling = DISPLAYCONFIG_SCALING_IDENTITY;
  path.targetInfo.refreshRate.Numerator = refresh;
  path.targetInfo.refreshRate.Denominator = 1;
  path.targetInfo.scanLineOrdering = DISPLAYCONFIG_SCANLINE_ORDERING_PROGRESSIVE;
  path.targetInfo.targetAvailable = TRUE;
  path.flags = DISPLAYCONFIG_PATH_ACTIVE;

  DISPLAYCONFIG_MODE_INFO modes[2] = {};
  modes[0].infoType = DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE;
  modes[0].id = sourceId;
  modes[0].adapterId = adapter;
  modes[0].sourceMode.width = width;
  modes[0].sourceMode.height = height;
  modes[0].sourceMode.pixelFormat = DISPLAYCONFIG_PIXELFORMAT_32BPP;
  modes[0].sourceMode.position.x = 0;
  modes[0].sourceMode.position.y = 0;

  modes[1].infoType = DISPLAYCONFIG_MODE_INFO_TYPE_TARGET;
  modes[1].id = targetId;
  modes[1].adapterId = adapter;
  fill_signal(modes[1].targetMode.targetVideoSignalInfo, width, height, refresh);

  UINT32 modeCount = includeTargetMode ? 2 : 1;
  if (virtualAware) {
    path.flags |= DISPLAYCONFIG_PATH_SUPPORT_VIRTUAL_MODE;
    path.sourceInfo.cloneGroupId = DISPLAYCONFIG_PATH_CLONE_GROUP_INVALID;
    path.sourceInfo.sourceModeInfoIdx = 0;
    path.targetInfo.desktopModeInfoIdx = DISPLAYCONFIG_PATH_DESKTOP_IMAGE_IDX_INVALID;
    path.targetInfo.targetModeInfoIdx = includeTargetMode ? 1 : DISPLAYCONFIG_PATH_TARGET_MODE_IDX_INVALID;
  } else {
    path.sourceInfo.modeInfoIdx = 0;
    path.targetInfo.modeInfoIdx = includeTargetMode ? 1 : DISPLAYCONFIG_PATH_MODE_IDX_INVALID;
  }

  UINT32 flags = SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_ALLOW_CHANGES;
  if (virtualAware)
    flags |= SDC_VIRTUAL_MODE_AWARE;
  if (saveToDb)
    flags |= SDC_SAVE_TO_DATABASE;

  SetLastError(0);
  LONG ret = SetDisplayConfig(1, &path, modeCount, modes, flags);
  printf("try source=%u virtual=%u targetMode=%u save=%u ret=%ld gle=%lu flags=0x%x pathFlags=0x%x\n",
         sourceId, virtualAware ? 1 : 0, includeTargetMode ? 1 : 0, saveToDb ? 1 : 0,
         ret, GetLastError(), flags, path.flags);
}

int wmain(int argc, wchar_t** argv) {
  LUID adapter = {};
  adapter.HighPart = argc > 1 ? wcstol(argv[1], nullptr, 0) : 0;
  adapter.LowPart = argc > 2 ? wcstoul(argv[2], nullptr, 16) : 0x18f818d0;
  UINT32 targetId = argc > 3 ? wcstoul(argv[3], nullptr, 0) : 256;
  UINT32 sourceId = argc > 4 ? wcstoul(argv[4], nullptr, 0) : 0;
  UINT32 width = argc > 5 ? wcstoul(argv[5], nullptr, 0) : 1920;
  UINT32 height = argc > 6 ? wcstoul(argv[6], nullptr, 0) : 1080;
  UINT32 refresh = argc > 7 ? wcstoul(argv[7], nullptr, 0) : 60;

  printf("adapter=%08lx:%08lx source=%u target=%u\n",
         adapter.HighPart, adapter.LowPart, sourceId, targetId);
  print_target_name(adapter, targetId);
  for (UINT32 sid = 0; sid < 8; ++sid)
    print_source_name(adapter, sid);

  for (UINT32 sid = sourceId; sid < sourceId + 8; ++sid) {
    try_supplied(adapter, sid, targetId, width, height, refresh, false, false, false);
    try_supplied(adapter, sid, targetId, width, height, refresh, false, true, false);
    try_supplied(adapter, sid, targetId, width, height, refresh, true, false, false);
    try_supplied(adapter, sid, targetId, width, height, refresh, true, true, false);
  }

  UINT32 paths = 0, modeCount = 0;
  LONG countRet = GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &paths, &modeCount);
  printf("active sizes ret=%ld paths=%u modes=%u gle=%lu\n", countRet, paths, modeCount, GetLastError());
  return 0;
}
