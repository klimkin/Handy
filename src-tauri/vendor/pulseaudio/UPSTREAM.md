# PulseAudio receive-buffer patch

Published crate `pulseaudio` 0.3.1 comes from
<https://crates.io/crates/pulseaudio/0.3.1> and upstream
<https://github.com/colinmarc/pulseaudio-rs> at revision
`fe59d48aeded261d9edbce43c823cc8c13f80e5a`.

The registry archive SHA-256 is
`d70623bd7967a9ca4c2ae0e807fc380b291f98480fc037042305ec643a4d3373`.
All published package files, including the MIT license, original manifest,
examples, and test fixture, are preserved. Registry cache markers and build
outputs are excluded.

The sole production change is in `src/client/reactor.rs`, `Reactor::recv`.
`self.read_buf.resize(off + 1024 * 1024, 0)` becomes
`self.read_buf.resize(off + 16 * 1024, 0)`. Each socket attempt initializes
16 KiB instead of 1 MiB. The existing accumulation, parser, dispatch, and
read-until-`WouldBlock` loop remain unchanged. The chunk size does not limit
protocol packet size. Large queued bursts require more socket reads.

Local unit tests in the same module exercise real nonblocking Unix socket
pairs without an audio server. They cover fragmented headers and payloads,
repeated `WouldBlock`, coalesced packets, payloads through the protocol's 4 MiB
maximum, control replies and errors, and EOF after complete or partial frames.
Callbacks must receive exact bytes once, in order, without added padding.

Run tests with `cargo test --manifest-path src-tauri/vendor/pulseaudio/Cargo.toml --lib`.
Preserve or explicitly retire this patch and its tests when updating the crate.
