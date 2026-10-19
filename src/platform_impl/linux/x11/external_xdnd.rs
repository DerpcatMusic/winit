//! Typed lifetime boundary for an external XDND adapter. No event queues are read.

use std::ffi::CStr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::{fmt, thread};

use x11rb::connection::{Connection, RequestConnection};
use x11rb::errors::ReplyError;
use x11rb::protocol::xproto::{self, AtomEnum, ConnectionExt as _, PropMode};
use x11rb::reexports::x11rb_protocol::parse_display::parse_display;
use x11rb::reexports::x11rb_protocol::xauth::get_auth;
use x11rb::rust_connection::{DefaultStream, RustConnection};
use x11rb::wrapper::ConnectionExt as _;

use super::external_xdnd_transport::DeadlineStream;
use super::{external_xdnd_state as state, XConnection};

const CREATION_PROPERTY: &str = "_WINIT_EXTERNAL_XDND_WINDOW";
const REGISTRATION_PROPERTY: &str = "_WINIT_EXTERNAL_XDND_REGISTRATION";
const TRANSACTION_TIME: Duration = Duration::from_millis(500);

/// A bounded registration failure; no native error text or payload is retained.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExternalXdndError {
    /// This capability currently supports Linux X11 only.
    UnsupportedBackend,
    /// The window did not opt out of the built-in XDND handler at construction.
    BuiltinEnabled,
    /// The host has been closed or its capability has been revoked.
    Revoked,
    /// Another worker still owns the one reservation.
    Busy,
    /// The checked per-window sequence is exhausted.
    Exhausted,
    /// Native worker activation was attempted on the native window's owner thread.
    HostThread,
    /// A registration property already exists, or has been replaced.
    Competing,
    /// A proof, type, format, length or trailing-byte check failed.
    MalformedProof,
    /// The target or proxy belongs to a different root.
    WrongRoot,
    /// The actual native target has been destroyed.
    WindowGone,
    /// This worker already installed a proxy or has finished.
    WrongPhase,
    /// A connection, transport deadline, byte budget or native request failed.
    Native,
}

impl fmt::Display for ExternalXdndError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "external XDND registration: {self:?}")
    }
}

impl std::error::Error for ExternalXdndError {}

/// Native facts are separate from immediate logical revocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExternalXdndPhase {
    /// No native worker connection has been activated.
    Reserved,
    /// A backend worker owns the connection; no proxy is confirmed installed.
    Active,
    /// The owned native tuple was installed and acknowledged.
    Installed,
    /// Matching target registration properties were removed, or never installed.
    Detached,
    /// The native target no longer exists.
    Gone,
    /// Current properties no longer match; cleanup left them untouched.
    Superseded,
    /// Native cleanup could not be established. This is not detachment.
    Uncertain,
}

/// Cheap scalar status. Logical revocation never promises immediate native removal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExternalXdndStatus {
    /// Host stopped accepting this generation, including during in-flight publication.
    pub revoked: bool,
    /// The worker relinquished its reservation; old leases cannot affect a new one.
    pub released: bool,
    /// Last established native fact.
    pub phase: ExternalXdndPhase,
}

impl From<state::Phase> for ExternalXdndPhase {
    fn from(phase: state::Phase) -> Self {
        match phase {
            state::Phase::Reserved => Self::Reserved,
            state::Phase::Active => Self::Active,
            state::Phase::Installed => Self::Installed,
            state::Phase::Detached => Self::Detached,
            state::Phase::Gone => Self::Gone,
            state::Phase::Superseded => Self::Superseded,
            state::Phase::Uncertain => Self::Uncertain,
        }
    }
}

/// Non-clone host control. Drop/revoke perform memory operations only, never join.
pub struct ExternalXdndLease(state::Lease);

impl ExternalXdndLease {
    /// Immediately invalidate this logical generation. Native cleanup is cooperative.
    pub fn revoke(&self) {
        self.0.revoke();
    }

    /// Read only scalar atomics; does not query the server.
    pub fn status(&self) -> ExternalXdndStatus {
        let status = self.0.status();
        ExternalXdndStatus {
            revoked: status.revoked,
            released: status.released,
            phase: status.phase.into(),
        }
    }
}

/// Bounded lifecycle proof, not authentication against other trusted X11 clients.
#[derive(Clone)]
pub struct ExternalXdndChallenge {
    display: Arc<str>,
    target: u32,
    root: u32,
    creation: [u8; 32],
    registration: [u8; 32],
}

impl ExternalXdndChallenge {
    /// Address captured from the original live connection, independent of DISPLAY.
    pub fn display(&self) -> &str {
        &self.display
    }
    /// Actual native host identity, never a replacement independent target.
    pub fn target(&self) -> u32 {
        self.target
    }
    /// Expected server root for the actual host and external proxy.
    pub fn root(&self) -> u32 {
        self.root
    }
    /// Immutable creation property; backend places the same typed bytes on its proxy.
    pub fn creation_property(&self) -> &'static str {
        CREATION_PROPERTY
    }
    /// Creation proof value; property type is its own atom, format is 8.
    pub fn creation_proof(&self) -> &[u8; 32] {
        &self.creation
    }
    /// Per-registration property; backend places the same typed bytes on its proxy.
    pub fn registration_property(&self) -> &'static str {
        REGISTRATION_PROPERTY
    }
    /// Registration proof value; property type is its own atom, format is 8.
    pub fn registration_proof(&self) -> &[u8; 32] {
        &self.registration
    }
}

pub(crate) struct Creation {
    display: Arc<str>,
    target: u32,
    root: u32,
    proof: [u8; 32],
    state: state::WindowState,
}

impl Creation {
    pub(crate) fn new(conn: &XConnection, target: u32) -> Result<Arc<Self>, ExternalXdndError> {
        // SAFETY: XDisplayString returns the live Xlib-owned nul-terminated
        // connection address. It remains valid while conn is borrowed here.
        let address = unsafe {
            let address = (conn.xlib.XDisplayString)(conn.display);
            if address.is_null() {
                return Err(ExternalXdndError::Native);
            }
            CStr::from_ptr(address)
        };
        let address = address.to_str().map_err(|_| ExternalXdndError::Native)?;
        if address.is_empty() || address.len() > 4096 {
            return Err(ExternalXdndError::Native);
        }
        let mut proof = [0; 32];
        let mut filled = 0;
        for _ in 0..64 {
            use rustix::rand::{getrandom, GetRandomFlags};
            match getrandom(&mut proof[filled..], GetRandomFlags::NONBLOCK) {
                Ok(n) if n != 0 => filled += n,
                Err(rustix::io::Errno::INTR) => continue,
                _ => return Err(ExternalXdndError::Native),
            }
            if filled == proof.len() {
                break;
            }
        }
        if filled != proof.len() {
            return Err(ExternalXdndError::Native);
        }
        let xcb = conn.xcb_connection();
        let atom =
            native(native(xcb.intern_atom(false, CREATION_PROPERTY.as_bytes()))?.reply())?.atom;
        let root = native(native(xcb.get_geometry(target))?.reply())?.root;
        // Publication belongs exclusively to original window construction,
        // before its handle escapes. A later worker never writes a saved bare ID.
        native(
            native(xcb.change_property8(PropMode::REPLACE, target, atom, atom, &proof))?.check(),
        )?;
        Ok(Arc::new(Self {
            display: Arc::from(address),
            target,
            root,
            proof,
            state: state::WindowState::new(thread::current().id()),
        }))
    }

    pub(crate) fn close(&self) {
        self.state.close();
    }

    pub(crate) fn prepare(
        self: &Arc<Self>,
    ) -> Result<(ExternalXdndLease, ExternalXdndPending), ExternalXdndError> {
        let (lease, pending) = self.state.reserve().map_err(|error| match error {
            state::ReserveError::Closed => ExternalXdndError::Revoked,
            state::ReserveError::Busy => ExternalXdndError::Busy,
            state::ReserveError::Exhausted => ExternalXdndError::Exhausted,
        })?;
        let mut registration = self.proof;
        registration[24..].copy_from_slice(&pending.sequence().to_ne_bytes());
        let challenge = ExternalXdndChallenge {
            display: Arc::clone(&self.display),
            target: self.target,
            root: self.root,
            creation: self.proof,
            registration,
        };
        Ok((ExternalXdndLease(lease), ExternalXdndPending { challenge, pending }))
    }
}

/// Send once to the backend. Dropping before activation releases its reservation.
pub struct ExternalXdndPending {
    challenge: ExternalXdndChallenge,
    pending: state::Pending,
}

impl ExternalXdndPending {
    /// Memory-only metadata for constructing a self-referencing external proxy.
    pub fn challenge(&self) -> &ExternalXdndChallenge {
        &self.challenge
    }

    /// Activate off the native host thread. Connection/setup I/O may block before
    /// a transaction starts, but never holds a server grab during that setup.
    pub fn connect(self) -> Result<ExternalXdndRegistration, ExternalXdndError> {
        let Self { challenge, pending } = self;
        let mut control = pending.activate().map_err(|(_, error)| match error {
            state::ActivateError::HostThread => ExternalXdndError::HostThread,
            state::ActivateError::Revoked => ExternalXdndError::Revoked,
        })?;
        let connection = match connect(&challenge.display) {
            Ok(connection) => connection,
            Err(error) => {
                control.finish(state::Phase::Detached);
                return Err(error);
            },
        };
        let atoms = match Atoms::new(&connection) {
            Ok(atoms) => atoms,
            Err(error) => {
                connection.stream().close();
                control.finish(state::Phase::Detached);
                return Err(error);
            },
        };
        Ok(ExternalXdndRegistration {
            connection: Some(connection),
            challenge,
            atoms,
            installed: None,
            #[cfg(test)]
            fail_after_first_property: false,
            control,
        })
    }
}

#[derive(Clone, Copy)]
struct Atoms {
    creation: u32,
    registration: u32,
    proxy: u32,
    aware: u32,
}

impl Atoms {
    fn new(conn: &NativeConnection) -> Result<Self, ExternalXdndError> {
        let intern = |name: &[u8]| -> Result<u32, ExternalXdndError> {
            Ok(native(native(conn.intern_atom(false, name))?.reply())?.atom)
        };
        // Force lazy protocol state before any grab, on this private connection.
        let _ = conn.maximum_request_bytes();
        native(native(conn.get_input_focus())?.reply())?;
        Ok(Self {
            creation: intern(CREATION_PROPERTY.as_bytes())?,
            registration: intern(REGISTRATION_PROPERTY.as_bytes())?,
            proxy: intern(b"XdndProxy")?,
            aware: intern(b"XdndAware")?,
        })
    }
}

type NativeConnection = RustConnection<DeadlineStream>;

fn connect(display: &str) -> Result<NativeConnection, ExternalXdndError> {
    let parsed = native(parse_display(Some(display)))?;
    for address in parsed.connect_instruction() {
        if let Ok((stream, (family, address))) = DefaultStream::connect(&address) {
            let (name, data) = get_auth(family, &address, parsed.display)
                .unwrap_or(None)
                .unwrap_or_else(|| (Vec::new(), Vec::new()));
            return native(RustConnection::connect_to_stream_with_auth_info(
                DeadlineStream::new(stream),
                parsed.screen.into(),
                name,
                data,
            ));
        }
    }
    Err(ExternalXdndError::Native)
}

/// Private, thread-affine active capability. Does not own or consume app events.
///
/// Drop attempts bounded matching cleanup on the backend thread. On panic it
/// only shuts down the unique transport; status remains native-uncertain.
pub struct ExternalXdndRegistration {
    connection: Option<NativeConnection>,
    challenge: ExternalXdndChallenge,
    atoms: Atoms,
    installed: Option<u32>,
    #[cfg(test)]
    fail_after_first_property: bool,
    // Last field: the transport is closed before control abandonment releases.
    control: state::Active,
}

impl ExternalXdndRegistration {
    /// Verify a proxy and advertise it on the actual host. Proxy must have
    /// XdndProxy(WINDOW,32) pointing to itself; XdndAware(ATOM,32) equal to 5;
    /// and both challenge properties with their own atom types and 32 bytes.
    /// No transfer is accepted or completed by registration.
    pub fn install_proxy(&mut self, proxy: u32) -> Result<(), ExternalXdndError> {
        if self.installed.is_some() || self.connection.is_none() {
            return Err(ExternalXdndError::WrongPhase);
        }
        if self.control.revoked() {
            return Err(ExternalXdndError::Revoked);
        }
        if proxy == 0 || proxy == self.challenge.target {
            return Err(ExternalXdndError::MalformedProof);
        }
        let outcome = self.install(proxy);
        if outcome.is_err() && self.installed.is_some() && self.connection.is_some() {
            self.control.uncertain();
        }
        outcome
    }

    fn install(&mut self, proxy: u32) -> Result<(), ExternalXdndError> {
        let conn = self.connection.as_ref().unwrap();
        let guard = Grab::new(conn)?;
        check_target(conn, &self.atoms, &self.challenge)?;
        check_proxy(conn, &self.atoms, &self.challenge, proxy)?;
        for property in [self.atoms.registration, self.atoms.proxy, self.atoms.aware] {
            if property_value(conn, self.challenge.target, property)?.is_some() {
                return Err(ExternalXdndError::Competing);
            }
        }
        if self.control.revoked() {
            return Err(ExternalXdndError::Revoked);
        }
        // From the first request onward failures may have left partial properties.
        self.installed = Some(proxy);
        native(
            native(conn.change_property8(
                PropMode::REPLACE,
                self.challenge.target,
                self.atoms.registration,
                self.atoms.registration,
                &self.challenge.registration,
            ))?
            .check(),
        )?;
        #[cfg(test)]
        if self.fail_after_first_property {
            conn.stream().close();
            return Err(ExternalXdndError::Native);
        }
        native(
            native(conn.change_property32(
                PropMode::REPLACE,
                self.challenge.target,
                self.atoms.proxy,
                AtomEnum::WINDOW,
                &[proxy],
            ))?
            .check(),
        )?;
        native(
            native(conn.change_property32(
                PropMode::REPLACE,
                self.challenge.target,
                self.atoms.aware,
                AtomEnum::ATOM,
                &[5],
            ))?
            .check(),
        )?;
        guard.finish()?;
        if !self.control.installed() {
            let _ = self.detach();
            return Err(ExternalXdndError::Revoked);
        }
        Ok(())
    }

    /// Remove only the complete matching tuple. Replaced/malformed properties
    /// remain untouched. Always closes this connection and releases reservation;
    /// failed native cleanup remains Uncertain rather than claiming detachment.
    pub fn detach(&mut self) -> Result<ExternalXdndPhase, ExternalXdndError> {
        let Some(conn) = self.connection.as_ref() else {
            return Err(ExternalXdndError::WrongPhase);
        };
        let outcome = match self.installed {
            None => Ok(state::Phase::Detached),
            Some(proxy) => self.remove_matching(conn, proxy),
        };
        conn.stream().close();
        self.connection.take();
        let phase = match outcome {
            Ok(phase) => phase,
            Err(ExternalXdndError::WindowGone) => state::Phase::Gone,
            Err(
                ExternalXdndError::Competing
                | ExternalXdndError::MalformedProof
                | ExternalXdndError::WrongRoot,
            ) => state::Phase::Superseded,
            Err(_) => state::Phase::Uncertain,
        };
        self.control.finish(phase);
        match outcome {
            Ok(_) | Err(ExternalXdndError::WindowGone) => Ok(phase.into()),
            Err(error) => Err(error),
        }
    }

    fn remove_matching(
        &self,
        conn: &NativeConnection,
        proxy: u32,
    ) -> Result<state::Phase, ExternalXdndError> {
        let guard = Grab::new(conn)?;
        check_target(conn, &self.atoms, &self.challenge)?;
        check_proxy(conn, &self.atoms, &self.challenge, proxy)?;
        exact_bytes(
            conn,
            self.challenge.target,
            self.atoms.registration,
            &self.challenge.registration,
        )?;
        exact_word(conn, self.challenge.target, self.atoms.proxy, AtomEnum::WINDOW.into(), proxy)?;
        exact_word(conn, self.challenge.target, self.atoms.aware, AtomEnum::ATOM.into(), 5)?;
        // No deletion occurs until the complete current tuple has matched.
        for property in [self.atoms.aware, self.atoms.proxy, self.atoms.registration] {
            native(native(conn.delete_property(self.challenge.target, property))?.check())?;
        }
        guard.finish()?;
        Ok(state::Phase::Detached)
    }
}

impl Drop for ExternalXdndRegistration {
    fn drop(&mut self) {
        if thread::panicking() {
            if let Some(conn) = &self.connection {
                // Bypass x11rb's potentially poisoned locks while unwinding.
                conn.stream().close();
            }
            self.connection.take();
        } else if self.connection.is_some() {
            let _ = self.detach();
        }
    }
}

fn native<T, E>(value: Result<T, E>) -> Result<T, ExternalXdndError> {
    value.map_err(|_| ExternalXdndError::Native)
}

fn check_target(
    conn: &NativeConnection,
    atoms: &Atoms,
    challenge: &ExternalXdndChallenge,
) -> Result<(), ExternalXdndError> {
    check_root(conn, challenge.target, challenge.root)?;
    exact_bytes(conn, challenge.target, atoms.creation, &challenge.creation)
}

fn check_root(conn: &NativeConnection, window: u32, root: u32) -> Result<(), ExternalXdndError> {
    native(conn.stream().check())?;
    let geometry = native(conn.get_geometry(window))?.reply().map_err(|error| match error {
        ReplyError::X11Error(error)
            if matches!(
                error.error_kind,
                x11rb::protocol::ErrorKind::Window | x11rb::protocol::ErrorKind::Drawable
            ) =>
        {
            ExternalXdndError::WindowGone
        },
        _ => ExternalXdndError::Native,
    })?;
    if geometry.root != root {
        return Err(ExternalXdndError::WrongRoot);
    }
    Ok(())
}

fn check_proxy(
    conn: &NativeConnection,
    atoms: &Atoms,
    challenge: &ExternalXdndChallenge,
    proxy: u32,
) -> Result<(), ExternalXdndError> {
    check_root(conn, proxy, challenge.root).map_err(|error| match error {
        ExternalXdndError::WindowGone => ExternalXdndError::MalformedProof,
        other => other,
    })?;
    exact_bytes(conn, proxy, atoms.creation, &challenge.creation)?;
    exact_bytes(conn, proxy, atoms.registration, &challenge.registration)?;
    exact_word(conn, proxy, atoms.proxy, AtomEnum::WINDOW.into(), proxy)?;
    exact_word(conn, proxy, atoms.aware, AtomEnum::ATOM.into(), 5)
}

fn property_value(
    conn: &NativeConnection,
    window: u32,
    property: u32,
) -> Result<Option<xproto::GetPropertyReply>, ExternalXdndError> {
    // Also check when the protocol library can satisfy a request from a cache.
    native(conn.stream().check())?;
    let value =
        native(native(conn.get_property(false, window, property, AtomEnum::ANY, 0, 8))?.reply())?;
    if value.bytes_after != 0 {
        return Err(ExternalXdndError::MalformedProof);
    }
    if value.type_ == 0 {
        if value.format != 0 || !value.value.is_empty() {
            return Err(ExternalXdndError::MalformedProof);
        }
        return Ok(None);
    }
    Ok(Some(value))
}

fn exact_bytes(
    conn: &NativeConnection,
    window: u32,
    property: u32,
    bytes: &[u8; 32],
) -> Result<(), ExternalXdndError> {
    let value = property_value(conn, window, property)?.ok_or(ExternalXdndError::MalformedProof)?;
    if value.type_ != property || value.format != 8 || value.value.as_slice() != bytes {
        return Err(ExternalXdndError::MalformedProof);
    }
    Ok(())
}

fn exact_word(
    conn: &NativeConnection,
    window: u32,
    property: u32,
    type_: u32,
    word: u32,
) -> Result<(), ExternalXdndError> {
    let value = property_value(conn, window, property)?.ok_or(ExternalXdndError::MalformedProof)?;
    if value.type_ != type_ || value.format != 32 || value.value.as_slice() != word.to_ne_bytes() {
        return Err(ExternalXdndError::MalformedProof);
    }
    Ok(())
}

struct Grab<'a> {
    conn: &'a NativeConnection,
    finished: bool,
}

impl<'a> Grab<'a> {
    fn new(conn: &'a NativeConnection) -> Result<Self, ExternalXdndError> {
        native(conn.stream().begin(Instant::now() + TRANSACTION_TIME))?;
        // Armed BEFORE the fallible send: the server may receive bytes even if
        // the local send reports failure. No other owner retains the transport.
        let guard = Self { conn, finished: false };
        native(native(conn.grab_server())?.check())?;
        Ok(guard)
    }

    fn finish(mut self) -> Result<(), ExternalXdndError> {
        native(self.conn.stream().check())?;
        native(native(self.conn.ungrab_server())?.check())?;
        native(self.conn.flush())?;
        self.finished = true;
        Ok(())
    }
}

impl Drop for Grab<'_> {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        if !thread::panicking() && self.conn.stream().check().is_ok() {
            let ungrabbed =
                self.conn.ungrab_server().ok().and_then(|cookie| cookie.check().ok()).is_some();
            if ungrabbed && self.conn.flush().is_ok() {
                return;
            }
        }
        self.conn.stream().close();
    }
}

#[cfg(test)]
mod tests {
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::sync::mpsc;

    use x11rb::rust_connection::Stream;

    use super::*;
    use crate::event_loop::EventLoop;
    use crate::platform::x11::{EventLoopBuilderExtX11, WindowAttributesExtX11, WindowExtX11};
    use crate::window::Window;

    fn proxy(peer: &RustConnection, challenge: &ExternalXdndChallenge) -> u32 {
        let id = peer.generate_id().unwrap();
        peer.create_window(
            0,
            id,
            challenge.root,
            0,
            0,
            1,
            1,
            0,
            xproto::WindowClass::INPUT_ONLY,
            0,
            &xproto::CreateWindowAux::new(),
        )
        .unwrap()
        .check()
        .unwrap();
        let atom = |name: &[u8]| peer.intern_atom(false, name).unwrap().reply().unwrap().atom;
        let creation = atom(challenge.creation_property().as_bytes());
        let registration = atom(challenge.registration_property().as_bytes());
        peer.change_property8(
            PropMode::REPLACE,
            id,
            creation,
            creation,
            challenge.creation_proof(),
        )
        .unwrap()
        .check()
        .unwrap();
        peer.change_property8(
            PropMode::REPLACE,
            id,
            registration,
            registration,
            challenge.registration_proof(),
        )
        .unwrap()
        .check()
        .unwrap();
        peer.change_property32(PropMode::REPLACE, id, atom(b"XdndProxy"), AtomEnum::WINDOW, &[id])
            .unwrap()
            .check()
            .unwrap();
        peer.change_property32(PropMode::REPLACE, id, atom(b"XdndAware"), AtomEnum::ATOM, &[5])
            .unwrap()
            .check()
            .unwrap();
        id
    }

    fn backend(
        pending: ExternalXdndPending,
        test: impl FnOnce(&mut ExternalXdndRegistration, &RustConnection, u32) + Send + 'static,
    ) {
        thread::spawn(move || {
            let (peer, _) = x11rb::connect(Some(pending.challenge().display())).unwrap();
            let id = proxy(&peer, pending.challenge());
            let mut registration = pending.connect().unwrap();
            test(&mut registration, &peer, id);
        })
        .join()
        .unwrap();
    }

    /// Run in its own process under an owned Xvfb. It creates real winit targets;
    /// it never reads the original connection's event queue or scans user files.
    #[test]
    #[ignore = "requires an owned X11 server (xvfb-run)"]
    #[allow(deprecated)]
    fn native_registration_identity_cleanup_timeout_and_panic() {
        let event_loop = EventLoop::builder().with_x11().with_any_thread(true).build().unwrap();
        let new_window = || {
            event_loop
                .create_window(
                    Window::default_attributes().with_visible(false).with_x11_drag_and_drop(false),
                )
                .unwrap()
        };
        let builtin =
            event_loop.create_window(Window::default_attributes().with_visible(false)).unwrap();
        assert!(matches!(builtin.prepare_external_xdnd(), Err(ExternalXdndError::BuiltinEnabled)));

        let window = new_window();
        let (lease, pending) = window.prepare_external_xdnd().unwrap();
        assert!(matches!(window.prepare_external_xdnd(), Err(ExternalXdndError::Busy)));
        drop(pending);
        assert!(lease.status().released);
        let (host_thread_lease, pending) = window.prepare_external_xdnd().unwrap();
        assert!(matches!(pending.connect(), Err(ExternalXdndError::HostThread)));
        assert!(host_thread_lease.status().released);

        // A retained/revoked old lease cannot affect a fresh native reservation.
        let (fresh, pending) = window.prepare_external_xdnd().unwrap();
        lease.revoke();
        backend(pending, |registration, _, proxy| {
            registration.install_proxy(proxy).unwrap();
            assert_eq!(registration.detach().unwrap(), ExternalXdndPhase::Detached);
        });
        assert_eq!(fresh.status().phase, ExternalXdndPhase::Detached);

        // Fault injection after the first acknowledged target property exercises
        // the real publication path. Partial state remains explicit and never
        // becomes a successful install or an unconditional detached claim.
        let window = new_window();
        let (lease, pending) = window.prepare_external_xdnd().unwrap();
        backend(pending, |registration, peer, proxy| {
            registration.fail_after_first_property = true;
            assert_eq!(registration.install_proxy(proxy), Err(ExternalXdndError::Native));
            let target = registration.challenge.target;
            let value = peer
                .get_property(false, target, registration.atoms.registration, AtomEnum::ANY, 0, 8)
                .unwrap()
                .reply()
                .unwrap();
            assert_eq!(value.value, registration.challenge.registration);
            let value = peer
                .get_property(false, target, registration.atoms.aware, AtomEnum::ANY, 0, 8)
                .unwrap()
                .reply()
                .unwrap();
            assert_eq!(value.type_, 0);
            assert_eq!(registration.detach(), Err(ExternalXdndError::Native));
        });
        assert_eq!(lease.status().phase, ExternalXdndPhase::Uncertain);
        assert!(lease.status().released);
        // Native residue may reject the next install, but the memory slot is no
        // longer Busy and the old lease has no control over its replacement.
        let (replacement, pending) = window.prepare_external_xdnd().unwrap();
        lease.revoke();
        assert!(!replacement.status().revoked);
        drop(pending);

        // Exact proof type, format, size and trailing-byte checks.
        for malformed in 0..4 {
            let window = new_window();
            let (lease, pending) = window.prepare_external_xdnd().unwrap();
            backend(pending, move |registration, peer, proxy| {
                let atom = registration.atoms.creation;
                match malformed {
                    0 => peer
                        .change_property8(
                            PropMode::REPLACE,
                            proxy,
                            atom,
                            AtomEnum::STRING,
                            &[0; 32],
                        )
                        .unwrap()
                        .check()
                        .unwrap(),
                    1 => peer
                        .change_property32(PropMode::REPLACE, proxy, atom, atom, &[0; 8])
                        .unwrap()
                        .check()
                        .unwrap(),
                    2 => peer
                        .change_property8(PropMode::REPLACE, proxy, atom, atom, &[0; 31])
                        .unwrap()
                        .check()
                        .unwrap(),
                    _ => peer
                        .change_property8(PropMode::REPLACE, proxy, atom, atom, &[0; 33])
                        .unwrap()
                        .check()
                        .unwrap(),
                }
                assert_eq!(
                    registration.install_proxy(proxy),
                    Err(ExternalXdndError::MalformedProof)
                );
                assert!(property_value(
                    registration.connection.as_ref().unwrap(),
                    registration.challenge.target,
                    registration.atoms.aware
                )
                .unwrap()
                .is_none());
            });
            assert_eq!(lease.status().phase, ExternalXdndPhase::Detached);
        }

        // MappingNotify is broadcast even to a client that selected no events.
        // Repeated benign admission failures must not grow x11rb's private
        // pending-event queue across fresh transactions without a lifetime cap.
        let window = new_window();
        let (lease, pending) = window.prepare_external_xdnd().unwrap();
        backend(pending, |registration, peer, proxy| {
            let keycode = peer.setup().min_keycode;
            let original = peer.get_keyboard_mapping(keycode, 1).unwrap().reply().unwrap();
            let mut changed = original.keysyms.clone();
            changed[0] ^= 1;
            let mut wrong_proof = registration.challenge.creation;
            wrong_proof[0] ^= 1;
            peer.change_property8(
                PropMode::REPLACE,
                proxy,
                registration.atoms.creation,
                registration.atoms.creation,
                &wrong_proof,
            )
            .unwrap()
            .check()
            .unwrap();
            let mut stopped = false;
            for attempt in 0..256 {
                let keysyms = if attempt % 2 == 0 { &changed } else { &original.keysyms };
                peer.change_keyboard_mapping(1, keycode, original.keysyms_per_keycode, keysyms)
                    .unwrap()
                    .check()
                    .unwrap();
                match registration.install_proxy(proxy) {
                    Err(ExternalXdndError::MalformedProof) => {},
                    Err(ExternalXdndError::Native) => {
                        stopped = true;
                        break;
                    },
                    result => panic!("unexpected bounded-retry outcome: {result:?}"),
                }
            }
            peer.change_keyboard_mapping(
                1,
                keycode,
                original.keysyms_per_keycode,
                &original.keysyms,
            )
            .unwrap()
            .check()
            .unwrap();
            assert!(stopped, "private event retention survived its lifetime packet cap");
            assert!(registration.connection.as_ref().unwrap().stream().check().is_err());
            assert_eq!(
                peer.get_property(
                    false,
                    registration.challenge.target,
                    registration.atoms.aware,
                    AtomEnum::ANY,
                    0,
                    8
                )
                .unwrap()
                .reply()
                .unwrap()
                .type_,
                0
            );
        });
        assert!(lease.status().released);
        assert_eq!(lease.status().phase, ExternalXdndPhase::Detached);

        // Admission never overwrites an existing target registration.
        let window = new_window();
        let (lease, pending) = window.prepare_external_xdnd().unwrap();
        backend(pending, |registration, peer, proxy| {
            let target = registration.challenge.target;
            let atom = registration.atoms.proxy;
            peer.change_property32(PropMode::REPLACE, target, atom, AtomEnum::WINDOW, &[0x1234])
                .unwrap()
                .check()
                .unwrap();
            assert_eq!(registration.install_proxy(proxy), Err(ExternalXdndError::Competing));
            exact_word(
                registration.connection.as_ref().unwrap(),
                target,
                atom,
                AtomEnum::WINDOW.into(),
                0x1234,
            )
            .unwrap();
        });
        assert_eq!(lease.status().phase, ExternalXdndPhase::Detached);

        // Stale cleanup leaves the entire changed tuple untouched; the immutable
        // creation proof survives both successful and refused cleanup.
        let window = new_window();
        let (lease, pending) = window.prepare_external_xdnd().unwrap();
        backend(pending, |registration, peer, proxy| {
            registration.install_proxy(proxy).unwrap();
            let target = registration.challenge.target;
            let atom = registration.atoms.registration;
            peer.change_property8(PropMode::REPLACE, target, atom, atom, &[7; 32])
                .unwrap()
                .check()
                .unwrap();
            assert_eq!(registration.detach(), Err(ExternalXdndError::MalformedProof));
            let value = peer
                .get_property(false, target, atom, AtomEnum::ANY, 0, 8)
                .unwrap()
                .reply()
                .unwrap();
            assert_eq!(value.value, [7; 32]);
            let value = peer
                .get_property(false, target, registration.atoms.creation, AtomEnum::ANY, 0, 8)
                .unwrap()
                .reply()
                .unwrap();
            assert_eq!(value.value, registration.challenge.creation);
        });
        assert_eq!(lease.status().phase, ExternalXdndPhase::Superseded);

        // Native target destruction is distinct from proxy destruction.
        let window = new_window();
        let (lease, pending) = window.prepare_external_xdnd().unwrap();
        backend(pending, |registration, peer, proxy| {
            registration.install_proxy(proxy).unwrap();
            peer.destroy_window(registration.challenge.target).unwrap().check().unwrap();
            assert_eq!(registration.detach().unwrap(), ExternalXdndPhase::Gone);
        });
        assert_eq!(lease.status().phase, ExternalXdndPhase::Gone);

        // Host revoke before backend installation is immediate and no native
        // advertisement is published. Channels, not delays, order the race.
        let window = new_window();
        let (lease, pending) = window.prepare_external_xdnd().unwrap();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (go_tx, go_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let (peer, _) = x11rb::connect(Some(pending.challenge().display())).unwrap();
            let proxy = proxy(&peer, pending.challenge());
            let mut registration = pending.connect().unwrap();
            ready_tx.send(()).unwrap();
            go_rx.recv().unwrap();
            assert_eq!(registration.install_proxy(proxy), Err(ExternalXdndError::Revoked));
        });
        ready_rx.recv().unwrap();
        drop(window);
        go_tx.send(()).unwrap();
        worker.join().unwrap();
        assert!(lease.status().revoked);

        // Confirmed server grabs are forcibly released on a real blocked-poll
        // timeout and on panic. An unrelated bounded client must then progress.
        for panic_after_grab in [false, true] {
            let window = new_window();
            let (lease, pending) = window.prepare_external_xdnd().unwrap();
            backend(pending, move |registration, peer, proxy| {
                registration.install_proxy(proxy).unwrap();
                let target = registration.challenge.target;
                let atom = registration.atoms.registration;
                peer.change_property8(PropMode::REPLACE, target, atom, atom, &[9; 32])
                    .unwrap()
                    .check()
                    .unwrap();
                let unrelated = connect(&registration.challenge.display).unwrap();
                let conn = registration.connection.as_ref().unwrap();
                if panic_after_grab {
                    assert!(catch_unwind(AssertUnwindSafe(|| {
                        let _grab = Grab::new(conn).unwrap();
                        panic!("owned native XDND fixture: after confirmed grab");
                    }))
                    .is_err());
                } else {
                    let grab = Grab::new(conn).unwrap();
                    conn.stream().begin(Instant::now() + Duration::from_millis(10)).unwrap();
                    assert_eq!(
                        conn.stream()
                            .poll(x11rb::rust_connection::PollMode::Readable)
                            .unwrap_err()
                            .kind(),
                        std::io::ErrorKind::TimedOut
                    );
                    drop(grab);
                }
                unrelated.stream().begin(Instant::now() + Duration::from_secs(1)).unwrap();
                unrelated.get_input_focus().unwrap().reply().unwrap();
                assert_eq!(
                    peer.get_property(false, target, atom, AtomEnum::ANY, 0, 8)
                        .unwrap()
                        .reply()
                        .unwrap()
                        .value,
                    [9; 32]
                );
                assert_eq!(registration.detach(), Err(ExternalXdndError::Native));
            });
            assert_eq!(lease.status().phase, ExternalXdndPhase::Uncertain);
        }
    }
}
