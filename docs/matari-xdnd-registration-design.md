# Proposed live-window XDND registration capability

This is the design and qualification checklist for a separate prerequisite after
release commit `9f16e4093a42b180d78496ca285832a6b2d7b069`. It is not a transfer
target, a MUI dependency change, or desktop application qualification. The
implementation requires independent source/native review before consumer use.

## Scope and public boundary

The proposed optional Linux/X11 `external-xdnd` feature defaults off. Existing
windows keep the existing built-in behavior. Only a live window constructed
with `with_x11_drag_and_drop(false)` can reserve a registration. The existing
early built-in suppression remains essential even when a peer bypasses a proxy.

`WindowExtX11::prepare_external_xdnd` returns an opaque host lease and one
pending worker capability. Preparation performs memory operations only; it
does not query the server, publish properties, open a display or consume events.
The lease exposes scalar status and atomic logical revocation. Lease Drop does
not join a worker or perform native cleanup. Window Drop and DestroyNotify
invalidate the live generation before ordinary host teardown.

The pending capability contains bounded challenge metadata and no active native
connection. Its Drop releases the reservation without native work. Activation
must occur on a different thread from preparation, checked before any I/O. The
activated worker is not Send or Sync, so it and its native cleanup remain on
that backend thread. No raw pointer, original connection, general property
operation, callback under a native transaction, or event-queue API is exposed.
The implemented activation is `ExternalXdndPending::connect`; it checks the
actual native creation thread, even if prepare was called on another thread.

## Creation identity and server binding

Retaining an Arc to the connection does not keep a native window alive. A
worker must never publish its first proof using only a saved window ID: the
window can have been destroyed and its ID reused before that request executes.

Instead, feature-enabled opt-out window construction installs a bounded fresh
creation proof before the Window handle escapes. This is part of the existing
native window construction, not a later UI-frame preparation operation. The
proof identifies that particular native window generation. It is immutable
host lifetime metadata; registration cleanup never rewrites or deletes it.

The capability captures the original connection's actual display address,
target ID, root and creation proof. It never rereads mutable DISPLAY to choose
a server. A dedicated worker connection uses the captured address and must
validate the exact proof and root on the actual target before changing it. A
wrong server, destroyed/recreated target, wrong property type, wrong format,
wrong length or trailing property bytes fails closed.

The public proof is an accidental identity/ABA binding, not authentication
against hostile X clients. X11 clients with sufficient access can inspect and
replace properties. No security claim is made against such a client.

## Registration transaction under review

X11 has no general conditional property update. A check followed by a write or
delete can race another client or host destruction. A server grab on the
original connection does not exclude other requests on that same connection,
including UI-thread Destroy/Create. The proposed transaction therefore uses a
separate worker connection which has already proved the target's creation
identity. Its grab excludes the original connection as well as external peers.

All connection setup, authentication, atom interning, entropy, proxy creation,
application-state allocation and backend work happen before the grab. Bounded
protocol-library reply/parser allocations can still occur. Under the grab the
code performs only a fixed set of bounded property/root queries and writes.
There are no callbacks, event-draining loops, mutex waits, original-connection
requests or payload transfers. Install validates the creation proof, target
root, unique registration token, proxy proof, proxy self-reference and expected
XdndAware type/format/version. It rejects a pre-existing target registration;
it does not overwrite or save a competing adapter's state for later restoration.

The cleanup guard is armed before attempting to send GrabServer, since a send
error can occur after bytes reached the server. Every exit queues UngrabServer
and flushes within the transaction deadline; ungrabbing when no grab was
acquired is harmless. If that fails, the dedicated connection is closed. A
panic cannot retain a live grabbed connection through a retained outer owner.

Cleanup revalidates the target creation proof and the complete installed tuple,
including exact property types, formats, lengths, bytes_after, registration
token, proxy ID, proxy proof/self-reference and Aware properties. On any
replacement or malformed value it leaves target properties alone and reports
superseded/uncertain state. It never partly restores a remembered property
snapshot. This prevents an old generation's cleanup from deleting a replacement.

## Grab liveness and native outcome

A server grab stalls all other clients, even though the operation runs off the
UI thread. Calling the worker method off-thread alone does not make it safe.
The fail-close transport implements x11rb's nonblocking Stream interface with
private narrow libc send/recvmsg/poll wrappers. It checks one absolute 500 ms
transaction deadline in reads, writes, vectored writes, polls and between fixed
protocol steps. Interrupted syscalls retry only after checking that same deadline.
Continuous readable traffic cannot reset it. The transport also limits each
packet to 256 KiB, and the entire worker connection lifetime to 1 MiB and 256
received packets, including setup. Transaction restarts never reset traffic
budgets: x11rb may retain broadcast events between admission retries, and that
retained queue must remain bounded even if a caller retries indefinitely. It
validates packet headers before exposing their lengths to the protocol allocator.
Unexpected incoming/outgoing file descriptors are rejected. An expired or failed
transport is unusable and is closed, releasing a grab when the server processes
the disconnect. A stalled reply must not rely on a later cooperative flag check.

This still cannot promise a fixed desktop latency: the process may be paused,
the OS may not schedule it, or the server may stop responding. The deadline
transport and its shutdown/error/panic behavior require independent review and
fault-injection qualification before native implementation is accepted. No
unreviewed generic protocol stack or watchdog service is introduced.

The supported feature graph excludes **x11rb's `tracing` feature**. Enabling
it through Cargo feature unification can run tracing subscribers/global log
callbacks in library requests under the grab, outside the transport deadline.
A NoSubscriber scope alone cannot exclude log fallback. CI records and audits
`cargo tree -e features -i x11rb`; MUI and application integration must repeat
that audit. Arbitrary consumer feature combinations are not qualified. Winit's
ordinary logging remains outside this transaction implementation.

Revocation is immediate logical invalidation, not proof that native properties
have already disappeared. An already queued publication can reach the server
after revocation. The worker must reject further use, recheck revocation before
confirming install, and clean its matching state. Partial writes or failed
cleanup report native uncertainty. Destroyed targets report gone. Neither
failure nor a cleared in-memory reservation claims native detachment.

## Reservation lifecycle

Each live host generation has one bounded reservation slot and a checked local
sequence. The lease and worker share an unforgeable control allocation; callers
cannot construct another generation by supplying integer IDs. A closed host
cannot prepare again. An old lease cannot revoke or release a new control.

Dropping the pending worker before challenge/activation revokes and releases
its reservation. Dropping the host lease revokes but does not release an active
worker's ownership. A worker releases only after native cleanup or definitive
connection closure, recording any uncertainty. A retained old host lease does
not keep the next reservation Busy once the worker has released it. Failed
native cleanup can cause later admission to return a concrete competing-property
error, but must not leave an irrecoverable in-memory Busy state.

## Required qualification before MUI use

- Pure state tests: pending-worker Drop, host Drop, close/prepare races,
  release/reprepare with an old lease retained, sequence exhaustion, and old
  controls unable to revoke/release a replacement.
- Owned-Xvfb tests: default behavior remains unchanged; opt-out actual target,
  self-referencing proxy, install/cleanup/reinstall; competing registrations;
  malformed proof/type/format/length/trailing bytes; wrong server/root;
  revoked install; destroyed/recreated target; stale cleanup.
- Fault transport tests: failure before/while sending GrabServer, failed reply,
  partial install, expired read/write poll, error/panic ungrab, failed flush and
  final close. A second connection must resume after each failed transaction.
- Existing baseline/head Linux, Windows and macOS qualification remains present.
  New feature/native tests qualify Linux X11 only. Legacy upstream CI failures
  remain visible and are not renamed or waived.

Only after that prerequisite is independently reviewed and merged should MUI
add a default-off typed hook and a unified immutable winit patch. MUI retains
`forbid(unsafe_code)`. The file manager still needs a real bounded XDND transport
and verified operation completion; this registration primitive does not claim
native drop success.

Reference: the Xlib specification describes server grabs as stopping processing
and close-downs on all other connections and warns against holding them for a
long time: [Grabbing the Server](https://xorg.freedesktop.org/archive/current/doc/libX11/libX11/libX11.html#Grabbing_the_Server).
