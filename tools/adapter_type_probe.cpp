// adapter_type_probe.cpp — enumerate WDDM adapters with their D3DKMT type flags
// to diagnose the "two Helios adapters" duplication + DXGI/CCD output-association
// inversion (priority #1, windowed-BLT / output-association thread, 2026-07-08).
//
// For every adapter it prints: the LUID, D3DKMT NumOfSources, the decoded
// D3DKMT_ADAPTERTYPE bitfield (Render/Display/IndirectDisplay/Paravirtualized/
// HybridDiscrete/…), and correlates with the DXGI EnumAdapters1 output count.
// D3DKMT entry points are resolved from gdi32.dll so no WDK headers are needed.
//
// Build (VM, WinLibs g++):
//   g++ -O2 -o adapter_type_probe.exe adapter_type_probe.cpp -ldxgi -ldxguid -lgdi32
// Run session-1 via schtasks; logs to C:\Users\Rupansh\adapter_type_probe.txt
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <dxgi1_6.h>
#include <cstdio>
#include <cstdint>

typedef LONG NTSTATUS;
typedef UINT D3DKMT_HANDLE;

struct D3DKMT_ADAPTERINFO {
  D3DKMT_HANDLE hAdapter;
  LUID          AdapterLuid;
  ULONG         NumOfSources;
  BOOL          bPresentMoveRegionsPreferred;
};
struct D3DKMT_ENUMADAPTERS2 {
  ULONG               NumAdapters;
  D3DKMT_ADAPTERINFO* pAdapters;
};
union D3DKMT_ADAPTERTYPE {
  struct {
    UINT RenderSupported            : 1;
    UINT DisplaySupported           : 1;
    UINT SoftwareDevice             : 1;
    UINT PostDevice                 : 1;
    UINT HybridDiscrete             : 1;
    UINT HybridIntegrated           : 1;
    UINT IndirectDisplayDevice      : 1;
    UINT Paravirtualized            : 1;
    UINT ACGSupported               : 1;
    UINT SupportSetTimingsFromVidPn : 1;
    UINT Detachable                 : 1;
    UINT ComputeOnly                : 1;
    UINT Prototype                  : 1;
    UINT RuntimePowerManagement     : 1;
    UINT Reserved                   : 18;
  };
  UINT Value;
};
struct D3DKMT_QUERYADAPTERINFO {
  D3DKMT_HANDLE hAdapter;
  UINT          Type;           // KMTQUERYADAPTERINFOTYPE
  VOID*         pPrivateDriverData;
  UINT          PrivateDriverDataSize;
};
struct D3DKMT_CLOSEADAPTER { D3DKMT_HANDLE hAdapter; };
static const UINT KMTQAITYPE_ADAPTERTYPE = 15;

typedef NTSTATUS (WINAPI *PFN_EnumAdapters2)(D3DKMT_ENUMADAPTERS2*);
typedef NTSTATUS (WINAPI *PFN_QueryAdapterInfo)(D3DKMT_QUERYADAPTERINFO*);
typedef NTSTATUS (WINAPI *PFN_CloseAdapter)(const D3DKMT_CLOSEADAPTER*);

static FILE* g=nullptr;
static void L(const char* fmt,...){char b[1024];va_list ap;va_start(ap,fmt);vsnprintf(b,sizeof b,fmt,ap);va_end(ap);
  fputs(b,stdout);fputc('\n',stdout);fflush(stdout); if(g){fputs(b,g);fputc('\n',g);fflush(g);} }

int main(){
  g=fopen("C:\\Users\\Rupansh\\adapter_type_probe.txt","w");
  HMODULE gdi=LoadLibraryA("gdi32.dll");
  auto EnumAdapters2=(PFN_EnumAdapters2)GetProcAddress(gdi,"D3DKMTEnumAdapters2");
  auto QueryAdapterInfo=(PFN_QueryAdapterInfo)GetProcAddress(gdi,"D3DKMTQueryAdapterInfo");
  auto CloseAdapter=(PFN_CloseAdapter)GetProcAddress(gdi,"D3DKMTCloseAdapter");
  L("D3DKMTEnumAdapters2=%p QueryAdapterInfo=%p", (void*)EnumAdapters2,(void*)QueryAdapterInfo);

  // --- D3DKMT enumeration with type flags ---
  D3DKMT_ENUMADAPTERS2 ea={}; NTSTATUS st=EnumAdapters2(&ea); // first call: count
  L("D3DKMTEnumAdapters2 count st=0x%lx NumAdapters=%lu",(long)st,(unsigned long)ea.NumAdapters);
  ea.pAdapters=(D3DKMT_ADAPTERINFO*)calloc(ea.NumAdapters,sizeof(D3DKMT_ADAPTERINFO));
  st=EnumAdapters2(&ea);
  L("D3DKMTEnumAdapters2 fill st=0x%lx",(long)st);
  for(ULONG i=0;i<ea.NumAdapters;i++){
    D3DKMT_ADAPTERINFO& a=ea.pAdapters[i];
    D3DKMT_ADAPTERTYPE t={}; D3DKMT_QUERYADAPTERINFO q={};
    q.hAdapter=a.hAdapter; q.Type=KMTQAITYPE_ADAPTERTYPE; q.pPrivateDriverData=&t; q.PrivateDriverDataSize=sizeof(t);
    NTSTATUS qs=QueryAdapterInfo(&q);
    L("D3DKMT[%lu] luid=%08lx:%08lx NumOfSources=%lu typeSt=0x%lx type=0x%08x { Render=%u Display=%u Sw=%u Post=%u HybDisc=%u HybInt=%u IndirectDisplay=%u Paravirt=%u Detach=%u ComputeOnly=%u }",
      (unsigned long)i,(unsigned long)a.AdapterLuid.HighPart,(unsigned long)a.AdapterLuid.LowPart,
      (unsigned long)a.NumOfSources,(long)qs,t.Value,
      t.RenderSupported,t.DisplaySupported,t.SoftwareDevice,t.PostDevice,t.HybridDiscrete,t.HybridIntegrated,
      t.IndirectDisplayDevice,t.Paravirtualized,t.Detachable,t.ComputeOnly);
    D3DKMT_CLOSEADAPTER c={a.hAdapter}; CloseAdapter(&c);
  }

  // --- DXGI correlation (LUID -> output count + desc) ---
  IDXGIFactory1* f=nullptr; CreateDXGIFactory1(__uuidof(IDXGIFactory1),(void**)&f);
  IDXGIAdapter1* a=nullptr;
  for(UINT i=0; f && f->EnumAdapters1(i,&a)!=DXGI_ERROR_NOT_FOUND; i++){
    DXGI_ADAPTER_DESC1 d{}; a->GetDesc1(&d);
    UINT outs=0; IDXGIOutput* o=nullptr; char names[512]; names[0]=0;
    for(UINT j=0; a->EnumOutputs(j,&o)!=DXGI_ERROR_NOT_FOUND; j++){ if(o){ DXGI_OUTPUT_DESC od{}; o->GetDesc(&od);
      char tmp[160]; snprintf(tmp,sizeof tmp," out%u='%ls' attached=%d", j, od.DeviceName, od.AttachedToDesktop);
      strncat(names,tmp,sizeof(names)-strlen(names)-1); outs++; o->Release(); o=nullptr; } }
    L("DXGI[%u] luid=%08lx:%08lx flags=0x%x outputs=%u name=%ls%s",
      i,(unsigned long)d.AdapterLuid.HighPart,(unsigned long)d.AdapterLuid.LowPart,(unsigned)d.Flags,outs,d.Description,names);
    a->Release(); a=nullptr;
  }
  L("done");
  return 0;
}
