$ErrorActionPreference = 'Stop'

$p = 'C:\Users\Rupansh\dxvk-helios\src\dxvk\dxvk_image.cpp'
Copy-Item $p 'C:\Users\Rupansh\dxvk-helios\src\dxvk\dxvk_image.cpp.pre-v2-keyed.bak' -Force

$lines = [System.Collections.Generic.List[string]](Get-Content $p)

function Replace-FunctionBody {
  param(
    [System.Collections.Generic.List[string]]$Lines,
    [string]$SignaturePattern,
    [string[]]$Replacement
  )

  $start = -1
  $end = -1
  for ($i = 0; $i -lt $Lines.Count; $i++) {
    if ($Lines[$i] -match $SignaturePattern) {
      $start = $i
      break
    }
  }
  if ($start -lt 0) { throw "function not found: $SignaturePattern" }

  for ($i = $start; $i -lt $Lines.Count; $i++) {
    if ($Lines[$i] -match '^  }\s*$') {
      $end = $i
      break
    }
  }
  if ($end -lt 0) { throw "function end not found: $SignaturePattern" }

  $Lines.RemoveRange($start, $end - $start + 1)
  $Lines.InsertRange($start, $Replacement)
}

Replace-FunctionBody $lines '^  HRESULT DxvkKeyedMutex::AcquireSync' @(
  '  HRESULT DxvkKeyedMutex::AcquireSync(UINT64 key, DWORD  milliseconds) {',
  '    if (m_owned.load(std::memory_order_acquire))',
  '      return DXGI_ERROR_INVALID_CALL;',
  '',
  '    LARGE_INTEGER timeout = { };',
  '    D3DKMT_ACQUIREKEYEDMUTEX2 acquire = { };',
  '    acquire.hKeyedMutex = m_kmtLocal;',
  '    acquire.Key = key;',
  '    acquire.pTimeout = &timeout;',
  '    timeout.QuadPart = milliseconds * -10000;',
  '',
  '    NTSTATUS status = D3DKMTAcquireKeyedMutex2(&acquire);',
  '    if (status == STATUS_TIMEOUT)',
  '      return WAIT_TIMEOUT;',
  '    if (status)',
  '      return DXGI_ERROR_INVALID_CALL;',
  '',
  '    VkSemaphore semaphore = m_fence->handle();',
  '    VkSemaphoreWaitInfo info = { VK_STRUCTURE_TYPE_SEMAPHORE_WAIT_INFO };',
  '    info.semaphoreCount = 1;',
  '    info.pSemaphores = &semaphore;',
  '    info.pValues = &acquire.FenceValue;',
  '',
  '    if (m_vkd->vkWaitSemaphores(m_vkd->device(), &info, -1)) {',
  '      Logger::warn("DxvkKeyedMutex::AcquireSync: Failed to wait semaphore");',
  '      return DXGI_ERROR_INVALID_CALL;',
  '    }',
  '',
  '    m_fenceValue = acquire.FenceValue;',
  '    m_owned.store(true, std::memory_order_release);',
  '    return S_OK;',
  '  }'
)

Replace-FunctionBody $lines '^  HRESULT DxvkKeyedMutex::ReleaseSync' @(
  '  HRESULT DxvkKeyedMutex::ReleaseSync(UINT64 key) {',
  '    if (!m_owned.load(std::memory_order_acquire))',
  '      return DXGI_ERROR_INVALID_CALL;',
  '',
  '    const uint64_t nextFenceValue = m_fenceValue + 1;',
  '',
  '    D3DKMT_RELEASEKEYEDMUTEX2 release = { };',
  '    release.hKeyedMutex = m_kmtLocal;',
  '    release.Key = key;',
  '    release.FenceValue = nextFenceValue;',
  '',
  '    NTSTATUS status = D3DKMTReleaseKeyedMutex2(&release);',
  '    if (status) {',
  '      Logger::warn(str::format("D3D11DXGIKeyedMutex::ReleaseSync: Failed to release mutex2: ", status));',
  '      return DXGI_ERROR_INVALID_CALL;',
  '    }',
  '',
  '    VkSemaphoreSignalInfo info = { VK_STRUCTURE_TYPE_SEMAPHORE_SIGNAL_INFO };',
  '    info.semaphore = m_fence->handle();',
  '    info.value = nextFenceValue;',
  '',
  '    if (m_vkd->vkSignalSemaphore(m_vkd->device(), &info)) {',
  '      Logger::warn("D3D11DXGIKeyedMutex::ReleaseSync: Failed to signal semaphore");',
  '      return DXGI_ERROR_INVALID_CALL;',
  '    }',
  '',
  '    m_fenceValue = nextFenceValue;',
  '    m_owned.store(false, std::memory_order_release);',
  '    return S_OK;',
  '  }'
)

Set-Content -Path $p -Value $lines -Encoding UTF8
(Get-Item $p).LastWriteTime = Get-Date

Select-String -Path $p -Pattern 'ACQUIREKEYEDMUTEX2|RELEASEKEYEDMUTEX2|D3DKMTAcquireKeyedMutex2|D3DKMTReleaseKeyedMutex2|Failed to release mutex2' -Context 2,2 |
  ForEach-Object {
    $_.Context.PreContext
    $_.Line
    $_.Context.PostContext
    '---'
  }
