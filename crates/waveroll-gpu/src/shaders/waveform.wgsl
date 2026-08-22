// The waveform, drawn from the reduced envelope.
//
// One instance per column, two quads each: the min/max peak bar and the RMS bar inside it. Peak
// alone tells you the excursion but not the density — a lone click and a sustained tone reaching
// the same level look identical — and RMS alone hides the transient that will clip. Drawing both,
// with RMS inside peak, is the shape every editor settled on because it reads as one thing.
//
// Geometry is generated rather than uploaded: six vertices from `vertex_index`, positioned from
// the storage buffer the compute pass already filled. Nothing crosses the bus per frame except
// the envelope itself.

struct Style {
  // x: width px   y: height px   z: 1/width   w: 1/height
  resolution: vec4<f32>,
  peak: vec4<f32>,
  rms: vec4<f32>,
  // Columns that have never been captured. Not silence — the first lap, ahead of the head.
  unwritten: vec4<f32>,
  // x: vertical gain   y: column count   z: minimum bar height px   w: unused
  geom: vec4<f32>,
}

@group(0) @binding(0) var<uniform> S: Style;
// min, max, rms, written
@group(0) @binding(1) var<storage, read> env: array<vec4<f32>>;

struct VsOut {
  @builtin(position) pos: vec4<f32>,
  @location(0) tint: vec4<f32>,
}

fn quadCorner(i: u32) -> vec2<f32> {
  var xs = array<f32, 6>(0.0, 1.0, 0.0, 0.0, 1.0, 1.0);
  var ys = array<f32, 6>(0.0, 0.0, 1.0, 1.0, 0.0, 1.0);
  return vec2<f32>(xs[i], ys[i]);
}

/// Pixel coordinates, origin top left, to normalised device coordinates.
fn toNdc(px: vec2<f32>, res: vec2<f32>) -> vec4<f32> {
  return vec4<f32>(px.x / res.x * 2.0 - 1.0, 1.0 - px.y / res.y * 2.0, 0.0, 1.0);
}

/// Sample value to a y pixel. +1 is the top of the pane, -1 the bottom.
fn valueToY(v: f32, height: f32, gain: f32) -> f32 {
  return (0.5 - clamp(v * gain, -1.0, 1.0) * 0.5) * height;
}

@vertex
fn vs(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32) -> VsOut {
  let columns = u32(S.geom.y);
  let column = ii % columns;
  let layer = ii / columns;          // 0 = peak, 1 = rms
  let e = env[column];
  let width = S.resolution.x;
  let height = S.resolution.y;
  let gain = S.geom.x;
  let minimum = S.geom.z;

  // Columns are laid out in pixels rather than fractions so a column boundary always lands on a
  // pixel boundary. Fractional columns would give neighbouring bars different widths, which reads
  // as a beating pattern across the trace at some window sizes and not others.
  let x0 = floor(f32(column) * width / S.geom.y);
  let x1 = max(floor(f32(column + 1u) * width / S.geom.y), x0 + 1.0);

  var top = 0.0;
  var bottom = 0.0;
  var tint = S.peak;

  if (e.w < 0.5) {
    // Never captured. A hairline at the centre, so the empty part of the first lap reads as
    // "nothing yet" rather than as digital silence, which is a different fact.
    if (layer == 1u) {
      // The RMS layer has nothing to add here; collapse it off screen.
      var out: VsOut;
      out.pos = vec4<f32>(0.0, 0.0, 0.0, 0.0);
      out.tint = vec4<f32>(0.0);
      return out;
    }
    top = height * 0.5 - 0.5;
    bottom = top + 1.0;
    tint = S.unwritten;
  } else if (layer == 0u) {
    top = valueToY(e.y, height, gain);
    bottom = valueToY(e.x, height, gain);
  } else {
    top = valueToY(e.z, height, gain);
    bottom = valueToY(-e.z, height, gain);
    tint = S.rms;
  }

  // Silence still has to draw something, or a quiet passage becomes a hole in the trace.
  let centre = (top + bottom) * 0.5;
  let half = max((bottom - top) * 0.5, minimum * 0.5);
  if (half * 2.0 <= 1.5) {
    // A hairline is only whole where it is centred. A bar running from y = 31.5 to 32.5 is
    // evaluated at the centres of rows 31 and 32 — which are precisely its own two edges, where
    // its coverage is zero — and disappears. It only happens where the arithmetic lands on a half
    // pixel, which is what makes it look like a bug in whatever computed the position rather than
    // in how it was drawn. Thin bars are snapped onto a whole pixel row instead.
    top = floor(centre);
    bottom = top + max(minimum, 1.0);
  } else {
    top = centre - half;
    bottom = centre + half;
  }

  let c = quadCorner(vi);
  var out: VsOut;
  out.pos = toNdc(vec2<f32>(mix(x0, x1, c.x), mix(top, bottom, c.y)), S.resolution.xy);
  out.tint = tint;
  return out;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
  return in.tint;
}
