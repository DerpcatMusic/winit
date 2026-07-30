//! # Wayland
//!
//! **Note:** Windows don't appear on Wayland until you draw/present to them.
//!
//! By default, Winit loads system libraries using `dlopen`. This can be
//! disabled by disabling the `"wayland-dlopen"` cargo feature.
//!
//! ## Client-side decorations
//!
//! Winit provides client-side decorations by default, but the behaviour can
//! be controlled with the following feature flags:
//!
//! * `wayland-csd-adwaita` (default).
//! * `wayland-csd-adwaita-crossfont`.
//! * `wayland-csd-adwaita-notitle`.

use std::ffi::c_void;
use std::fmt;
use std::io;
use std::num::NonZeroU32;
use std::ptr::NonNull;

use crate::event_loop::{ActiveEventLoop, EventLoop, EventLoopBuilder};
use crate::monitor::MonitorHandle;
use crate::window::{Window, WindowAttributes};

pub use crate::window::Theme;

/// One MIME payload advertised by an outbound Wayland data source.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WaylandMimeData {
    mime_type: String,
    data: Vec<u8>,
}

impl WaylandMimeData {
    /// Creates an owned MIME payload.
    ///
    /// # Errors
    ///
    /// Returns [`WaylandDragError::InvalidMimeType`] for an empty MIME type.
    pub fn new(
        mime_type: impl Into<String>,
        data: impl Into<Vec<u8>>,
    ) -> Result<Self, WaylandDragError> {
        let mime_type = mime_type.into();
        if mime_type.trim().is_empty() {
            return Err(WaylandDragError::InvalidMimeType);
        }
        Ok(Self { mime_type, data: data.into() })
    }

    /// Advertised MIME type.
    pub fn mime_type(&self) -> &str {
        &self.mime_type
    }

    /// Bytes written when the target requests this MIME type.
    pub fn data(&self) -> &[u8] {
        &self.data
    }
}

/// A compositor event for one outbound Wayland data source.
#[derive(Debug)]
#[non_exhaustive]
pub enum WaylandDataSourceEvent {
    /// The target changed its accepted MIME type.
    MimeAccepted(Option<String>),
    /// The target requested and received one MIME payload.
    DataRequested(String),
    /// Writing the requested MIME payload failed.
    DataTransferFailed { mime_type: String, error: io::Error },
    /// The compositor reported that a drop was performed.
    DropPerformed,
    /// The compositor selected an action for the current target.
    ActionSelected(WaylandDragAction),
    /// The target completed the operation.
    Finished,
    /// The compositor or target cancelled the operation.
    Cancelled,
}

/// Action selected by the compositor for an outbound source.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum WaylandDragAction {
    None,
    Copy,
    Move,
    Ask,
    Unknown,
}

/// Receives compositor events for one outbound Wayland data source.
pub trait WaylandDataSourceHandler: Send + 'static {
    /// Handles one event on Winit's Wayland event-loop thread.
    fn event(&mut self, event: WaylandDataSourceEvent);
}

/// Failure to start an outbound Wayland data source.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum WaylandDragError {
    /// `wl_data_device_manager` is unavailable.
    Unsupported,
    /// No non-empty MIME payload set was supplied.
    EmptyOfferSet,
    /// A MIME type was empty.
    InvalidMimeType,
    /// Another outbound source is active on this event loop.
    AlreadyActive,
    /// The window has no focused pointer with an initiating button serial.
    MissingPointerPress,
}

impl fmt::Display for WaylandDragError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unsupported => "Wayland data-device manager is unavailable",
            Self::EmptyOfferSet => "Wayland drag has no MIME payloads",
            Self::InvalidMimeType => "Wayland drag contains an empty MIME type",
            Self::AlreadyActive => "another Wayland drag is already active",
            Self::MissingPointerPress => {
                "Wayland drag is missing a focused initiating pointer press"
            },
        })
    }
}

impl std::error::Error for WaylandDragError {}

/// Short-lived authority for the current focused Wayland pointer press.
pub struct WaylandDragToken {
    serial: NonZeroU32,
}

impl WaylandDragToken {
    pub(crate) const fn new(serial: NonZeroU32) -> Self {
        Self { serial }
    }

    pub(crate) const fn into_serial(self) -> NonZeroU32 {
        self.serial
    }
}

impl fmt::Debug for WaylandDragToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("WaylandDragToken(..)")
    }
}

/// Additional methods on [`ActiveEventLoop`] that are specific to Wayland.
pub trait ActiveEventLoopExtWayland {
    /// True if the [`ActiveEventLoop`] uses Wayland.
    fn is_wayland(&self) -> bool;
}

impl ActiveEventLoopExtWayland for ActiveEventLoop {
    #[inline]
    fn is_wayland(&self) -> bool {
        self.p.is_wayland()
    }
}

/// Additional methods on [`EventLoop`] that are specific to Wayland.
pub trait EventLoopExtWayland {
    /// True if the [`EventLoop`] uses Wayland.
    fn is_wayland(&self) -> bool;
}

impl<T: 'static> EventLoopExtWayland for EventLoop<T> {
    #[inline]
    fn is_wayland(&self) -> bool {
        self.event_loop.is_wayland()
    }
}

/// Additional methods on [`EventLoopBuilder`] that are specific to Wayland.
pub trait EventLoopBuilderExtWayland {
    /// Force using Wayland.
    fn with_wayland(&mut self) -> &mut Self;

    /// Whether to allow the event loop to be created off of the main thread.
    ///
    /// By default, the window is only allowed to be created on the main
    /// thread, to make platform compatibility easier.
    fn with_any_thread(&mut self, any_thread: bool) -> &mut Self;
}

impl<T> EventLoopBuilderExtWayland for EventLoopBuilder<T> {
    #[inline]
    fn with_wayland(&mut self) -> &mut Self {
        self.platform_specific.forced_backend = Some(crate::platform_impl::Backend::Wayland);
        self
    }

    #[inline]
    fn with_any_thread(&mut self, any_thread: bool) -> &mut Self {
        self.platform_specific.any_thread = any_thread;
        self
    }
}

/// Additional methods on [`Window`] that are specific to Wayland.
///
/// [`Window`]: crate::window::Window
pub trait WindowExtWayland {
    /// Returns `xdg_toplevel` of the window or [`None`] if the window is X11 window.
    fn xdg_toplevel(&self) -> Option<NonNull<c_void>>;

    /// Returns whether the current window can start an outbound data-source drag.
    fn wayland_drag_token(&self) -> Option<WaylandDragToken>;

    /// Starts an outbound data-source drag from the current pointer press.
    ///
    /// All protocol objects and callbacks remain on Winit's existing Wayland
    /// connection and event queue.
    fn start_wayland_drag(
        &self,
        token: WaylandDragToken,
        payloads: Vec<WaylandMimeData>,
        handler: Box<dyn WaylandDataSourceHandler>,
    ) -> Result<(), WaylandDragError>;
}

impl WindowExtWayland for Window {
    #[inline]
    fn xdg_toplevel(&self) -> Option<NonNull<c_void>> {
        #[allow(clippy::single_match)]
        match &self.window {
            #[cfg(x11_platform)]
            crate::platform_impl::Window::X(_) => None,
            #[cfg(wayland_platform)]
            crate::platform_impl::Window::Wayland(window) => window.xdg_toplevel(),
        }
    }

    #[inline]
    fn wayland_drag_token(&self) -> Option<WaylandDragToken> {
        match &self.window {
            #[cfg(x11_platform)]
            crate::platform_impl::Window::X(_) => None,
            #[cfg(wayland_platform)]
            crate::platform_impl::Window::Wayland(window) => window.wayland_drag_token(),
        }
    }

    #[inline]
    fn start_wayland_drag(
        &self,
        token: WaylandDragToken,
        payloads: Vec<WaylandMimeData>,
        handler: Box<dyn WaylandDataSourceHandler>,
    ) -> Result<(), WaylandDragError> {
        match &self.window {
            #[cfg(x11_platform)]
            crate::platform_impl::Window::X(_) => Err(WaylandDragError::Unsupported),
            #[cfg(wayland_platform)]
            crate::platform_impl::Window::Wayland(window) => {
                window.start_wayland_drag(token, payloads, handler)
            },
        }
    }
}

/// Additional methods on [`WindowAttributes`] that are specific to Wayland.
pub trait WindowAttributesExtWayland {
    /// Build window with the given name.
    ///
    /// The `general` name sets an application ID, which should match the `.desktop`
    /// file distributed with your program. The `instance` is a `no-op`.
    ///
    /// For details about application ID conventions, see the
    /// [Desktop Entry Spec](https://specifications.freedesktop.org/desktop-entry-spec/desktop-entry-spec-latest.html#desktop-file-id)
    fn with_name(self, general: impl Into<String>, instance: impl Into<String>) -> Self;
}

impl WindowAttributesExtWayland for WindowAttributes {
    #[inline]
    fn with_name(mut self, general: impl Into<String>, instance: impl Into<String>) -> Self {
        self.platform_specific.name =
            Some(crate::platform_impl::ApplicationName::new(general.into(), instance.into()));
        self
    }
}

/// Additional methods on `MonitorHandle` that are specific to Wayland.
pub trait MonitorHandleExtWayland {
    /// Returns the inner identifier of the monitor.
    fn native_id(&self) -> u32;
}

impl MonitorHandleExtWayland for MonitorHandle {
    #[inline]
    fn native_id(&self) -> u32 {
        self.inner.native_identifier()
    }
}
