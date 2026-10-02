# Retired private ExecuteIndirect implementation

The owner withdrew this implementation on 2026-09-09 and authorized a
virglrenderer fork. The private `indirect_emulation*` files, shader, PSO/root
variants and CPU-assisted IA continuation hooks have been removed from vkd3d.
The replacement contract is [SUBSTRATE](SUBSTRATE.md#native-dgc-contract): native EXT DGC
through paired Venus protocol, renderer and Mesa implementations. Missing
native DGC refuses state-changing signatures; it does not select this fallback.

Earlier 361C and BE9D guest runs exercised the retired path (12 IA cases,
48 words, 12 query results and pending-list Reset/dependency ordering).
Their receipts remain in `tmp/fl12-indirect-ia-20260908/` and
`tmp/fl12-predicated-tiles-20260908/native-validation.json`; ROADMAP.md retains
the dated deployment evidence. These results do not validate the replacement.
The unresolved allocator-reset/fence-worker lifetime question remains open.

The private 128-DWORD runtime root ABI remains an independent contract in
[ROOT_SIGNATURES.md](ROOT_SIGNATURES.md). Native DGC still refuses roots using
push UBOs; this boundary must be exercised through the Windows runtime and
resolved before claiming complete FL12 conformance. No fallback, capability
override or silent success substitutes for that behavior.
