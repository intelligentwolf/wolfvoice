# Spatial listener axes correction

## Problem

The mixer used local +X as listener-right. In Firestorm's listener basis, +X is
forward, +Y is left and +Z is up. With identity rotation, a source directly ahead
therefore panned fully right, while sources to the left/right were centred.

## Source convention

- [Firestorm LLCoordFrame](https://github.com/FirestormViewer/phoenix-firestorm/blob/master/indra/llmath/llcoordframe.h):
  getAtAxis() returns X, getLeftAxis() returns Y, getUpAxis() returns Z.
- [OpenSim ScenePresence direction vectors](https://github.com/opensim/opensim/blob/eb01e50a678ce69c67bf7f9cd8865a28f7d2224a/OpenSim/Region/Framework/Scenes/ScenePresence.cs#L397-L404):
  forward is +X, left is +Y and up is +Z.
- [Firestorm voice position/orientation](https://github.com/FirestormViewer/phoenix-firestorm/blob/master/indra/newview/llvoicewebrtc.cpp):
  updatePosition() selects the listener frame; sendPositionUpdate() quantizes
  quaternion components with integer(value * 100).

World/region axes are east/north/up. These must not be confused with the rotated
local listener axes forward/left/up. No special OpenSim/SL axis swap is needed
for this viewer protocol.

## Fix

Compute the relative world vector, normalize the listener quaternion, then
inverse-rotate into listener space. The existing expanded inverse rotation is
retained; normalization restores its unit-quaternion precondition.

Replace:

```text
lateral = local.x / sqrt(local.x^2 + local.y^2)
```

with:

```text
length = hypot(hypot(local.x, local.y), local.z)
lateral = -local.y / length
angle = (clamp(lateral, -1, 1) + 1) * pi/4
left = cos(angle)
right = sin(angle)
```

A coincident source is centred. The full 3D length avoids extreme pan for a
nearly overhead source with a small lateral offset.

For identity listener rotation:

| Source | Old L/R | Corrected L/R |
| --- | --- | --- |
| Front (+X) | 0 / 1 | 0.707 / 0.707 |
| Back (-X) | 1 / 0 | 0.707 / 0.707 |
| Left (+Y) | 0.707 / 0.707 | 1 / 0 |
| Right (-Y) | 0.707 / 0.707 | 0 / 1 |

Quaternion normalization first scales by the largest absolute component to avoid
overflow, then normalizes. Zero/non-finite values use identity as a finite fallback.

## Scope

Distance attenuation, per-user gain/mute, participant levels, private calls,
credentials and the wire protocol are unchanged. No new viewer setting, region
DLL, key or port is required. This remains a stereo panner rather than an HRTF model.

Both the old and new laws preserve total stereo power (L^2 + R^2 = 1).
The confirmed axis error explains incorrect direction, but alone does not prove
the cause of complete two-channel silence or every camera-related symptom.

The operator reported improved in-world results with the corrected build on
28 October 2026, after earlier reports of quiet voice while following a speaker.
This is field feedback, not a controlled measurement of every viewer mode.

## Tests

- Existing mixer tests updated to the actual listener basis.
- Standalone geometry suite: front/back, left/right, full yaw with wire rounding,
  pitch, elevation, common translation, constant stereo power and invalid/large
  quaternion handling.
- Room-level test follows a speaker at constant 3 m through changing global
  coordinates and speaker rotation, checking centred audio and stable energy.
- Geometry CI reproduces the front-source failure on the pinned unmodified
  upstream baseline, then runs the corrected suite.
- Native CI installs libopus-dev/pkg-config so tests avoid the vendored Opus
  CMake 4 incompatibility; this is a CI prerequisite, not a runtime behavior change.

Run geometry tests independently of Opus:

```sh
rustc --edition=2021 --test tests/spatial_geometry.rs -o /tmp/spatial-geometry
/tmp/spatial-geometry
```

The full suite is run with cargo test --locked; clippy and example builds remain
covered by the existing CI. Runtime changes are limited to src/mixer.rs; changes
to src/room.rs are test-only.

Binary packaging and baseline-CPU compatibility changes from the downstream
deployment are intentionally separate from this source correction.
