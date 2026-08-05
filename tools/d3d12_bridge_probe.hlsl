// tools/d3d12_bridge_probe.hlsl — the D12-G1 engine gate's triangle.
// Same shape as vkd3d-proton-helios/demos/triangle.hlsl, but compiled to DXIL
// (SM 6.0) rather than the demos' precompiled DXBC vs_5_0/ps_5_0 blobs, because
// DXIL is the path real D3D12 clients take and the one H5 says is reachable.
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
