// Waveform envelope reduction, per pixel column.
//
// Sixteen bars at 120 BPM is 1.54 million samples competing for maybe 1,600 pixel columns.
// Point-sampling that is not decimation, it is aliasing: a 15 kHz tone sampled every 960th frame
// becomes an arbitrary low-frequency squiggle that looks like signal. Every scope and audio editor
// worth using reduces each column to the true min/max pair present in it instead, so what is drawn
// is the real peak excursion. RMS comes along for the density fill.
//
// One workgroup per column; threads stride through it and then tree-reduce in workgroup memory.
//
// This shader knows nothing about tempo, bars, laps or wrapping. It is handed a table of "reduce
// these samples into this pixel" and that is all — every bit of musical-time reasoning, including
// which side of the write head a column falls on and what happens to one that straddles it, is
// done on the CPU in `waveroll_core::view`, where it is testable without a GPU.

struct Params {
  // x: columns   y: ringCapacity (power of two)   z: ringChannels   w: unused
  a: vec4<u32>,
  // Mixing weights turning the ring's physical channels into one logical trace, so left, right,
  // mid, side and mono all share this code path.
  mix: vec4<f32>,
}

@group(0) @binding(0) var<uniform> P: Params;
@group(0) @binding(1) var<storage, read> audio: array<f32>;
// Per column: x = first frame, already masked into the ring; y = how many frames.
// The index arrives pre-masked because WGSL has no 64-bit integer and the ring's absolute frame
// counter does not fit in one — masking on the CPU costs nothing and keeps the counter exact.
@group(0) @binding(2) var<storage, read> columns: array<vec2<u32>>;
// Per column: min, max, rms, and 1.0 where anything was captured at all.
@group(0) @binding(3) var<storage, read_write> env: array<vec4<f32>>;

const WG: u32 = 64u;

var<workgroup> sMin: array<f32, WG>;
var<workgroup> sMax: array<f32, WG>;
var<workgroup> sSum: array<f32, WG>;

fn fetch(frame: u32) -> f32 {
  let cap = P.a.y;
  let idx = frame & (cap - 1u);
  let a = audio[idx];
  var b = a;
  if (P.a.z > 1u) {
    b = audio[cap + idx];
  }
  return P.mix.x * a + P.mix.y * b;
}

@compute @workgroup_size(WG)
fn main(
  @builtin(workgroup_id) wg: vec3<u32>,
  @builtin(local_invocation_id) lid: vec3<u32>,
) {
  let column = wg.x;
  // Uniform across the workgroup, so returning here cannot leave some threads waiting at a
  // barrier the others never reach.
  if (column >= P.a.x) {
    return;
  }
  let t = lid.x;
  let entry = columns[column];
  let start = entry.x;
  let count = entry.y;

  var lo = 3.4e38;
  var hi = -3.4e38;
  var sum = 0.0;
  var i = t;
  loop {
    if (i >= count) { break; }
    let v = fetch(start + i);
    lo = min(lo, v);
    hi = max(hi, v);
    sum = sum + v * v;
    i = i + WG;
  }

  sMin[t] = lo;
  sMax[t] = hi;
  sSum[t] = sum;
  workgroupBarrier();

  var stride = WG / 2u;
  loop {
    if (stride == 0u) { break; }
    if (t < stride) {
      sMin[t] = min(sMin[t], sMin[t + stride]);
      sMax[t] = max(sMax[t], sMax[t + stride]);
      sSum[t] = sSum[t] + sSum[t + stride];
    }
    workgroupBarrier();
    stride = stride / 2u;
  }

  if (t == 0u) {
    if (count == 0u) {
      // Nothing has ever been captured here — the first lap, ahead of the head. That is not
      // silence, and the renderer draws it differently, so it has to be distinguishable.
      env[column] = vec4<f32>(0.0, 0.0, 0.0, 0.0);
    } else {
      env[column] = vec4<f32>(sMin[0], sMax[0], sqrt(sSum[0] / f32(count)), 1.0);
    }
  }
}
