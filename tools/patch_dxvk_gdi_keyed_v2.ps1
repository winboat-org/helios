$ErrorActionPreference = 'Stop'

$p = 'C:\Users\Rupansh\dxvk-helios\src\util\util_gdi.cpp'
Copy-Item $p 'C:\Users\Rupansh\dxvk-helios\src\util\util_gdi.cpp.pre-keyed-v2.bak' -Force

$text = Get-Content $p -Raw

if ($text -notmatch 'D3DKMTAcquireKeyedMutex2') {
  $needle = @'
  NTSTATUS WINAPI D3DKMTAcquireKeyedMutex(D3DKMT_ACQUIREKEYEDMUTEX *desc) {
    static decltype(D3DKMTAcquireKeyedMutex) *func;
    if (!func) {
      InterlockedCompareExchangePointer((void **)&func, (void *)GetProcAddress(GetModuleHandle("gdi32"), "D3DKMTAcquireKeyedMutex"), NULL);
      InterlockedCompareExchangePointer((void **)&func, (void *)NoD3DKMTAcquireKeyedMutex, NULL);
    }
    return func(desc);
  }

'@
  $insert = $needle + @'
  static NTSTATUS WINAPI NoD3DKMTAcquireKeyedMutex2(D3DKMT_ACQUIREKEYEDMUTEX2 *desc) {
    return (NTSTATUS)0xC0000002;
  }

  NTSTATUS WINAPI D3DKMTAcquireKeyedMutex2(D3DKMT_ACQUIREKEYEDMUTEX2 *desc) {
    static decltype(D3DKMTAcquireKeyedMutex2) *func;
    if (!func) {
      InterlockedCompareExchangePointer((void **)&func, (void *)GetProcAddress(GetModuleHandle("gdi32"), "D3DKMTAcquireKeyedMutex2"), NULL);
      InterlockedCompareExchangePointer((void **)&func, (void *)NoD3DKMTAcquireKeyedMutex2, NULL);
    }
    return func(desc);
  }

'@
  if (-not $text.Contains($needle)) { throw 'acquire implementation anchor not found' }
  $text = $text.Replace($needle, $insert)
}

if ($text -notmatch 'D3DKMTReleaseKeyedMutex2') {
  $needle = @'
  NTSTATUS WINAPI D3DKMTReleaseKeyedMutex(D3DKMT_RELEASEKEYEDMUTEX *desc) {
    static decltype(D3DKMTReleaseKeyedMutex) *func;
    if (!func) {
      InterlockedCompareExchangePointer((void **)&func, (void *)GetProcAddress(GetModuleHandle("gdi32"), "D3DKMTReleaseKeyedMutex"), NULL);
      InterlockedCompareExchangePointer((void **)&func, (void *)NoD3DKMTReleaseKeyedMutex, NULL);
    }
    return func(desc);
  }

'@
  $insert = $needle + @'
  static NTSTATUS WINAPI NoD3DKMTReleaseKeyedMutex2(D3DKMT_RELEASEKEYEDMUTEX2 *desc) {
    return (NTSTATUS)0xC0000002;
  }

  NTSTATUS WINAPI D3DKMTReleaseKeyedMutex2(D3DKMT_RELEASEKEYEDMUTEX2 *desc) {
    static decltype(D3DKMTReleaseKeyedMutex2) *func;
    if (!func) {
      InterlockedCompareExchangePointer((void **)&func, (void *)GetProcAddress(GetModuleHandle("gdi32"), "D3DKMTReleaseKeyedMutex2"), NULL);
      InterlockedCompareExchangePointer((void **)&func, (void *)NoD3DKMTReleaseKeyedMutex2, NULL);
    }
    return func(desc);
  }

'@
  if (-not $text.Contains($needle)) { throw 'release implementation anchor not found' }
  $text = $text.Replace($needle, $insert)
}

Set-Content -Path $p -Value $text -Encoding UTF8
(Get-Item $p).LastWriteTime = Get-Date

Select-String -Path $p -Pattern 'D3DKMTAcquireKeyedMutex2|D3DKMTReleaseKeyedMutex2|NoD3DKMT.*KeyedMutex2' -Context 1,1 |
  ForEach-Object {
    $_.Context.PreContext
    $_.Line
    $_.Context.PostContext
    '---'
  }
