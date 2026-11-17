// The colour times the coverage a - t*t*k, clamped: r + 1 - the distance from the circle's
// centre, with a = r + 1 - the radial and t the tangential offset in the band segment's own
// frame (interpolated exactly: both are linear in the position), k = 1 / (2r + 1). Small
// numbers throughout, as half floats need. Only the band's ~2 px run this (four instructions);
// insides stay ui_cov (two).
precision mediump float;
varying vec4 v_color;
varying vec3 v_cov;

void main() {
    gl_FragColor = v_color * clamp(v_cov.x - v_cov.y * v_cov.y * v_cov.z, 0.0, 1.0);
}
