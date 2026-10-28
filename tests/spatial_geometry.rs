// Standalone geometry regressions: rustc --test tests/spatial_geometry.rs.
// The protocol constants below are used only by mixer.rs level-meter tests.
#![allow(dead_code)]
mod proto {
    pub const LEVEL_SCALE_TO_WIRE: f32 = 128.0;
    pub const SPEAKING_AUDIO_LEVEL: f32 = 0.30;
}
#[path = "../src/mixer.rs"]
mod mixer;
use mixer::pan_weights;

const IDENTITY: [f64; 4] = [0.0, 0.0, 0.0, 1.0];
fn centred(pos: [f64; 3], q: [f64; 4]) {
    let (l, r) = pan_weights(pos, [0.0; 3], q);
    assert!((l-r).abs() < 1e-5, "expected centred {pos:?}: {l}, {r}");
}
#[test]
fn regression_front_and_back_are_centred() {
    centred([3.0, 0.0, 0.0], IDENTITY);
    centred([-3.0, 0.0, 0.0], IDENTITY);
}
#[test]
fn regression_left_and_right_match_viewer_axes() {
    let (l,r) = pan_weights([0.0,3.0,0.0], [0.0;3], IDENTITY);
    assert!(l > 0.99 && r < 1e-5, "left source: {l}, {r}");
    let (l,r) = pan_weights([0.0,-3.0,0.0], [0.0;3], IDENTITY);
    assert!(r > 0.99 && l < 1e-5, "right source: {l}, {r}");
}
#[test]
fn regression_front_remains_centred_through_full_turn_with_wire_rounding() {
    for degrees in 0..360 {
        let yaw = (degrees as f64).to_radians();
        let q = [0.0, 0.0, (yaw/2.0).sin(), (yaw/2.0).cos()];
        let wire = q.map(|x| (x*100.0).trunc()/100.0);
        // Use the direction represented by the quantized quaternion, so this
        // tests normalization independently of the wire's angular precision.
        let n = (wire[2]*wire[2]+wire[3]*wire[3]).sqrt();
        let z=wire[2]/n; let w=wire[3]/n;
        centred([3.0*(1.0-2.0*z*z), 3.0*(2.0*w*z), 0.0], wire);
    }
}
#[test]
fn regression_pitch_keeps_a_source_along_the_camera_forward_axis_centred() {
    // 90-degree pitch, then 45-degree pitch with wire quantization.
    centred([0.0,0.0,-3.0], [0.0,0.70,0.0,0.70]);
    let y=0.38_f64; let w=0.92_f64; let n=(y*y+w*w).sqrt();
    let y=y/n; let w=w/n;
    centred([3.0*(1.0-2.0*y*y),0.0,-6.0*y*w], [0.0,0.38,0.0,0.92]);
}
#[test]
fn regression_elevation_does_not_turn_small_lateral_offsets_into_hard_pan() {
    let (l,r)=pan_weights([0.0,0.01,10.0],[0.0;3],IDENTITY);
    assert!((l-r).abs()<0.002, "nearly overhead: {l}, {r}");
}
#[test]
fn regression_fixed_distance_orbit_preserves_stereo_power_and_distance_gain() {
    for degrees in 0..360 {
        let a=(degrees as f64).to_radians();
        let pos=[3.0*a.cos(),3.0*a.sin(),0.0];
        let (l,r)=pan_weights(pos,[0.0;3],IDENTITY);
        assert!((l*l+r*r-1.0).abs()<1e-5);
        assert_eq!(mixer::distance_gain(pos[0].hypot(pos[1])),1.0);
    }
}
#[test]
fn regression_common_translation_does_not_change_pan() {
    let relative=[3.0,2.0,1.0];
    let origin=[256000.0,768000.0,22.0];
    let absolute=std::array::from_fn(|i|origin[i]+relative[i]);
    let a=pan_weights(relative,[0.0;3],IDENTITY);
    let b=pan_weights(absolute,origin,IDENTITY);
    assert!((a.0-b.0).abs()<1e-6 && (a.1-b.1).abs()<1e-6);
}
#[test]
fn regression_invalid_orientation_has_a_finite_identity_fallback() {
    for q in [[0.0;4],[f64::NAN,0.0,0.0,1.0],[0.0,0.0,0.0,f64::INFINITY]] {
        centred([3.0,0.0,0.0],q);
    }
}
#[test]
fn regression_large_finite_orientation_is_normalized_without_overflow() {
    centred([0.0,3.0,0.0],[0.0,0.0,1e300,1e300]);
}
