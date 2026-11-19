#include <metal_stdlib>
using namespace metal;

struct TexDesc {
    uint enabled, width, height, nlevels, filter, border, kind, depth;
    uint first, layers, map, sampler, manual;
    uint pad[3];
};

