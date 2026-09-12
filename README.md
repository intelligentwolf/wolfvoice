# wolfvoice

Spatial WebRTC voice for [OpenSimulator](http://opensimulator.org) — one service that
Firestorm and browser-based viewers both reach through the region's own capabilities,
so users of different viewers hear each other, positionally.

wolfvoice is a **voice service backend for
[os-webrtc-janus](https://github.com/wolfsoftwaresystemsltd/os-webrtc-janus)**. That addon does the
OpenSimulator half — capabilities, provider advertisement, session bookkeeping — and
wolfvoice is an alternative to point it at when you want the mixing done differently.

```
Firestorm 7.1.10+  /  browser viewer
        │  ProvisionVoiceAccountRequest + VoiceSignalingRequest   (LLSD caps)
        ▼
OpenSim region  +  os-webrtc-janus addon   ← REQUIRED
        │          forwards the caps onward as JSON-RPC
        ▼
   wolfvoice  ── answers the SDP, terminates DTLS/SRTP/SCTP,
        │         mixes a SEPARATE stream per listener
        ▼
   viewers   (UDP media)
```

## What you need

**os-webrtc-janus is a hard requirement.** It supplies `WebRtcVoice.dll` and the
region-side capability handlers that everything here depends on; without it there is
nothing for a viewer to talk to. See [docs/OPENSIM.md](docs/OPENSIM.md) for building it.

What you can skip is running a **Janus gateway server** alongside it. The addon ships
two backends: `WebRtcJanusService` (which drives Janus) and
`WebRtcVoiceServiceConnector` (a JSON-RPC client that will post to any URL). wolfvoice
plugs into the second, so you point the addon at wolfvoice instead of at Janus.

**For the voice server**

- A Linux host with a **public IP address** and a **DNS name** pointing at it. TLS is
  not optional: the SDP carries the server's DTLS fingerprint, so anyone able to
  rewrite plaintext signalling in flight could become the media endpoint.
- **UDP 40000–40999** open to the world. This range is also the ceiling on concurrent
  sessions — one socket per peer connection.
- **TCP 9443** reachable *from your region hosts only*. This endpoint has no
  authentication of its own.
- Modest CPU: **~17 ms per listener per second**, spread across all cores. See [Capacity](#capacity).

**For viewers**

- **Firestorm 7.1.10 or newer** works with no configuration at all — the region tells
  it which voice system to use. Nothing to install, nothing to tick.
- Browser viewers need a client speaking the same contract.
  [`client/voice_llwebrtc.js`](client/voice_llwebrtc.js) is a working reference
  implementation, extracted from WolfStorm.

## Why a per-listener mix

Linden Lab's WebRTC voice puts spatialisation, per-user gain and mute on the
**server**: the viewer sends its position, gain and mute requests *up* a data channel
and receives one already-mixed audio stream plus a participant roster *down* it.

wolfvoice therefore computes **a distinct mix for every listener**. If A is east of B,
A hears B on their right while B simultaneously hears A on their left — which one
shared room mix cannot express, however it is panned.

Janus's AudioBridge plugin is a conference mixer: it produces a single mix for a room,
and it does not carry WebRTC data channels (no `incoming_data` callback, and Janus's
SDP helper declines an `m=application` line no plugin has claimed). Those are perfectly
reasonable choices for what AudioBridge is for, but they mean the parts of LL's
protocol that ride the data channel — position, per-user gain, mute, the speaker
roster — have nowhere to travel, and the viewer will not consider voice ready until
that channel opens. wolfvoice accepts the data channel and does the mixing itself,
which is the whole reason it exists.

If you want a straightforward conference-style voice service, os-webrtc-janus with
Janus is a simpler deployment and the sensible default. Choose wolfvoice when you want
positional audio.

## Quick start

### 1. Install the voice server

```bash
curl -fsSLO https://raw.githubusercontent.com/wolfsoftwaresystemsltd/wolfvoice/main/setup.sh
less setup.sh          # it runs as root; read it first
sudo bash setup.sh
```

The installer detects your platform, downloads and checksums the matching release
binary, creates an unprivileged service user, obtains a TLS certificate with certbot
(including renewal hooks), and installs a hardened systemd unit.

### 2. Build os-webrtc-janus into OpenSim

**First, check whether your OpenSim already ships it.** Since 26 Feb 2026 the addon
is integrated into both upstream OpenSimulator and OpenSim-NGC — if either directory
below exists, **do not clone anything** (a second copy in `addon-modules/` breaks the
build with duplicate project names). Just build OpenSim the way you normally do and
skip to step 3:

```bash
ls OpenSim/Addons/os-webrtc-janus 2>/dev/null   # upstream master
ls Addons/os-webrtc-janus         2>/dev/null   # OpenSim-NGC
```

On an **older tree** without it, clone the Wolf fork's fixed branch (upstream
`Misterblue/os-webrtc-janus` is archived, and its `main` still has the
`ChatSessionRequest` bug that broke Firestorm text IMs grid-wide whenever WebRTC
voice was enabled):

```bash
cd opensim/addon-modules
git clone -b chatsession-p2p-session-id-and-fast-fail https://github.com/wolfsoftwaresystemsltd/os-webrtc-janus.git os-webrtc-janus
cd ..
```

Then build. **Which build you have is told by the tree itself:**

**A. Classic tree — `runprebuild.sh` exists at the OpenSim root**
(opensimulator.org 0.9.2.x / early 0.9.3):

```bash
./runprebuild.sh    # also (re)generates compile.sh — it's normal for it to be missing before this
./compile.sh        # or: dotnet build -c Release OpenSim.sln
```

Prebuild picks the module up automatically and the DLLs land directly in `bin/`.

**B. dotnet-era tree — no `runprebuild.sh`, but `OpenSim.sln` and
`Directory.Build.props` at the root** (OpenSim-NGC snapshots; "the only .sh in the
tree is the addon's own updateVersion.sh" means you are here). Prebuild is gone and
the solution only builds projects registered in it, so register the module's four
projects once, then build:

```bash
dotnet sln OpenSim.sln add \
    addon-modules/os-webrtc-janus/WebRtcVoice/WebRtcVoice.csproj \
    addon-modules/os-webrtc-janus/WebRtcVoiceServiceModule/WebRtcVoiceServiceModule.csproj \
    addon-modules/os-webrtc-janus/WebRtcVoiceRegionModule/WebRtcVoiceRegionModule.csproj \
    addon-modules/os-webrtc-janus/Janus/WebRtcJanusService.csproj
dotnet build --configuration Release OpenSim.sln
```

Build via the solution, not the individual csproj files — the tree's
`Directory.Build.props` computes output paths from the solution directory. Output
goes to `build/Release/<AssemblyName>/`, **not** `bin/`; copy the DLLs into the
`bin/` your regions actually run from:

```bash
cp build/Release/WebRtcVoice/WebRtcVoice.dll \
   build/Release/WebRtcVoiceServiceModule/WebRtcVoiceServiceModule.dll \
   build/Release/WebRtcVoiceRegionModule/WebRtcVoiceRegionModule.dll  /path/to/runtime/bin/
```

**C. Binary-only distribution** (no `OpenSim.sln`, no source directories — just
`bin/`): you cannot build the addon there. Build it inside a source tree of the
**exact same OpenSim version** using A or B, then copy the three `WebRtc*.dll`
files into the distribution's `bin/`. Mismatched versions will fail to load.

Whichever route: you need `WebRtcVoice.dll`, `WebRtcVoiceServiceModule.dll` and
`WebRtcVoiceRegionModule.dll` in the runtime `bin/` (the also-built
`WebRtcJanusService.dll` is simply unused when pointing at wolfvoice). The modules
load at startup — **restart the region** after copying.

### 3. Configure each region

Create **`<region>/bin/config/wolfvoice.ini`** — a copy of
[`contrib/wolfvoice.ini`](contrib/wolfvoice.ini) — with your server URL:

```ini
[WebRtcVoice]
    Enabled = true

    ; Point the addon's JSON-RPC connector at wolfvoice, for both spatial
    ; (region/parcel) and non-spatial (group, IM) voice.
    SpatialVoiceService = WebRtcVoice.dll:WebRtcVoiceServiceConnector
    NonSpatialVoiceService = WebRtcVoice.dll:WebRtcVoiceServiceConnector

    ; Your wolfvoice server. HTTPS, with a certificate .NET will trust.
    WebRtcVoiceServerURI = https://voice.example.org:9443

    ; Logs entire SDP bodies. Handy on one region while diagnosing.
    MessageDetails = false

[VivoxVoice]
    ; Turn Vivox off wherever WebRTC is on: both modules register the SAME
    ; capability name and the last registration wins, so leaving both enabled
    ; is a race rather than a choice.
    enabled = false
```

Then **restart the region**.

Why `bin/config/` rather than `OpenSim.ini`: OpenSim reads every `.ini` in that
directory *after* `OpenSim.ini` and treats them as overrides, so a shared or templated
`OpenSim.ini` can be regenerated without clobbering your voice settings. With one
OpenSim process per region it also scopes the change to that region alone. (In a
multi-region simulator, `Enabled` is read once per process, so it applies to every
region in it.)

### 4. Allow voice on the estate and the parcel

Both are required, and this is the most common reason for "nothing happens":

- Region/Estate → Estate → **Allow Voice Chat**
- About Land → Sound → **Allow Voice Chat**

With either unset the viewer decides locally that voice is unavailable and never asks
the region for anything — so **nothing appears in any log**. If you see
`setting VoiceServerType=webrtc` in the region log and then no provision request,
this is why.

### 5. Check it

```bash
# from a permitted region host
curl https://voice.example.org:9443/
# {"rooms":0,"service":"wolfvoice","sessions":0}
```

## Documentation

| | |
|---|---|
| [docs/SERVER.md](docs/SERVER.md) | Installing, running and operating the service; TLS; firewall; capacity |
| [docs/OPENSIM.md](docs/OPENSIM.md) | The addon, region configuration, estate/parcel flags, rolling out to many regions |
| [docs/CLIENT.md](docs/CLIENT.md) | Viewer support, and writing your own client |
| [docs/PROTOCOL.md](docs/PROTOCOL.md) | The complete wire contract, with citations into the Firestorm and OpenSim sources |
| [docs/TROUBLESHOOTING.md](docs/TROUBLESHOOTING.md) | Every failure we hit, and how to tell them apart |

## Capacity

Measured with `cargo run --release --example load_test` on a 12-core VM:

| Clients | Speaking | Service CPU | Per listener | Frames delivered |
|---|---|---|---|---|
| 10 | 5 | 12 % of one core | 12.3 ms/s | all |
| 40 | 20 | 57 % of one core | 14.2 ms/s | all |
| 120 | 60 | **203 % of one core** | 16.9 ms/s | all, no ticks dropped |

Cost is roughly linear in listeners — the per-listener Opus encode dominates and the
O(N²) frame summing is comparatively free.

The mixer parallelises on both available axes: each room's tick is its own task, and
each *listener within* a room is its own task too. That second axis is the one that
matters, because the interesting case is a single busy region. The 120-listener run
above is the proof: 203 % of one core means the work genuinely spans cores, and every
frame still arrived.

So the first limit you will now meet is the **media port range — 1000 concurrent
sessions**, not CPU (12 cores at ~17 ms per listener is on the order of 700
listeners before saturation). Widen `MEDIA_PORT_LO`/`HI` and your firewall together
if you need more.

If the host ever cannot keep up, a whole 20 ms tick is skipped rather than queued —
audible, but bounded — and the service logs `mixer overloaded` with a count.

## Building from source

Needs a Rust toolchain and `cmake` (the `opus` crate compiles libopus from source).

```bash
cargo build --release
cargo test              # 31 tests, no network required
```

**On Windows**, with a recent `cmake` (4.x) you also need:

```powershell
$env:CMAKE_POLICY_VERSION_MINIMUM = "3.5"
cargo build --release
```

libopus's vendored `CMakeLists.txt` still declares `cmake_minimum_required(VERSION
3.1)`, which CMake 4 refuses outright ("Compatibility with CMake < 3.5 has been
removed"). The env var above is CMake's own documented escape hatch — it configures
libopus as if it had declared at least that policy version, instead of erroring. A
new enough `cmake` to need this also recognizes current Visual Studio versions as a
generator, so this is a complete fix, not a workaround for half the problem.

Release binaries for Linux and Windows are built by GitHub Actions — see
[.github/workflows/release.yml](.github/workflows/release.yml). macOS is not built
there yet; the same fix likely applies but is unverified on that platform.

## Testing it without a viewer

```bash
# Correctness: two synthetic viewers, real SDP, real Opus, real mixing.
cargo run --release --example two_clients -- https://127.0.0.1:9443
# -> AUDIO_MIXED=true ROSTER_DELIVERED=true LEAVE_NOTIFIED=true / PASS

# Capacity: N clients, M speaking, reports service CPU.
cargo run --release --example load_test -- https://127.0.0.1:9443 40 20
```

Run these on the voice host (the JSON-RPC port is firewalled to region hosts, but
loopback is permitted). Both harnesses share the server's own WebRTC crate, so they
cannot catch a shared misreading of the wire format — a real viewer remains the final
test.

## Known limitations

- **No TURN, and no way to add one.** Firestorm hardcodes its STUN servers to
  `stun:stunN.<grid>.secondlife.io`, which do not resolve outside Second Life. Media
  still connects, because the server advertises a routable host candidate and learns
  the viewer's address from the connectivity check — but a user whose network blocks
  outbound UDP cannot use voice at all. The client is told, rather than failing
  silently.
- **Single instance.** If wolfvoice is down, voice is down everywhere it is
  configured. `Restart=always` covers a crash, not a host failure.
- **Group and IM voice** (`channel_type: multiagent`) is implemented and rooms are
  keyed correctly, but has had far less exercise than spatial voice.

## Credits

Built by **Wolf Software Systems Ltd** for the
[Wolf Territories Grid](https://wolfterritories.org), and released for the wider
OpenSimulator community.

wolfvoice stands on **[os-webrtc-janus](https://github.com/Misterblue/os-webrtc-janus)
by Robert Adams**, which brought WebRTC voice to OpenSimulator and supplies the
region-side capability handlers, provider advertisement and session plumbing this
service depends on. wolfvoice is only a backend for it; the hard integration work with
OpenSimulator is his.

The wire protocol is Linden Lab's, as implemented in
[Firestorm](https://www.firestormviewer.org/).

## Licence

Apache License 2.0 — see [LICENSE](LICENSE).
