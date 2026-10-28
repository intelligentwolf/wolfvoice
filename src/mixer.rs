/*
 * WolfVoice - spatial WebRTC voice for OpenSimulator
 * By Shadow Wolf and Lone Wolf, Wolf Territories Grid and IntelligentWolf Ltd
 * Copyright 2026 IntelligentWolf Ltd
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

//! Per-listener spatial mixing.
//!
//! This is the part a Janus AudioBridge cannot do. AudioBridge produces ONE shared
//! mix per room (its `spatial_audio` option only pans each participant inside that
//! single shared stereo image), whereas Linden Lab's design requires the mix to be
//! computed from each LISTENER's own position and orientation: A hears B on their
//! left while B simultaneously hears A on their right.
//!
//! Everything runs on a 20 ms grid at 48 kHz:
//!   * every speaker is decoded to MONO (one decoder per speaker),
//!   * for each listener we sum every OTHER speaker with that listener's distance
//!     gain, per-user gain and mute applied, panned into stereo,
//!   * the result is encoded once per listener.
//!
//! Cost is O(N^2) frame adds per tick, which at the crowd sizes a region sees is
//! negligible; the Opus encodes (one per listener per 20 ms) dominate.

/// Opus internal clock for WebRTC. Source: llvoicewebrtc.cpp registers Opus at
/// 48000 in the SDP it offers; webrtc's MIME_TYPE_OPUS is defined at 48 kHz.
pub const SAMPLE_RATE: u32 = 48_000;

/// 20 ms of audio at 48 kHz, per channel. This is the standard WebRTC Opus frame
/// and what the viewer's encoder produces.
pub const FRAME_SAMPLES: usize = 960;

/// Distance model constants.
///
/// These come from OpenSim's own spatial-voice channel defaults so that the
/// server-side mix matches what WolfStorm's client-side WebAudio panner already
/// does for its own users (js/voice/voice_webrtc.js:36-42 cites the same source):
///
/// Source: OpenSim VivoxVoiceModule.cs:80 —
///     CHAN_CLAMPING_DISTANCE_DEFAULT = 10   "distance before attenuation applies"
/// Source: OpenSim VivoxVoiceModule.cs:77 —
///     CHAN_MAX_RANGE_DEFAULT = 60           "distance at which channel is silent"
///
/// The curve is WebAudio's `linear` distance model with rolloffFactor 1.0:
///     gain = 1 - (d - ref) / (max - ref)     clamped to [0, 1]
/// Rolloff 1.0 rather than Vivox's CHAN_ROLL_OFF_DEFAULT of 2.0 because in the
/// linear model a rolloff of 2.0 reaches silence at ref + (max-ref)/2 = 35 m,
/// which would contradict the documented meaning of "max range = 60".
pub const REF_DISTANCE: f64 = 10.0;
pub const MAX_DISTANCE: f64 = 60.0;

/// Hard cutoff the VIEWER itself enforces on the listener position.
/// Source: llvoicewebrtc.cpp:88 MAX_AUDIO_DIST = 50.0f, applied in enforceTether
/// (:1188-1200) so a camera further than 50 m from the avatar cannot be used to
/// eavesdrop. We re-apply it here because the tether must be server-enforced —
/// llvoicewebrtc.cpp:1209-1212 says as much, and a modified client could simply
/// not clamp.
pub const MAX_AUDIO_DIST: f64 = 50.0;

/// Distance attenuation, linear model.
pub fn distance_gain(distance: f64) -> f32 {
    if distance <= REF_DISTANCE {
        return 1.0;
    }
    if distance >= MAX_DISTANCE {
        return 0.0;
    }
    (1.0 - (distance - REF_DISTANCE) / (MAX_DISTANCE - REF_DISTANCE)) as f32
}

/// Clamp a listener (ear) position to within MAX_AUDIO_DIST of the avatar.
///
/// Source: llvoicewebrtc.cpp:1188-1200 enforceTether — the same computation, done
/// client-side. Doing it again here is deliberate, not redundant.
pub fn tether(listener: [f64; 3], avatar: [f64; 3]) -> [f64; 3] {
    let off = [
        listener[0] - avatar[0],
        listener[1] - avatar[1],
        listener[2] - avatar[2],
    ];
    let d = (off[0] * off[0] + off[1] * off[1] + off[2] * off[2]).sqrt();
    if d <= MAX_AUDIO_DIST || d == 0.0 {
        return listener;
    }
    let k = MAX_AUDIO_DIST / d;
    [
        avatar[0] + off[0] * k,
        avatar[1] + off[1] * k,
        avatar[2] + off[2] * k,
    ]
}

/// Rotate a world-space vector into the listener's local frame using the
/// conjugate of the listener's orientation quaternion.
///
/// The quaternion arrives as (x, y, z, w) — llvoicewebrtc.cpp:1231-1236 writes
/// mListenerRot[0..3] in that order, and LLQuaternion stores x,y,z,w.
fn world_to_local(v: [f64; 3], q: [f64; 4]) -> [f64; 3] {
    // Firestorm sends quaternion components truncated to hundredths. Restore
    // unit length before using the quaternion-vector rotation formula.
    // Scale first so even very large finite components cannot overflow.
    let scale = q.iter().fold(0.0_f64, |m, x| m.max(x.abs()));
    let q = if q.iter().all(|x| x.is_finite()) && scale > 0.0 {
        let scaled = q.map(|x| x / scale);
        let norm = scaled.iter().map(|x| x * x).sum::<f64>().sqrt();
        scaled.map(|x| x / norm)
    } else {
        [0.0, 0.0, 0.0, 1.0]
    };
    // Conjugate: negate the vector part.
    let (qx, qy, qz, qw) = (-q[0], -q[1], -q[2], q[3]);
    // Standard quaternion-vector rotation: v' = q * v * q^-1, expanded.
    let tx = 2.0 * (qy * v[2] - qz * v[1]);
    let ty = 2.0 * (qz * v[0] - qx * v[2]);
    let tz = 2.0 * (qx * v[1] - qy * v[0]);
    [
        v[0] + qw * tx + (qy * tz - qz * ty),
        v[1] + qw * ty + (qz * tx - qx * tz),
        v[2] + qw * tz + (qx * ty - qy * tx),
    ]
}

/// Constant-power stereo pan weights (left, right) for a speaker heard by a
/// listener at the given orientation.
///
/// SL/OpenSim is Z-up. The viewer listener frame is +X forward, +Y left,
/// +Z up (LLCoordFrame::getAtAxis/getLeftAxis/getUpAxis). Thus local -Y is right.
/// Include elevation in the direction length so a source overhead stays centred.
/// We use
/// the constant-power (sin/cos) law so a source moving across the front does not
/// change apparent loudness.
pub fn pan_weights(speaker_world: [f64; 3], listener_pos: [f64; 3], listener_rot: [f64; 4]) -> (f32, f32) {
    let rel = [
        speaker_world[0] - listener_pos[0],
        speaker_world[1] - listener_pos[1],
        speaker_world[2] - listener_pos[2],
    ];
    let local = world_to_local(rel, listener_rot);
    let distance = local[0].hypot(local[1]).hypot(local[2]);
    // A coincident speaker has no direction: centre it.
    let lateral = if distance < 1e-6 { 0.0 } else { -local[1] / distance };
    // lateral is -1 (hard left) .. +1 (hard right); map to 0..PI/2.
    let angle = (lateral.clamp(-1.0, 1.0) + 1.0) * (std::f64::consts::FRAC_PI_4);
    (angle.cos() as f32, angle.sin() as f32)
}

/// RMS level of a mono frame, mapped to the viewer's 0-128 `p` scale.
///
/// Source: llvoicewebrtc.cpp:3227 — the viewer computes mLevel = p / 128.0, and
/// llvoicewebrtc.cpp:1938 treats mLevel > SPEAKING_AUDIO_LEVEL (0.30) as speaking.
pub fn level_to_wire(frame: &[f32]) -> u8 {
    if frame.is_empty() {
        return 0;
    }
    let sum: f32 = frame.iter().map(|s| s * s).sum();
    let rms = (sum / frame.len() as f32).sqrt();
    // RMS of full-scale speech sits well below 1.0; scale so normal speech lands
    // above the 0.30 speaking threshold without clipping the top of the range.
    let scaled = (rms * 4.0).min(1.0);
    (scaled * crate::proto::LEVEL_SCALE_TO_WIRE).min(127.0) as u8
}

/// One speaker's contribution as seen by one listener.
pub struct Contribution<'a> {
    pub frame: &'a [f32],
    pub gain: f32,
    pub pan: (f32, f32),
}

/// Sum contributions into an interleaved stereo buffer, soft-clipped.
///
/// `out` must be 2 * FRAME_SAMPLES long. Uses tanh soft clipping rather than hard
/// clamping so a crowded room degrades gracefully instead of buzzing.
pub fn mix_stereo(contributions: &[Contribution<'_>], out: &mut [f32]) {
    debug_assert_eq!(out.len(), FRAME_SAMPLES * 2);
    out.fill(0.0);
    for c in contributions {
        if c.gain <= 0.0 {
            continue;
        }
        let gl = c.gain * c.pan.0;
        let gr = c.gain * c.pan.1;
        for (i, s) in c.frame.iter().take(FRAME_SAMPLES).enumerate() {
            out[i * 2] += s * gl;
            out[i * 2 + 1] += s * gr;
        }
    }
    for s in out.iter_mut() {
        if *s > 1.0 || *s < -1.0 {
            *s = s.tanh();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distance_gain_matches_documented_range() {
        // Inside the clamping distance: no attenuation at all.
        assert_eq!(distance_gain(0.0), 1.0);
        assert_eq!(distance_gain(10.0), 1.0);
        // Silent at and beyond the documented max range.
        assert_eq!(distance_gain(60.0), 0.0);
        assert_eq!(distance_gain(1000.0), 0.0);
        // Half way between ref and max is half gain in the linear model.
        assert!((distance_gain(35.0) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn tether_clamps_beyond_fifty_metres() {
        let avatar = [0.0, 0.0, 0.0];
        // 100 m east must come back to exactly 50 m east.
        let t = tether([100.0, 0.0, 0.0], avatar);
        assert!((t[0] - 50.0).abs() < 1e-9);
        // Inside the tether is untouched.
        let t = tether([10.0, 0.0, 0.0], avatar);
        assert_eq!(t, [10.0, 0.0, 0.0]);
    }

    #[test]
    fn pan_uses_the_viewer_local_left_axis() {
        // Identity rotation: +X forward, +Y left, -Y right.
        let ident = [0.0, 0.0, 0.0, 1.0];
        let (l, r) = pan_weights([0.0, -10.0, 0.0], [0.0, 0.0, 0.0], ident);
        assert!(r > l, "local -Y should be louder on the right, got l={l} r={r}");

        let (l, r) = pan_weights([0.0, 10.0, 0.0], [0.0, 0.0, 0.0], ident);
        assert!(l > r, "local +Y should be louder on the left, got l={l} r={r}");
    }

    #[test]
    fn pan_is_constant_power() {
        let ident = [0.0, 0.0, 0.0, 1.0];
        for pos in [[10.0, 0.0, 0.0], [0.0, 10.0, 0.0], [-3.0, 4.0, 0.0]] {
            let (l, r) = pan_weights(pos, [0.0, 0.0, 0.0], ident);
            let power = l * l + r * r;
            assert!((power - 1.0).abs() < 1e-5, "power {power} for {pos:?}");
        }
    }

    #[test]
    fn straight_ahead_is_centred() {
        let ident = [0.0, 0.0, 0.0, 1.0];
        let (l, r) = pan_weights([10.0, 0.0, 0.0], [0.0, 0.0, 0.0], ident);
        assert!((l - r).abs() < 1e-5, "front should be centred, got l={l} r={r}");
    }

    #[test]
    fn mix_writes_interleaved_stereo_and_soft_clips() {
        let frame = vec![0.5f32; FRAME_SAMPLES];
        let contribs = vec![Contribution {
            frame: &frame,
            gain: 1.0,
            pan: (1.0, 0.0),
        }];
        let mut out = vec![0.0f32; FRAME_SAMPLES * 2];
        mix_stereo(&contribs, &mut out);
        assert!((out[0] - 0.5).abs() < 1e-6, "left carries the signal");
        assert_eq!(out[1], 0.0, "hard-left pan leaves the right channel silent");

        // Many loud sources must not exceed full scale.
        let loud = vec![1.0f32; FRAME_SAMPLES];
        let many: Vec<Contribution<'_>> = (0..8)
            .map(|_| Contribution { frame: &loud, gain: 1.0, pan: (1.0, 1.0) })
            .collect();
        mix_stereo(&many, &mut out);
        assert!(out.iter().all(|s| s.abs() <= 1.0), "soft clip bounds output");
    }

    #[test]
    fn silent_frame_reports_zero_level() {
        assert_eq!(level_to_wire(&[0.0; FRAME_SAMPLES]), 0);
        // A loud frame must clear the viewer's speaking threshold of 0.30.
        let loud = level_to_wire(&[0.5; FRAME_SAMPLES]);
        assert!(
            (loud as f32 / crate::proto::LEVEL_SCALE_TO_WIRE) > crate::proto::SPEAKING_AUDIO_LEVEL,
            "loud frame level {loud} should read as speaking"
        );
    }
}
