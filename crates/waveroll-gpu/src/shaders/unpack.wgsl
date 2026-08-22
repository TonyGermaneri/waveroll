// Stage 3: split the half-length complex transform back into the real signal's spectrum.
//
// Given Z = FFT_{L}(z) where z[j] = x[2j] + i*x[2j+1] and L = N/2:
//
//   Ze[k] = (Z[k] + conj(Z[L-k])) / 2          (transform of the even samples)
//   Zo[k] = (Z[k] - conj(Z[L-k])) / (2i)       (transform of the odd samples)
//   X[k]  = Ze[k] + e^(-2*pi*i*k/N) * Zo[k]    k = 0 .. L
//
// Z is treated as periodic with period L, so Z[L] aliases to Z[0]; that makes the k = 0 and
// k = L (DC and Nyquist) cases fall out of the same expression instead of needing branches.
// The e^(-2*pi*i*k/N) factor is read straight out of the same length-N twiddle table the FFT
// stages use.

struct Params {
  // x: l (= n/2)   y: n   z: batch   w: totalThreads
  a: vec4<u32>,
}

@group(0) @binding(0) var<uniform> P: Params;
@group(0) @binding(1) var<storage, read> src: array<vec2<f32>>;
@group(0) @binding(2) var<storage, read_write> bins: array<vec2<f32>>;
@group(0) @binding(3) var<storage, read> twiddle: array<vec2<f32>>;

@compute @workgroup_size(64)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
  let tid = gid.x;
  if (tid >= P.a.w) {
    return;
  }
  let l = P.a.x;
  let outStride = l + 1u;

  let k = tid % outStride;
  let batch = tid / outStride;
  let base = batch * l;

  let zk = src[base + (k % l)];
  let zm = src[base + ((l - k) % l)];
  let conj = vec2<f32>(zm.x, -zm.y);

  let even = 0.5 * (zk + conj);
  let diff = 0.5 * (zk - conj);
  // Dividing by i is a -90 degree rotation: (a + bi)/i = b - ai.
  let odd = vec2<f32>(diff.y, -diff.x);

  let w = twiddle[k % P.a.y];
  let rotated = vec2<f32>(odd.x * w.x - odd.y * w.y, odd.x * w.y + odd.y * w.x);

  bins[batch * outStride + k] = even + rotated;
}
