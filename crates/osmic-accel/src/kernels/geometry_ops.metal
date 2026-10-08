#include <metal_stdlib>
using namespace metal;

// ============================================================================
// Shared layout. These structs MUST match the #[repr(C)] structs in
// src/metal/types.rs field for field; the Rust side asserts size, alignment and
// every field offset at compile time.
// ============================================================================

struct GpuUnit {            // 48 bytes
    uint  coord_offset;     // 0   first input vertex (index into coords)
    uint  coord_count;      // 4
    uint  kind;             // 8   KIND_LINE / KIND_RING
    uint  out_offset;       // 12  first output vertex (index into out_points)
    uint  out_capacity;     // 16  output vertex capacity
    uint  scratch_offset;   // 20  first scratch vertex (rings only)
    uint  part_offset;      // 24  first entry in part_lens (lines only)
    uint  part_capacity;    // 28
    float min_x;            // 32  clip rectangle
    float min_y;            // 36
    float max_x;            // 40
    float max_y;            // 44
};

struct GpuUnitResult {      // 16 bytes
    uint out_count;         // 0   vertices written
    uint part_count;        // 4   parts written (lines)
    uint status;            // 8   STATUS_*
    uint _pad;              // 12
};

struct GpuClipParams {      // 16 bytes
    uint unit_count;        // 0
    uint _pad0;             // 4
    uint _pad1;             // 8
    uint _pad2;             // 12
};

constant uint KIND_LINE = 1;
constant uint KIND_RING = 2;

constant uint STATUS_OK       = 0;
constant uint STATUS_OVERFLOW = 1;  // capacity exceeded; host recomputes on CPU
constant uint STATUS_INVALID  = 2;  // malformed descriptor; host reports an error
// 0xFFFFFFFF (host-initialised) means "never processed".

// ============================================================================
// Rings: Sutherland-Hodgman
//
// Mirrors src/cpu.rs::clip_ring. Per-ring output and scratch regions live in
// device memory with host-computed capacities; every write is bounds-checked
// and an exceeded capacity is reported, never truncated.
// ============================================================================

// One stage. A vertex p is inside when (p[axis] - value) * sign >= 0.
// Returns false if more than `cap` vertices would be written.
static bool clip_stage(
    device const float2* src, uint n,
    device float2*       dst, uint cap,
    uint axis, float sign, float value,
    thread uint& out_n
) {
    uint count = 0;
    if (n == 0) { out_n = 0; return true; }

    float2 prev = src[n - 1];
    float prev_d = (prev[axis] - value) * sign;
    for (uint i = 0; i < n; i++) {
        float2 curr = src[i];
        float curr_d = (curr[axis] - value) * sign;
        bool curr_in = curr_d >= 0.0f;
        bool prev_in = prev_d >= 0.0f;
        if (curr_in != prev_in) {
            if (count >= cap) return false;
            float t = prev_d / (prev_d - curr_d);
            float2 p = prev + t * (curr - prev);
            p[axis] = value;
            dst[count++] = p;
        }
        if (curr_in) {
            if (count >= cap) return false;
            dst[count++] = curr;
        }
        prev = curr;
        prev_d = curr_d;
    }
    out_n = count;
    return true;
}

static uint clip_ring(
    GpuUnit u,
    device const float2* coords,
    device float2*       out_points,
    device float2*       scratch,
    thread uint&         status
) {
    device const float2* input = coords + u.coord_offset;
    uint n = u.coord_count;
    uint cap = u.out_capacity;
    device float2* out = out_points + u.out_offset;
    device float2* tmp = scratch + u.scratch_offset;

    float2 lo = input[0];
    float2 hi = lo;
    for (uint i = 1; i < n; i++) {
        lo = min(lo, input[i]);
        hi = max(hi, input[i]);
    }

    // Entirely outside one edge: every stage would drop everything.
    if (hi.x < u.min_x || lo.x > u.max_x || hi.y < u.min_y || lo.y > u.max_y) {
        return 0;
    }

    bool needed[4] = { lo.x < u.min_x, hi.x > u.max_x, lo.y < u.min_y, hi.y > u.max_y };
    uint axes[4]   = { 0, 0, 1, 1 };
    float signs[4] = { 1.0f, -1.0f, 1.0f, -1.0f };
    float values[4] = { u.min_x, u.max_x, u.min_y, u.max_y };

    uint stages = 0;
    for (uint s = 0; s < 4; s++) stages += needed[s] ? 1u : 0u;

    if (stages == 0) {
        if (n > cap) { status = STATUS_OVERFLOW; return 0; }
        for (uint i = 0; i < n; i++) out[i] = input[i];
        return n;
    }

    // Ping-pong between `tmp` and `out` so the last stage lands in `out`.
    device const float2* src = input;
    uint src_n = n;
    uint remaining = stages;
    for (uint s = 0; s < 4; s++) {
        if (!needed[s]) continue;
        remaining--;
        device float2* dst;
        if (src == input) {
            dst = (remaining % 2 == 0) ? out : tmp;
        } else {
            dst = (src == out) ? tmp : out;
        }
        uint m = 0;
        if (!clip_stage(src, src_n, dst, cap, axes[s], signs[s], values[s], m)) {
            status = STATUS_OVERFLOW;
            return 0;
        }
        if (m == 0) return 0;
        src = dst;
        src_n = m;
    }
    return src_n < 3 ? 0 : src_n;
}

// ============================================================================
// Lines: Liang-Barsky per segment, one output part per contiguous inside run.
//
// Mirrors src/cpu.rs::clip_polyline. Capacities are exact upper bounds
// (2 * (n - 1) points, n - 1 parts) but every write is still checked.
// ============================================================================

static bool clip_segment(float2 p0, float2 p1, GpuUnit u, thread float& t0_out, thread float& t1_out) {
    float dx = p1.x - p0.x;
    float dy = p1.y - p0.y;
    float t0 = 0.0f;
    float t1 = 1.0f;
    float ps[4] = { -dx, dx, -dy, dy };
    float qs[4] = { p0.x - u.min_x, u.max_x - p0.x, p0.y - u.min_y, u.max_y - p0.y };
    for (uint k = 0; k < 4; k++) {
        float p = ps[k];
        float q = qs[k];
        if (p == 0.0f) {
            if (q < 0.0f) return false;
        } else {
            float r = q / p;
            if (p < 0.0f) {
                if (r > t1) return false;
                if (r > t0) t0 = r;
            } else {
                if (r < t0) return false;
                if (r < t1) t1 = r;
            }
        }
    }
    if (!(t0 < t1)) return false;  // grazing a corner is not a segment
    t0_out = t0;
    t1_out = t1;
    return true;
}

static float2 point_at(float2 p0, float2 p1, float t, GpuUnit u) {
    float2 p = p0 + t * (p1 - p0);
    return float2(clamp(p.x, u.min_x, u.max_x), clamp(p.y, u.min_y, u.max_y));
}

// Close the open part (if any). Returns false if the part table is full.
static bool close_part(thread uint& current, thread uint& parts, device uint* part_lens, GpuUnit u) {
    if (current > 0) {
        if (parts >= u.part_capacity) return false;
        part_lens[u.part_offset + parts] = current;
        parts++;
        current = 0;
    }
    return true;
}

// Returns false on capacity overflow.
static bool clip_polyline(
    GpuUnit u,
    device const float2* coords,
    device float2*       out_points,
    device uint*         part_lens,
    thread uint&         out_count,
    thread uint&         part_count
) {
    device const float2* input = coords + u.coord_offset;
    device float2* out = out_points + u.out_offset;
    uint count = 0;
    uint parts = 0;
    uint current = 0;

    for (uint i = 0; i + 1 < u.coord_count; i++) {
        float2 p0 = input[i];
        float2 p1 = input[i + 1];
        float t0, t1;
        if (!clip_segment(p0, p1, u, t0, t1)) {
            if (!close_part(current, parts, part_lens, u)) return false;
            continue;
        }
        bool start_clipped = t0 > 0.0f;
        bool end_clipped = t1 < 1.0f;
        if (current > 0 && start_clipped) {
            if (!close_part(current, parts, part_lens, u)) return false;
        }
        if (current == 0) {
            if (count >= u.out_capacity) return false;
            out[count++] = start_clipped ? point_at(p0, p1, t0, u) : p0;
            current = 1;
        }
        if (count >= u.out_capacity) return false;
        out[count++] = end_clipped ? point_at(p0, p1, t1, u) : p1;
        current++;
        if (end_clipped) {
            if (!close_part(current, parts, part_lens, u)) return false;
        }
    }
    if (!close_part(current, parts, part_lens, u)) return false;

    out_count = count;
    part_count = parts;
    return true;
}

// ============================================================================
// Kernel: one thread per unit (ring or polyline).
// ============================================================================

kernel void clip_units(
    device const float2*     coords      [[buffer(0)]],
    device float2*           out_points  [[buffer(1)]],
    device float2*           scratch     [[buffer(2)]],
    device uint*             part_lens   [[buffer(3)]],
    device const GpuUnit*    units       [[buffer(4)]],
    device GpuUnitResult*    results     [[buffer(5)]],
    constant GpuClipParams&  params      [[buffer(6)]],
    uint                     gid         [[thread_position_in_grid]]
) {
    if (gid >= params.unit_count) return;

    GpuUnit u = units[gid];
    uint out_count = 0;
    uint part_count = 0;
    uint status = STATUS_OK;

    if (u.kind == KIND_RING && u.coord_count >= 3) {
        out_count = clip_ring(u, coords, out_points, scratch, status);
    } else if (u.kind == KIND_LINE && u.coord_count >= 2) {
        if (!clip_polyline(u, coords, out_points, part_lens, out_count, part_count)) {
            status = STATUS_OVERFLOW;
            out_count = 0;
            part_count = 0;
        }
    } else {
        status = STATUS_INVALID;
    }

    GpuUnitResult r;
    r.out_count = (status == STATUS_OK) ? out_count : 0;
    r.part_count = (status == STATUS_OK) ? part_count : 0;
    r.status = status;
    r._pad = 0;
    results[gid] = r;
}
