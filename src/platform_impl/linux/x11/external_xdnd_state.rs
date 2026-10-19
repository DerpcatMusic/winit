//! Memory-only ownership for the proposed external-XDND registration capability.
//!
//! Native integration must close its unique connection before releasing an active
//! control. This module neither claims native cleanup nor performs any native I/O.

use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::thread::{self, ThreadId};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReserveError {
    Closed,
    Busy,
    Exhausted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ActivateError {
    HostThread,
    Revoked,
}

/// Native facts and logical revocation are deliberately separate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum Phase {
    Reserved,
    Active,
    Installed,
    Detached,
    Gone,
    Superseded,
    Uncertain,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Status {
    pub(crate) revoked: bool,
    pub(crate) released: bool,
    pub(crate) phase: Phase,
}

struct Control {
    sequence: u64,
    host_thread: ThreadId,
    revoked: AtomicBool,
    released: AtomicBool,
    phase: AtomicU8,
}

impl Control {
    fn revoke(&self) {
        self.revoked.store(true, Ordering::SeqCst);
    }

    fn release(&self, phase: Phase) {
        self.revoke();
        self.phase.store(phase as u8, Ordering::SeqCst);
        // Admission observing release must also observe the final native phase.
        self.released.store(true, Ordering::SeqCst);
    }

    fn status(&self) -> Status {
        let phase = match self.phase.load(Ordering::SeqCst) {
            0 => Phase::Reserved,
            1 => Phase::Active,
            2 => Phase::Installed,
            3 => Phase::Detached,
            4 => Phase::Gone,
            5 => Phase::Superseded,
            _ => Phase::Uncertain,
        };
        Status {
            revoked: self.revoked.load(Ordering::SeqCst),
            released: self.released.load(Ordering::SeqCst),
            phase,
        }
    }
}

struct Slot {
    closed: bool,
    /// None is permanent exhaustion, including after the previous worker exits.
    next: Option<u64>,
    active: Weak<Control>,
}

pub(crate) struct WindowState {
    /// Captured at native creation, never from the thread calling reserve.
    host_thread: ThreadId,
    slot: Mutex<Slot>,
}

impl WindowState {
    pub(crate) fn new(host_thread: ThreadId) -> Self {
        Self {
            host_thread,
            slot: Mutex::new(Slot { closed: false, next: Some(1), active: Weak::new() }),
        }
    }

    pub(crate) fn reserve(&self) -> Result<(Lease, Pending), ReserveError> {
        // This mutex never covers native calls or caller-supplied code.
        let mut slot = self.slot.lock().unwrap_or_else(|poison| poison.into_inner());
        if slot.closed {
            return Err(ReserveError::Closed);
        }
        if slot.active.upgrade().is_some_and(|active| !active.released.load(Ordering::SeqCst)) {
            return Err(ReserveError::Busy);
        }
        let sequence = slot.next.ok_or(ReserveError::Exhausted)?;
        slot.next = sequence.checked_add(1);
        let control = Arc::new(Control {
            sequence,
            host_thread: self.host_thread,
            revoked: AtomicBool::new(false),
            released: AtomicBool::new(false),
            phase: AtomicU8::new(Phase::Reserved as u8),
        });
        slot.active = Arc::downgrade(&control);
        Ok((Lease(Arc::clone(&control)), Pending(Some(control))))
    }

    /// Must run before host teardown; does not wait for the backend worker.
    pub(crate) fn close(&self) {
        let mut slot = self.slot.lock().unwrap_or_else(|poison| poison.into_inner());
        slot.closed = true;
        if let Some(active) = slot.active.upgrade() {
            active.revoke();
        }
    }
}

pub(crate) struct Lease(Arc<Control>);

impl Lease {
    pub(crate) fn revoke(&self) {
        self.0.revoke();
    }

    pub(crate) fn status(&self) -> Status {
        self.0.status()
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.revoke();
    }
}

/// Safe to send to the backend before any native connection is opened.
pub(crate) struct Pending(Option<Arc<Control>>);

impl Pending {
    pub(crate) fn activate(mut self) -> Result<Active, (Self, ActivateError)> {
        let control = self.0.as_ref().expect("unconsumed pending control");
        if thread::current().id() == control.host_thread {
            return Err((self, ActivateError::HostThread));
        }
        if control.revoked.load(Ordering::SeqCst) {
            return Err((self, ActivateError::Revoked));
        }
        let control = self.0.take().expect("unconsumed pending control");
        control.phase.store(Phase::Active as u8, Ordering::SeqCst);
        Ok(Active { control, finished: false, thread_bound: PhantomData })
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        if let Some(control) = self.0.take() {
            // No native transport ever belonged to this pending capability.
            control.release(Phase::Detached);
        }
    }
}

/// Cannot return to the UI thread after activation, or be used concurrently.
pub(crate) struct Active {
    control: Arc<Control>,
    finished: bool,
    thread_bound: PhantomData<Rc<()>>,
}

impl Active {
    pub(crate) fn revoked(&self) -> bool {
        self.control.revoked.load(Ordering::SeqCst)
    }

    pub(crate) fn installed(&self) -> bool {
        if self.revoked() {
            return false;
        }
        self.control.phase.store(Phase::Installed as u8, Ordering::SeqCst);
        // A racing revoke remains visible and does not become "installed" proof.
        !self.revoked()
    }

    /// Native wrapper calls only after cleanup or closing its unique transport.
    pub(crate) fn finish(&mut self, phase: Phase) {
        assert!(matches!(
            phase,
            Phase::Detached | Phase::Gone | Phase::Superseded | Phase::Uncertain
        ));
        if !self.finished {
            self.control.release(phase);
            self.finished = true;
        }
    }
}

impl Drop for Active {
    fn drop(&mut self) {
        if !self.finished {
            // Abandonment is never evidence that native properties disappeared.
            self.control.release(Phase::Uncertain);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    #[test]
    fn pending_drop_releases_reservation_even_with_old_lease_retained() {
        let state = WindowState::new(thread::current().id());
        let (old, pending) = state.reserve().unwrap();
        assert!(matches!(state.reserve(), Err(ReserveError::Busy)));
        drop(pending);
        assert_eq!(old.status(), Status { revoked: true, released: true, phase: Phase::Detached });
        let (replacement, pending) = state.reserve().unwrap();
        assert_ne!(old.0.sequence, replacement.0.sequence);
        old.revoke();
        drop(old);
        assert!(!replacement.status().revoked);
        assert!(matches!(state.reserve(), Err(ReserveError::Busy)));
        drop(pending);
    }

    #[test]
    fn host_drop_revokes_but_cannot_release_worker_or_claim_native_cleanup() {
        let state = WindowState::new(thread::current().id());
        let (lease, pending) = state.reserve().unwrap();
        let observer = Arc::clone(&lease.0);
        drop(lease);
        assert!(matches!(state.reserve(), Err(ReserveError::Busy)));
        let pending = thread::spawn(move || match pending.activate() {
            Err((pending, ActivateError::Revoked)) => pending,
            _ => panic!("revoked capability activated"),
        })
        .join()
        .unwrap();
        assert!(!observer.status().released);
        drop(pending);
        assert_eq!(observer.status().phase, Phase::Detached);
        assert!(state.reserve().is_ok());
    }

    #[test]
    fn activation_checks_creation_thread_even_when_reserved_elsewhere() {
        let host_thread = thread::current().id();
        let state = Arc::new(WindowState::new(host_thread));
        let remote = Arc::clone(&state);
        let (lease, pending) = thread::spawn(move || remote.reserve().unwrap()).join().unwrap();
        let pending = match pending.activate() {
            Err((pending, ActivateError::HostThread)) => pending,
            _ => panic!("host thread activated native worker"),
        };
        thread::spawn(move || {
            let mut active = pending.activate().unwrap_or_else(|_| panic!("backend activation"));
            assert!(active.installed());
            active.finish(Phase::Detached);
        })
        .join()
        .unwrap();
        assert_eq!(lease.status().phase, Phase::Detached);
        assert!(lease.status().released);
    }

    #[test]
    fn close_revokes_active_without_waiting_for_it() {
        let state = WindowState::new(thread::current().id());
        let (lease, pending) = state.reserve().unwrap();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (continue_tx, continue_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let mut active = pending.activate().unwrap_or_else(|_| panic!("backend activation"));
            assert!(active.installed());
            ready_tx.send(()).unwrap();
            continue_rx.recv().unwrap();
            assert!(active.revoked());
            assert!(!active.installed());
            active.finish(Phase::Gone);
        });
        ready_rx.recv().unwrap();
        state.close();
        assert!(lease.status().revoked);
        assert!(!lease.status().released);
        assert!(matches!(state.reserve(), Err(ReserveError::Closed)));
        continue_tx.send(()).unwrap();
        worker.join().unwrap();
        assert_eq!(lease.status().phase, Phase::Gone);
    }

    #[test]
    fn abandoned_active_is_uncertain_and_replacement_is_not_affected() {
        let state = WindowState::new(thread::current().id());
        let (old, pending) = state.reserve().unwrap();
        thread::spawn(move || {
            let active = pending.activate().unwrap_or_else(|_| panic!("backend activation"));
            assert!(active.installed());
            drop(active);
        })
        .join()
        .unwrap();
        assert_eq!(old.status().phase, Phase::Uncertain);
        assert!(old.status().released);
        let (replacement, pending) = state.reserve().unwrap();
        drop(old);
        assert!(!replacement.status().revoked);
        drop(pending);
    }

    #[test]
    fn exhausted_sequence_never_wraps_after_release() {
        let state = WindowState::new(thread::current().id());
        state.slot.lock().unwrap().next = Some(u64::MAX);
        let (lease, pending) = state.reserve().unwrap();
        assert_eq!(lease.0.sequence, u64::MAX);
        drop(pending);
        assert!(matches!(state.reserve(), Err(ReserveError::Exhausted)));
        assert!(matches!(state.reserve(), Err(ReserveError::Exhausted)));
    }

    #[test]
    fn close_racing_reservation_always_revokes_or_refuses() {
        for _ in 0..64 {
            let state = Arc::new(WindowState::new(thread::current().id()));
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let remote_state = Arc::clone(&state);
            let remote_barrier = Arc::clone(&barrier);
            let worker = thread::spawn(move || {
                remote_barrier.wait();
                remote_state.reserve()
            });
            barrier.wait();
            state.close();
            match worker.join().unwrap() {
                Ok((lease, pending)) => {
                    assert!(lease.status().revoked);
                    drop(pending);
                },
                Err(ReserveError::Closed) => {},
                _ => panic!("unexpected admission outcome"),
            }
            assert!(matches!(state.reserve(), Err(ReserveError::Closed)));
        }
    }
}
