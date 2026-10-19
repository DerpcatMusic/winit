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

## Fork CI qualification and inherited failures

The first PR run, [39186113705](https://github.com/DerpcatMusic/winit/actions/runs/39186113705), at `6780ecbff344d25686d0a64edd9acedddd9b1ebf` failed formatting in the newly added native test. The file had been formatted with stable rustfmt, which ignores this repository's nightly import/macro formatting settings. That regression is corrected using nightly rustfmt; `cargo +nightly fmt -- --check` passes.

The platform jobs failed before compiling winit. Their unchanged lock-generation script ends in `cargo update -p syn --precise 2.0.114`, which is ambiguous once current dependency resolution includes both syn 2.0.119 and syn 3.0.9. Sample exact logs: [X11](https://github.com/DerpcatMusic/winit/actions/runs/39186113705/job/117848785597), [Windows](https://github.com/DerpcatMusic/winit/actions/runs/39186113705/job/117848785445), [macOS](https://github.com/DerpcatMusic/winit/actions/runs/39186113705/job/117848785415). The Linux/web cargo-deny jobs also reject duplicate syn majors under the existing deny policy.

Independent baseline reproduction used a fresh worktree at the exact unmodified upstream tag `e9809ef54b18499bb4f2cac945719ecc2a61061b`. Running every lock-generation command from the original workflow in order reproduced the identical ambiguity. Running cargo-deny on that baseline reproduced the syn duplicate ban. Local cargo-deny 0.20.2 additionally reports inherited license/advisory failures; this is a newer checker than the original CI action and those additional diagnostics are not asserted to be identical to the CI job. The workflow blob is unchanged from the upstream tag (SHA-256 `8490a47a6e7d2d1d2dff32acb0a537dd58bafbf3119803777207f4dfc6e8cb01`), as are both package manifests and deny policy.

Qualifying the intended update as `cargo update -p syn@2 --precise 2.0.114` succeeds on that baseline, but does not restore MSRV qualification: current resolution still includes dependencies declaring Rust versions above 1.70, including log/polling/proc-macro2 at 1.71 and Android activity at 1.85. Upstream 1.70 compatibility is **not** requalified by this patch. The package's upstream rust-version declaration is retained; solving its rolling dependency/MSRV/security policy requires separate compatibility work.

The additive `Matari XDND qualification` workflow uses Rust **1.98.1** and the checked-in `.github/ci/matari-0.30.13.lock` snapshot (SHA-256 `a4a8d044eea2c14657b99efe3a2f0b9b02a8ad8dd3094bccf5580d98ae3e8800`). It runs default library/integration tests for both the exact upstream baseline and patched source on Linux, Windows and macOS using the same dependency graph. The patched Linux job explicitly runs the ignored native test under an owned Xvfb server. Its lock is installed as Cargo.lock and all Cargo commands use `--locked`; the existing broad CI's floating-resolution policy is not changed.

This workflow is a narrow fork qualification, not a replacement for the original CI, a cargo-deny exemption, MSRV proof or desktop application evidence. The original jobs remain present with their original names and failures. Do not describe the PR as all-green while those checks remain red. A passing Windows/macOS job qualifies default compilation/unit/integration tests, not native file-drop support on those systems.
