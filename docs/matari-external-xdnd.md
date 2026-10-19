# External XDND prerequisite (0.30.13 fork)

Based exactly on upstream `v0.30.13`, peeled commit `e9809ef54b18499bb4f2cac945719ecc2a61061b`. The existing package version, licenses and default behavior remain unchanged. This is a narrow prerequisite for an external, real-window-bound drop adapter; it does not implement that adapter or qualify a file manager's drag/drop operations.

`WindowAttributesExtX11::with_x11_drag_and_drop(false)` is an immutable per-window opt-out. It prevents built-in `XdndAware` advertisement and ignores `XdndEnter`, `XdndPosition`, `XdndDrop`, `XdndLeave` and selection notifications addressed to that target before parsing, source requests, shared built-in Dnd mutation, path canonicalization, file notifications or automatic Status/Finished replies. The default remains `true`; Wayland, Windows and macOS are unchanged. Unknown/destroyed target IDs cannot activate the built-in handler.

Removing a property or ignoring `DroppedFile` callbacks alone is insufficient: noncompliant/direct peers can still address the real window, and built-in replies occur before application operations finish. An external adapter must opt out **before creating** its window, then establish its own server-bound lifetime and advertisement/proxy ownership. A proxy is not an unrelated fake target: it must be self-referencing, attached to the real window, and retain the real target identity in message fields. This patch intentionally exposes no generic raw event, native connection or unsafe-pointer callback. It does not authenticate peers or provide a server-bound registration capability; that is a separate reviewed integration.

Native fixture qualification:

```sh
xvfb-run -a cargo test --locked --no-default-features --features x11,rwh_06 \
  --test x11_external_dnd -- --ignored --nocapture
cargo check --locked --no-default-features --features x11,rwh_06
```

The owned-server test creates actual default-enabled and external-disabled winit windows on one event loop, with an independent X11 peer. It verifies default advertisement, no built-in external advertisement, self-validating proxy delivery of Enter/Position/Drop/Leave, direct messages that bypass an installed proxy, repeated malformed selection notifications, unknown and destroyed/stale target fields, and disabled recreation. A valid default drag remains live across those attacks, and emits the expected native drop and successful built-in Finished twice. Explicit window-event markers synchronize processing; sleeps are not synchronization. Only an owned dev-drive file fixture is read/canonicalized by the unchanged enabled parser.

The integration test is ignored by default because it requires an owned X server. It exercises protocol suppression/compatibility, not a production external transfer transport or application mutation result. Native selection/INCR bounds, cancellation, verified operation completion and proxy registration teardown remain the adapter's responsibility.

On Rust 1.98.1, the strict scoped Clippy command encounters four unchanged upstream `clippy::manual_filter`/`clippy::iter_kv_map` findings (`common/xkb/compose.rs` and the existing X11 event replay loop). With only those two categories forced to warnings, the scoped library and native test check passes:

```sh
cargo clippy --locked --no-default-features --features x11,rwh_06 \
  --lib --test x11_external_dnd -- -D warnings \
  --force-warn clippy::manual_filter --force-warn clippy::iter_kv_map
```

No release, application, Wayland compositor or other-platform qualification was run. Consumers must pin a reviewed immutable fork commit and apply one workspace-wide `patch.crates-io` when mixing this fork with registry-winit adapters; duplicate source copies would produce incompatible Window types. The unrelated fork default branch and outbound-Wayland work are not modified by this release branch.
