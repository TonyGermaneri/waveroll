// Batched Stockham autosort FFT.
//
// Why Stockham rather than textbook Cooley-Tukey: the autosort formulation produces
// naturally-ordered output without a bit-reversal permutation. Bit reversal is a scatter with
// terrible memory locality and it is the single worst thing you can ask a GPU to do in an
// otherwise perfectly coalesced kernel. Stockham pays for this by being out-of-place, which
// costs one extra buffer and nothing else — we ping-pong between two storage buffers.
//
// Radix-4 is the main kernel: it halves the number of dispatches versus radix-2 (log4 N
// instead of log2 N) and does 8 complex adds + 3 complex multiplies per 4 points, which is
// fewer multiplies per point than two radix-2 stages. When log2(N) is odd we run a single
// radix-2 stage first to make the remaining stage count even.
//
// Twiddle factors come from a table computed in f64 on the CPU. Evaluating sin/cos in the
// shader is a full ULP or two worse at N = 65536, and twiddle error is the dominant error
// term in a large FFT.

struct Params {
  // x: l (complex transform length)   y: p (stage parameter)
  // z: unit (twiddle table stride for this stage)   w: threadsPerTransform
  a: vec4<u32>,
  // x: batch   y: tableMask (= n - 1, table length is n = 2l)   z: totalThreads   w: unused
  b: vec4<u32>,
}

@group(0) @binding(0) var<uniform> P: Params;
@group(0) @binding(1) var<storage, read> src: array<vec2<f32>>;
@group(0) @binding(2) var<storage, read_write> dst: array<vec2<f32>>;
@group(0) @binding(3) var<storage, read> twiddle: array<vec2<f32>>;

fn cmul(a: vec2<f32>, b: vec2<f32>) -> vec2<f32> {
  return vec2<f32>(a.x * b.x - a.y * b.y, a.x * b.y + a.y * b.x);
}

// Multiplication by -i and +i, used for the radix-4 butterfly's quarter-turn terms.
fn mul_neg_i(z: vec2<f32>) -> vec2<f32> { return vec2<f32>(z.y, -z.x); }
fn mul_pos_i(z: vec2<f32>) -> vec2<f32> { return vec2<f32>(-z.y, z.x); }

@compute @workgroup_size(64)
fn radix2(@builtin(global_invocation_id) gid: vec3<u32>) {
  let tid = gid.x;
  if (tid >= P.b.z) {
    return;
  }
  let l = P.a.x;
  let p = P.a.y;
  let unit = P.a.z;
  let t = P.a.w;          // l / 2
  let mask = P.b.y;

  let i = tid % t;
  let xform = tid / t;
  let base = xform * l;

  let k = i & (p - 1u);
  let w = twiddle[(k * unit) & mask];

  let u0 = src[base + i];
  let u1 = src[base + i + t];
  let v = cmul(w, u1);

  let j = ((i - k) << 1u) + k;
  dst[base + j] = u0 + v;
  dst[base + j + p] = u0 - v;
}

@compute @workgroup_size(64)
fn radix4(@builtin(global_invocation_id) gid: vec3<u32>) {
  let tid = gid.x;
  if (tid >= P.b.z) {
    return;
  }
  let l = P.a.x;
  let p = P.a.y;
  let unit = P.a.z;
  let t = P.a.w;          // l / 4
  let mask = P.b.y;

  let i = tid % t;
  let xform = tid / t;
  let base = xform * l;

  let k = i & (p - 1u);
  let step = k * unit;
  let w1 = twiddle[step & mask];
  let w2 = twiddle[(2u * step) & mask];
  let w3 = twiddle[(3u * step) & mask];

  let a = src[base + i];
  let b = cmul(w1, src[base + i + t]);
  let c = cmul(w2, src[base + i + 2u * t]);
  let d = cmul(w3, src[base + i + 3u * t]);

  // DFT-4 of (a, b, c, d) with the forward sign convention e^(-2*pi*i*nk/4):
  //   X0 = a + b + c + d      X1 = a - i*b - c + i*d
  //   X2 = a - b + c - d      X3 = a + i*b - c - i*d
  let apc = a + c;
  let amc = a - c;
  let bpd = b + d;
  let bmd = b - d;

  let j = ((i - k) << 2u) + k;
  dst[base + j] = apc + bpd;
  dst[base + j + p] = amc + mul_neg_i(bmd);
  dst[base + j + 2u * p] = apc - bpd;
  dst[base + j + 3u * p] = amc + mul_pos_i(bmd);
}
