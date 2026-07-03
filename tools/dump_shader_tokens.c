/* dump_shader_tokens.c — D3DCompile the draw-probe shaders and write both the
 * full DXBC container and the raw code-chunk token stream (what the D3D11
 * runtime hands the UMD) to files, for offline dxbc-spirv repro.
 *
 * Build (VM, vcvars64):
 *   cl /nologo dump_shader_tokens.c /link d3dcompiler.lib
 */
#include <windows.h>
#include <d3dcompiler.h>
#include <stdio.h>

static const char* kVs =
    "float4 main(float2 pos : POSITION) : SV_Position {"
    "  return float4(pos, 0.0, 1.0);"
    "}";
static const char* kPs =
    "float4 main() : SV_Target {"
    "  return float4(0.2, 0.4, 0.6, 0.8);"
    "}";

static void write_file(const char* path, const void* data, size_t len) {
  FILE* f = fopen(path, "wb");
  if (!f) { printf("open %s failed\n", path); return; }
  fwrite(data, 1, len, f);
  fclose(f);
  printf("wrote %s (%zu bytes)\n", path, len);
}

/* Extract the SHDR/SHEX chunk payload from a DXBC container. */
static void dump_code_chunk(const char* path, const unsigned char* dxbc, size_t len) {
  unsigned chunk_count = *(const unsigned*)(dxbc + 28);
  for (unsigned i = 0; i < chunk_count; ++i) {
    unsigned off = *(const unsigned*)(dxbc + 32 + 4 * i);
    const char* tag = (const char*)(dxbc + off);
    unsigned size = *(const unsigned*)(dxbc + off + 4);
    if (!memcmp(tag, "SHDR", 4) || !memcmp(tag, "SHEX", 4)) {
      write_file(path, dxbc + off + 8, size);
      return;
    }
  }
  printf("no code chunk in %s\n", path);
  (void)len;
}

static void compile_one(const char* src, const char* target, const char* base) {
  ID3DBlob *blob = NULL, *err = NULL;
  char path[256];
  HRESULT hr = D3DCompile(src, strlen(src), NULL, NULL, NULL, "main", target,
                          0, 0, &blob, &err);
  if (FAILED(hr)) {
    printf("%s compile hr=0x%08x %s\n", base, (unsigned)hr,
           err ? (char*)err->lpVtbl->GetBufferPointer(err) : "");
    return;
  }
  snprintf(path, sizeof(path), "%s.dxbc", base);
  write_file(path, blob->lpVtbl->GetBufferPointer(blob),
             blob->lpVtbl->GetBufferSize(blob));
  snprintf(path, sizeof(path), "%s.tokens", base);
  dump_code_chunk(path, blob->lpVtbl->GetBufferPointer(blob),
                  blob->lpVtbl->GetBufferSize(blob));
}

int main(void) {
  compile_one(kVs, "vs_4_0", "probe_vs");
  compile_one(kPs, "ps_4_0", "probe_ps");
  return 0;
}
