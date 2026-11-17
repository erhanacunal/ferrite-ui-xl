// A vertex of a circle's edge band (ferrite-core lcd/tess.rs `quarter_band`, compose.rs
// `Prim::Tris` with `band`): its position in pixels from the frame's top left, a
// premultiplied colour, and the coverage terms `cov` = (a, t, k): see ui_band.frag.
uniform vec2 scale;
attribute vec2 pos;
attribute vec4 color;
attribute vec3 cov;
varying vec4 v_color;
varying vec3 v_cov;

void main() {
    v_color = color;
    v_cov = cov;
    // scale = 2 / the frame's size: pixels to clip space, +y up (the frame's first line).
    gl_Position = vec4(pos.x * scale.x - 1.0, 1.0 - pos.y * scale.y, 0.0, 1.0);
}
