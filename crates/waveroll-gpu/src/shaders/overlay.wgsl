// Grid lines, the selection, and the write head.
//
// One pipeline for all three, because all three are rectangles and the differences between them
// are colour and position rather than behaviour. Anything that needs to sit over the trace goes
// through here, in the order it was pushed — which is also the order it stacks, since there is no
// depth test and the later draw wins.
//
// Positions arrive in pixels, already snapped by the caller. Snapping in the shader would have to
// happen after the projection and would then depend on the viewport size in a way that puts a
// one-bar line half a pixel from where the selection edge that shares its position lands.

struct Style {
  // x: width px   y: height px   z: 1/width   w: 1/height
  resolution: vec4<f32>,
}

struct Rect {
  // x0, y0, x1, y1 in pixels, origin top left
  bounds: vec4<f32>,
  colour: vec4<f32>,
}

@group(0) @binding(0) var<uniform> S: Style;
@group(0) @binding(1) var<storage, read> rects: array<Rect>;

struct VsOut {
  @builtin(position) pos: vec4<f32>,
  @location(0) tint: vec4<f32>,
}

fn quadCorner(i: u32) -> vec2<f32> {
  var xs = array<f32, 6>(0.0, 1.0, 0.0, 0.0, 1.0, 1.0);
  var ys = array<f32, 6>(0.0, 0.0, 1.0, 1.0, 0.0, 1.0);
  return vec2<f32>(xs[i], ys[i]);
}

fn toNdc(px: vec2<f32>, res: vec2<f32>) -> vec4<f32> {
  return vec4<f32>(px.x / res.x * 2.0 - 1.0, 1.0 - px.y / res.y * 2.0, 0.0, 1.0);
}

@vertex
fn vs(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32) -> VsOut {
  let r = rects[ii];
  let c = quadCorner(vi);
  var out: VsOut;
  out.pos = toNdc(
    vec2<f32>(mix(r.bounds.x, r.bounds.z, c.x), mix(r.bounds.y, r.bounds.w, c.y)),
    S.resolution.xy,
  );
  out.tint = r.colour;
  return out;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
  return in.tint;
}
