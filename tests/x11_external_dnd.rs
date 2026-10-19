//! Native qualification, not a drag/drop implementation. Run in an owned server:
//! `xvfb-run -a cargo test --locked --no-default-features --features x11,rwh_06
//! --test x11_external_dnd -- --ignored --nocapture`
#![cfg(all(x11_platform, feature = "rwh_06"))]

use rwh_06::{HasWindowHandle, RawWindowHandle};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{fs, thread};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::platform::run_on_demand::EventLoopExtRunOnDemand;
use winit::platform::x11::{EventLoopBuilderExtX11, WindowAttributesExtX11};
use winit::window::{Window, WindowId};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    self, AtomEnum, ConnectionExt as _, EventMask, PropMode, WindowClass,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{COPY_DEPTH_FROM_PARENT, CURRENT_TIME};

#[derive(Debug)]
enum User {
    Recreate(Sender<u32>),
    Done(Result<(), String>),
}
struct App {
    enabled: Option<Window>,
    external: Option<Window>,
    ids: Option<Sender<(u32, u32)>>,
    marker: Sender<u32>,
    dropped: Vec<PathBuf>,
    unexpected_external: Vec<String>,
    deadline: Instant,
    result: Option<Result<(), String>>,
}
fn xid(window: &Window) -> u32 {
    match window.window_handle().unwrap().as_raw() {
        RawWindowHandle::Xlib(h) => h.window.try_into().unwrap(),
        RawWindowHandle::Xcb(h) => h.window.get(),
        other => panic!("expected X11, got {other:?}"),
    }
}
fn external(event_loop: &ActiveEventLoop) -> Window {
    event_loop
        .create_window(
            Window::default_attributes()
                .with_title("owned external XDND test")
                .with_x11_drag_and_drop(false),
        )
        .unwrap()
}
impl ApplicationHandler<User> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.enabled.is_some() {
            return;
        }
        let enabled = event_loop
            .create_window(Window::default_attributes().with_title("owned default XDND test"))
            .unwrap();
        let external = external(event_loop);
        self.ids.take().unwrap().send((xid(&enabled), xid(&external))).unwrap();
        self.enabled = Some(enabled);
        self.external = Some(external);
    }
    fn window_event(&mut self, _: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        if matches!(event, WindowEvent::CloseRequested) {
            let window = [self.enabled.as_ref(), self.external.as_ref()]
                .into_iter()
                .flatten()
                .find(|w| w.id() == id)
                .unwrap();
            self.marker.send(xid(window)).unwrap();
        }
        match event {
            WindowEvent::HoveredFile(_)
            | WindowEvent::DroppedFile(_)
            | WindowEvent::HoveredFileCancelled
                if self.external.as_ref().is_some_and(|w| w.id() == id) =>
            {
                self.unexpected_external.push(format!("{event:?}"));
            },
            WindowEvent::DroppedFile(path) => self.dropped.push(path),
            _ => {},
        }
    }
    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: User) {
        match event {
            User::Recreate(reply) => {
                self.external.take();
                let window = external(event_loop);
                reply.send(xid(&window)).unwrap();
                self.external = Some(window);
            },
            User::Done(result) => {
                self.result = Some(result);
                event_loop.exit();
            },
        }
    }
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if Instant::now() >= self.deadline {
            self.result = Some(Err("native peer deadline".into()));
            event_loop.exit();
        } else {
            event_loop.set_control_flow(ControlFlow::WaitUntil(self.deadline));
        }
    }
}
struct Peer {
    conn: RustConnection,
    source: u32,
    uri: u32,
    selection: u32,
    enter: u32,
    position: u32,
    drop: u32,
    leave: u32,
    status: u32,
    finished: u32,
    aware: u32,
    proxy: u32,
    marker: Receiver<u32>,
    payload: Vec<u8>,
    unexpected: Vec<String>,
}
impl Peer {
    fn new(marker: Receiver<u32>, path: &std::path::Path) -> Self {
        let (conn, screen) = x11rb::connect(None).unwrap();
        let source = conn.generate_id().unwrap();
        conn.create_window(
            COPY_DEPTH_FROM_PARENT,
            source,
            conn.setup().roots[screen].root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            &xproto::CreateWindowAux::new(),
        )
        .unwrap()
        .check()
        .unwrap();
        let atom = |name: &[u8]| conn.intern_atom(false, name).unwrap().reply().unwrap().atom;
        let values: Vec<_> = [
            b"text/uri-list".as_slice(),
            b"XdndSelection",
            b"XdndEnter",
            b"XdndPosition",
            b"XdndDrop",
            b"XdndLeave",
            b"XdndStatus",
            b"XdndFinished",
            b"XdndAware",
            b"XdndProxy",
        ]
        .iter()
        .map(|name| atom(name))
        .collect();
        Self {
            conn,
            source,
            uri: values[0],
            selection: values[1],
            enter: values[2],
            position: values[3],
            drop: values[4],
            leave: values[5],
            status: values[6],
            finished: values[7],
            aware: values[8],
            proxy: values[9],
            marker,
            payload: format!("file://{}\r\n", path.display()).into_bytes(),
            unexpected: Vec::new(),
        }
    }
    fn message(&self, destination: u32, window: u32, type_: u32, data: [u32; 5]) {
        let event = xproto::ClientMessageEvent::new(32, window, type_, data);
        self.conn
            .send_event(false, destination, EventMask::NO_EVENT, event)
            .unwrap()
            .check()
            .unwrap();
        self.conn.flush().unwrap();
    }
    fn barrier(&mut self, window: u32) {
        let atom = |name: &[u8]| self.conn.intern_atom(false, name).unwrap().reply().unwrap().atom;
        self.message(window, window, atom(b"WM_PROTOCOLS"), [
            atom(b"WM_DELETE_WINDOW"),
            0,
            0,
            0,
            0,
        ]);
        assert_eq!(self.marker.recv_timeout(Duration::from_secs(5)).unwrap(), window);
    }
    fn event(&mut self, timeout: Duration) -> Event {
        let end = Instant::now() + timeout;
        loop {
            if let Some(event) = self.conn.poll_for_event().unwrap() {
                return event;
            }
            assert!(Instant::now() < end, "peer event deadline");
            thread::yield_now();
        }
    }
    fn serve(&self, request: xproto::SelectionRequestEvent) {
        assert_eq!(request.selection, self.selection);
        assert_eq!(request.target, self.uri);
        self.conn
            .change_property8(
                PropMode::REPLACE,
                request.requestor,
                request.property,
                self.uri,
                &self.payload,
            )
            .unwrap()
            .check()
            .unwrap();
        self.notify(request.requestor, request.time);
    }
    fn notify(&self, requestor: u32, time: u32) {
        let event = xproto::SelectionNotifyEvent {
            response_type: xproto::SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time,
            requestor,
            selection: self.selection,
            target: self.uri,
            property: self.selection,
        };
        self.conn
            .send_event(false, requestor, EventMask::NO_EVENT, event)
            .unwrap()
            .check()
            .unwrap();
        self.conn.flush().unwrap();
    }
    fn begin_default(&mut self, window: u32) {
        self.conn
            .set_selection_owner(self.source, self.selection, CURRENT_TIME)
            .unwrap()
            .check()
            .unwrap();
        self.message(window, window, self.enter, [self.source, 5 << 24, self.uri, 0, 0]);
        self.message(window, window, self.position, [self.source, 0, 0, CURRENT_TIME, 0]);
        let (mut requested, mut accepted) = (false, false);
        while !requested || !accepted {
            match self.event(Duration::from_secs(5)) {
                Event::SelectionRequest(event) => {
                    self.serve(event);
                    requested = true;
                },
                Event::ClientMessage(event) if event.type_ == self.status => {
                    assert_eq!(event.data.as_data32()[0], window);
                    assert_eq!(event.data.as_data32()[1] & 1, 1);
                    accepted = true;
                },
                event => self.record_unexpected(event),
            }
        }
        // The real app processes SelectionNotify before this marker from the
        // same connection; this is synchronization, not a timing sleep.
        self.barrier(window);
    }
    fn attack_external(&mut self, delivery: u32, target: u32) {
        self.message(delivery, target, self.enter, [self.source, 5 << 24, 0, 0, 0]);
        for type_ in [self.position, self.drop, self.leave, self.drop] {
            self.message(delivery, target, type_, [self.source, 0, 0, 0, 0]);
        }
        // A valid-looking but differently typed, repeated selection notification
        // must not clear another window's retained built-in payload.
        self.conn
            .change_property8(
                PropMode::REPLACE,
                delivery,
                self.selection,
                AtomEnum::STRING,
                b"invalid",
            )
            .unwrap()
            .check()
            .unwrap();
        self.notify(delivery, CURRENT_TIME);
        self.notify(delivery, CURRENT_TIME);
        self.barrier(delivery);
    }
    fn finish_default(&mut self, window: u32) {
        self.message(window, window, self.drop, [self.source, 0, CURRENT_TIME, 0, 0]);
        loop {
            match self.event(Duration::from_secs(5)) {
                Event::ClientMessage(event) if event.type_ == self.finished => {
                    let data = event.data.as_data32();
                    assert_eq!(data[0], window, "external/stale target emitted Finished");
                    assert_eq!(data[1] & 1, 1, "external traffic corrupted enabled state");
                    break;
                },
                event => self.record_unexpected(event),
            }
        }
        self.barrier(window);
        while let Some(event) = self.conn.poll_for_event().unwrap() {
            self.record_unexpected(event);
        }
    }
    fn record_unexpected(&mut self, event: Event) {
        if matches!(&event, Event::ClientMessage(e) if e.type_ == self.status || e.type_ == self.finished)
            || matches!(&event, Event::SelectionRequest(_))
        {
            self.unexpected.push(format!("{event:?}"));
        }
    }
    fn proxy_roundtrip(&mut self, target: u32) -> u32 {
        let proxy = self.conn.generate_id().unwrap();
        let root = self.conn.setup().roots[0].root;
        self.conn
            .create_window(
                COPY_DEPTH_FROM_PARENT,
                proxy,
                root,
                0,
                0,
                1,
                1,
                0,
                WindowClass::INPUT_OUTPUT,
                0,
                &xproto::CreateWindowAux::new(),
            )
            .unwrap()
            .check()
            .unwrap();
        for (window, property, type_, data) in [
            (proxy, self.proxy, u32::from(AtomEnum::WINDOW), vec![proxy]),
            (target, self.proxy, u32::from(AtomEnum::WINDOW), vec![proxy]),
            (proxy, self.aware, u32::from(AtomEnum::ATOM), vec![5]),
            (target, self.aware, u32::from(AtomEnum::ATOM), vec![5]),
        ] {
            self.conn
                .change_property32(PropMode::REPLACE, window, property, type_, &data)
                .unwrap()
                .check()
                .unwrap();
        }
        // A compliant source discovers the real target's self-validating proxy.
        let get = |w| {
            self.conn
                .get_property(false, w, self.proxy, AtomEnum::WINDOW, 0, 1)
                .unwrap()
                .reply()
                .unwrap()
                .value32()
                .unwrap()
                .next()
                .unwrap()
        };
        let selected = get(target);
        assert_eq!(get(selected), selected);
        for type_ in [self.enter, self.position, self.drop, self.leave] {
            self.message(selected, target, type_, [self.source, 5 << 24, self.uri, 0, 0]);
            let Event::ClientMessage(event) = self.event(Duration::from_secs(5)) else {
                panic!("proxy did not receive client message")
            };
            assert_eq!((event.window, event.type_), (target, type_));
        }
        proxy
    }
    fn detach_proxy(&self, target: u32, proxy: u32) {
        self.conn.delete_property(target, self.proxy).unwrap().check().unwrap();
        self.conn.delete_property(target, self.aware).unwrap().check().unwrap();
        self.conn.destroy_window(proxy).unwrap().check().unwrap();
    }
}

#[test]
#[ignore = "requires an owned X11 server (xvfb-run), exercises real native windows"]
fn external_xdnd_is_exclusive_per_live_window_and_default_behavior_is_unchanged() {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".test-work").join(format!(
        "external-dnd-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
    ));
    fs::create_dir_all(&fixture).unwrap();
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(fixture.clone());
    let source = fixture.join("source");
    fs::write(&source, b"owned native DnD fixture").unwrap();
    let mut builder = EventLoop::<User>::with_user_event();
    builder.with_x11().with_any_thread(true);
    let mut event_loop = builder.build().unwrap();
    let proxy: EventLoopProxy<User> = event_loop.create_proxy();
    let (ids, receive_ids) = mpsc::channel();
    let (marker, receive_marker) = mpsc::channel();
    let peer_source = source.clone();
    let worker = thread::spawn(move || {
        let result = std::panic::catch_unwind(|| {
            let (enabled, external) = receive_ids.recv_timeout(Duration::from_secs(5)).unwrap();
            let mut peer = Peer::new(receive_marker, &peer_source);
            let aware = |w| {
                peer.conn
                    .get_property(false, w, peer.aware, AtomEnum::ANY, 0, 1)
                    .unwrap()
                    .reply()
                    .unwrap()
            };
            assert_eq!(aware(enabled).value32().unwrap().next(), Some(5));
            assert_eq!(aware(external).type_, u32::from(AtomEnum::NONE));
            let external_proxy = peer.proxy_roundtrip(external);
            peer.begin_default(enabled);
            peer.attack_external(external, external);
            peer.attack_external(external, u32::MAX);
            peer.finish_default(enabled);
            peer.detach_proxy(external, external_proxy);
            let (reply, recreated) = mpsc::channel();
            proxy.send_event(User::Recreate(reply)).unwrap();
            let new = recreated.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_ne!(new, external);
            peer.begin_default(enabled);
            // Deliver stale target fields to the new live window after the old
            // one has been destroyed. No registration/state may be inherited.
            peer.attack_external(new, external);
            peer.attack_external(new, new);
            peer.finish_default(enabled);
            assert!(
                peer.unexpected.is_empty(),
                "unexpected native activity: {:?}",
                peer.unexpected
            );
        })
        .map_err(|_| "native peer assertion failed".to_owned());
        proxy.send_event(User::Done(result)).unwrap();
    });
    let mut app = App {
        enabled: None,
        external: None,
        ids: Some(ids),
        marker,
        dropped: Vec::new(),
        unexpected_external: Vec::new(),
        deadline: Instant::now() + Duration::from_secs(25),
        result: None,
    };
    event_loop.run_app_on_demand(&mut app).unwrap();
    worker.join().unwrap();
    app.result.unwrap().unwrap();
    assert_eq!(app.dropped, vec![source.clone(), source]);
    assert!(
        app.unexpected_external.is_empty(),
        "external callbacks: {:?}",
        app.unexpected_external
    );
}
