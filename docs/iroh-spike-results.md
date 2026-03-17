# Iroh WASM Validation Spike Results

## Summary

iroh 0.97 and iroh-gossip 0.97 both compile to `wasm32-unknown-unknown` and the native integration tests pass. The WASM binary size impact is modest (~100KB). One toolchain issue needs to be addressed before CI/production builds can target WASM.

## Native Compilation

`cargo check -p sync-core` passes cleanly. The `native` feature gate correctly enables the full iroh networking stack (including tokio, QUIC, and TLS) for native builds.

Two integration tests in `crates/sync-core/tests/iroh_gossip_spike.rs` both pass:

- `gossip_two_peers_can_exchange_messages` — B broadcasts a message; A receives it
- `gossip_neighbor_up_fires_on_both_sides` — `NeighborUp` fires on A and B after the swarm connects

Both tests complete in under 100ms on a local machine, with a 10s timeout as a safety net.

## WASM Compilation

### Outcome

WASM compilation succeeds with one workaround (see below). `cargo check -p sync-core --target wasm32-unknown-unknown --no-default-features` passes cleanly.

### The ring / Apple Clang blocker

The `ring` crate (pulled in transitively via iroh → noq → rustls → ring) fails to compile with macOS's built-in Apple Clang because Apple Clang does not support the `wasm32-unknown-unknown` target triple for C code.

`ring` is not optional in iroh 0.97 — it is a hard dependency of the QUIC stack (`noq`) that iroh uses for TLS. There is no iroh feature flag that removes it.

**Workaround:** Use LLVM Clang from Homebrew instead of Apple's system Clang:

```sh
CC=/opt/homebrew/opt/llvm/bin/clang AR=/opt/homebrew/opt/llvm/bin/llvm-ar cargo check -p sync-core --target wasm32-unknown-unknown --no-default-features
```

This is a local development issue only. CI environments (Linux, GitHub Actions) use standard LLVM toolchains and do not hit this problem. Docker-based builds are unaffected.

### Feature architecture

The `native` feature in `sync-core` gates everything that doesn't compile to WASM:

```toml
[features]
default = ["native"]
native = ["iroh/default", "iroh-gossip/net", "dep:tokio"]
```

WASM builds must use `--no-default-features` or `default-features = false`. The `sync-wasm` crate's `Cargo.toml` has been updated to pass `default-features = false` when depending on `sync-core`.

## WASM Binary Size Impact

| Build | Binary Size |
|-------|-------------|
| Before iroh (baseline) | 4.4 MB |
| After iroh | 4.5 MB |
| **Delta** | **~100 KB** |

The baseline was a release build of `sync-wasm` from before the iroh dependencies were added. The "after" build includes all iroh and iroh-gossip types compiled into the WASM module (though the actual networking code is behind the `native` feature and not present in the WASM binary). The ~100KB increase is the iroh identity types, base protocol types, and gossip data structures.

Note: these are unoptimized WASM binaries. The Obsidian plugin pipeline runs `wasm-opt` and other size reduction passes that will compress both numbers further, preserving the delta relationship.

## Surprises and Concerns

**QUIC is always compiled into WASM (for type compatibility).** Even with `default-features = false`, iroh's QUIC types are present in the WASM binary because iroh's data types reference them. Only the networking *code* (socket binding, relay connections) is gated behind the `native` feature. This is the expected iroh design: the same types are used on all platforms.

**iroh 0.97 does not have a "WASM-only" feature split.** There's no way to compile *only* the identity/key types without also pulling in the full noq/rustls/ring stack at the type level. This is acceptable — the ring C code compiles fine with LLVM Clang — but it means the Apple Clang workaround is a permanent requirement for local macOS WASM builds, not a temporary gap.

**relay-based connections are required for browser WASM.** The iroh docs confirm that `wasm32-unknown-unknown` targets cannot bind UDP sockets directly. All peer connections from browser/WASM contexts must flow through an iroh relay server. This is a known constraint and aligns with the planned architecture.

## Next Steps

1. Add the LLVM Clang env vars to the project's `.cargo/config.toml` for macOS WASM builds, or document in the contributing guide.
2. Ensure CI uses a Linux runner for WASM checks (no workaround needed there).
3. Proceed with the iroh migration — the feasibility validation is confirmed.
