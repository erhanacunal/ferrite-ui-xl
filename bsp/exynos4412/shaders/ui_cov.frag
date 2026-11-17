// The colour times the coverage, the tent of the interpolated `a`: 0 at a = 0 and 2, 1 at
// a = 1 (a 1 px line's quad runs 0 -> 2 across it; other pieces stay within 0..1).
precision mediump float;
varying vec4 v_color;
varying float v_a;

void main() {
    gl_FragColor = v_color * (1.0 - abs(1.0 - v_a));
}
