#include <windows.h>
#include <stdio.h>
#include <stdint.h>
#include <d3dkmthk.h>

static void print_status(const char* name, NTSTATUS status) {
  printf("%s status=0x%08lx\n", name, (unsigned long)status);
}

int main() {
  D3DKMT_CREATEKEYEDMUTEX2 create = {};
  create.InitialValue = 0;
  create.Flags.NtSecuritySharing = 1;
  NTSTATUS status = D3DKMTCreateKeyedMutex2(&create);
  print_status("CreateKeyedMutex2", status);
  printf("  hKeyedMutex=0x%08x hSharedHandle=0x%08x\n",
      create.hKeyedMutex, create.hSharedHandle);
  if (status)
    return 1;

  LARGE_INTEGER timeout = {};
  timeout.QuadPart = -10'000'000LL;

  D3DKMT_ACQUIREKEYEDMUTEX acquire = {};
  acquire.hKeyedMutex = create.hKeyedMutex;
  acquire.Key = 0;
  acquire.pTimeout = &timeout;
  status = D3DKMTAcquireKeyedMutex(&acquire);
  print_status("AcquireKeyedMutex key=0", status);
  printf("  fence=%llu\n", (unsigned long long)acquire.FenceValue);

  D3DKMT_RELEASEKEYEDMUTEX release = {};
  release.hKeyedMutex = create.hKeyedMutex;
  release.Key = 1;
  release.FenceValue = acquire.FenceValue + 1;
  status = D3DKMTReleaseKeyedMutex(&release);
  print_status("ReleaseKeyedMutex key=1", status);

  D3DKMT_DESTROYKEYEDMUTEX destroy = {};
  destroy.hKeyedMutex = create.hKeyedMutex;
  status = D3DKMTDestroyKeyedMutex(&destroy);
  print_status("DestroyKeyedMutex", status);
  return 0;
}
