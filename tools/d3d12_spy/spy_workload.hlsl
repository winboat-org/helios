// spy_workload.hlsl — the triangle the D12-G5 workload draws.
//
// Deliberately compilable as BOTH `vs_5_1`/`ps_5_1` (DXBC, via D3DCompile at runtime) and
// `vs_6_0`/`ps_6_0` (DXIL, via dxc -Fh at build time). Q1 / DDI_REFERENCE.md §15 #14 asks
// what the runtime hands `pfnCreateShader` in each case, and the only way to answer it for
// both is to draw with both in one process.
struct ps_in
{
    float4 position : SV_POSITION;
    float4 colour : COLOR;
};

struct ps_in vs_main(float4 position : POSITION, float4 colour : COLOR)
{
    struct ps_in o;
    o.position = position;
    o.colour = colour;
    return o;
}

float4 ps_main(struct ps_in i) : SV_TARGET
{
    return i.colour;
}
