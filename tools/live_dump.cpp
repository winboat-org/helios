#include <windows.h>
#include <dbghelp.h>
#include <stdio.h>

int wmain(int argc, wchar_t** argv) {
  if (argc != 3) {
    fwprintf(stderr, L"usage: live_dump <pid> <dump-path>\n");
    return 2;
  }

  DWORD pid = wcstoul(argv[1], nullptr, 0);
  HANDLE process = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ | PROCESS_DUP_HANDLE,
                               FALSE, pid);
  if (!process) {
    fwprintf(stderr, L"OpenProcess(%lu) failed gle=%lu\n", pid, GetLastError());
    return 1;
  }

  HANDLE file = CreateFileW(argv[2], GENERIC_WRITE, 0, nullptr, CREATE_ALWAYS,
                            FILE_ATTRIBUTE_NORMAL, nullptr);
  if (file == INVALID_HANDLE_VALUE) {
    fwprintf(stderr, L"CreateFile(%ls) failed gle=%lu\n", argv[2], GetLastError());
    CloseHandle(process);
    return 1;
  }

  MINIDUMP_TYPE type = (MINIDUMP_TYPE)(
      MiniDumpWithFullMemory |
      MiniDumpWithHandleData |
      MiniDumpWithUnloadedModules |
      MiniDumpWithThreadInfo |
      MiniDumpWithFullMemoryInfo);
  BOOL ok = MiniDumpWriteDump(process, pid, file, type, nullptr, nullptr, nullptr);
  if (!ok)
    fwprintf(stderr, L"MiniDumpWriteDump failed gle=%lu\n", GetLastError());
  CloseHandle(file);
  CloseHandle(process);
  return ok ? 0 : 1;
}
