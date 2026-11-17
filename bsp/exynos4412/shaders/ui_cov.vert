// A vertex of the tessellator's anti-aliased triangles (ferrite-core lcd/tess.rs, compose.rs
// `Prim::Tris`): its position in pixels from the frame's top left, a premultiplied colour,
// and `a` 0..2, whose tent min(a, 2 - a) is the coverage.
uniform vec2 scale;
attribute vec2 pos;
attribute vec4 color;
attribute float a;
varying vec4 v_color;
varying float v_a;

void main() {
    v_color = color;
    v_a = a;
    // scale = 2 / the frame's size: pixels to clip space, +y up (the frame's first line).
    gl_Position = vec4(pos.x * scale.x - 1.0, 1.0 - pos.y * scale.y, 0.0, 1.0);
}
