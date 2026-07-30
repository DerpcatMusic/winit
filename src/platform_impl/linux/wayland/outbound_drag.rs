use std::io::Write;
use std::sync::{Arc, Mutex};

use sctk::data_device_manager::data_device::{DataDevice, DataDeviceHandler};
use sctk::data_device_manager::data_offer::{DataOfferHandler, DragOffer};
use sctk::data_device_manager::data_source::{DataSourceHandler, DragSource};
use sctk::data_device_manager::{DataDeviceManagerState, WritePipe};
use sctk::reexports::client::protocol::wl_data_device::WlDataDevice;
use sctk::reexports::client::protocol::wl_data_device_manager::DndAction;
use sctk::reexports::client::protocol::wl_data_source::WlDataSource;
use sctk::reexports::client::protocol::wl_surface::WlSurface;
use sctk::reexports::client::{Connection, QueueHandle};

use crate::platform::wayland::{
    WaylandDataSourceEvent, WaylandDataSourceHandler, WaylandDragAction, WaylandDragError,
    WaylandMimeData,
};

use super::state::WinitState;

pub struct OutboundDragState {
    manager: Option<Arc<DataDeviceManagerState>>,
    active: Option<ActiveDrag>,
}

struct ActiveDrag {
    _device: DataDevice,
    source: DragSource,
    payloads: Vec<WaylandMimeData>,
    handler: Option<Box<dyn WaylandDataSourceHandler>>,
}

impl OutboundDragState {
    pub fn new(manager: Option<Arc<DataDeviceManagerState>>) -> Self {
        Self { manager, active: None }
    }

    pub fn is_available(&self) -> bool {
        self.manager.is_some() && self.active.is_none()
    }

    pub fn start(
        &mut self,
        queue: &QueueHandle<WinitState>,
        origin: &WlSurface,
        seat: &sctk::reexports::client::protocol::wl_seat::WlSeat,
        serial: u32,
        payloads: Vec<WaylandMimeData>,
        handler: Box<dyn WaylandDataSourceHandler>,
    ) -> Result<(), WaylandDragError> {
        if payloads.is_empty() {
            return Err(WaylandDragError::EmptyOfferSet);
        }
        if payloads.iter().any(|payload| payload.mime_type().trim().is_empty()) {
            return Err(WaylandDragError::InvalidMimeType);
        }
        if self.active.is_some() {
            return Err(WaylandDragError::AlreadyActive);
        }
        let manager = self.manager.as_ref().ok_or(WaylandDragError::Unsupported)?;
        let device = manager.get_data_device(queue, seat);
        let source = manager.create_drag_and_drop_source(
            queue,
            payloads.iter().map(WaylandMimeData::mime_type),
            DndAction::Copy,
        );
        source.start_drag(&device, origin, None, serial);
        self.active =
            Some(ActiveDrag { _device: device, source, payloads, handler: Some(handler) });
        Ok(())
    }

    fn active_mut(&mut self, source: &WlDataSource) -> Option<&mut ActiveDrag> {
        self.active.as_mut().filter(|active| active.source.inner() == source)
    }

    fn take_handler(&mut self, source: &WlDataSource) -> Option<Box<dyn WaylandDataSourceHandler>> {
        self.active_mut(source)?.handler.take()
    }

    fn restore_handler(
        &mut self,
        source: &WlDataSource,
        handler: Box<dyn WaylandDataSourceHandler>,
    ) {
        if let Some(active) = self.active_mut(source) {
            active.handler = Some(handler);
        }
    }

    fn take_terminal(&mut self, source: &WlDataSource) -> Option<ActiveDrag> {
        if self.active.as_ref().is_some_and(|active| active.source.inner() == source) {
            self.active.take()
        } else {
            None
        }
    }
}

fn notify(state: &SharedOutboundDrag, source: &WlDataSource, event: WaylandDataSourceEvent) {
    let Some(mut handler) = state.lock().unwrap().take_handler(source) else {
        return;
    };
    handler.event(event);
    state.lock().unwrap().restore_handler(source, handler);
}

fn finish(state: &SharedOutboundDrag, source: &WlDataSource, event: WaylandDataSourceEvent) {
    let Some(mut active) = state.lock().unwrap().take_terminal(source) else {
        return;
    };
    if let Some(mut handler) = active.handler.take() {
        handler.event(event);
    }
}

impl DataSourceHandler for WinitState {
    fn accept_mime(
        &mut self,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
        source: &WlDataSource,
        mime: Option<String>,
    ) {
        notify(&self.outbound_drag, source, WaylandDataSourceEvent::MimeAccepted(mime));
    }

    fn send_request(
        &mut self,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
        source: &WlDataSource,
        mime: String,
        mut pipe: WritePipe,
    ) {
        let payload = {
            let mut state = self.outbound_drag.lock().unwrap();
            let Some(active) = state.active_mut(source) else {
                return;
            };
            let Some(payload) = active.payloads.iter().find(|payload| payload.mime_type() == mime)
            else {
                return;
            };
            payload.data().to_vec()
        };
        let event = match pipe.write_all(&payload).and_then(|()| pipe.flush()) {
            Ok(()) => WaylandDataSourceEvent::DataRequested(mime),
            Err(error) => WaylandDataSourceEvent::DataTransferFailed { mime_type: mime, error },
        };
        notify(&self.outbound_drag, source, event);
    }

    fn cancelled(
        &mut self,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
        source: &WlDataSource,
    ) {
        finish(&self.outbound_drag, source, WaylandDataSourceEvent::Cancelled);
    }

    fn dnd_dropped(
        &mut self,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
        source: &WlDataSource,
    ) {
        notify(&self.outbound_drag, source, WaylandDataSourceEvent::DropPerformed);
    }

    fn dnd_finished(
        &mut self,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
        source: &WlDataSource,
    ) {
        finish(&self.outbound_drag, source, WaylandDataSourceEvent::Finished);
    }

    fn action(
        &mut self,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
        source: &WlDataSource,
        action: DndAction,
    ) {
        let action = if action.is_empty() {
            WaylandDragAction::None
        } else if action == DndAction::Copy {
            WaylandDragAction::Copy
        } else if action == DndAction::Move {
            WaylandDragAction::Move
        } else if action == DndAction::Ask {
            WaylandDragAction::Ask
        } else {
            WaylandDragAction::Unknown
        };
        notify(&self.outbound_drag, source, WaylandDataSourceEvent::ActionSelected(action));
    }
}

impl DataDeviceHandler for WinitState {
    fn enter(
        &mut self,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
        _device: &WlDataDevice,
        _x: f64,
        _y: f64,
        _surface: &WlSurface,
    ) {
    }

    fn leave(
        &mut self,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
        _device: &WlDataDevice,
    ) {
    }

    fn motion(
        &mut self,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
        _device: &WlDataDevice,
        _x: f64,
        _y: f64,
    ) {
    }

    fn selection(
        &mut self,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
        _device: &WlDataDevice,
    ) {
    }

    fn drop_performed(
        &mut self,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
        _device: &WlDataDevice,
    ) {
    }
}

impl DataOfferHandler for WinitState {
    fn source_actions(
        &mut self,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
        _offer: &mut DragOffer,
        _actions: DndAction,
    ) {
    }

    fn selected_action(
        &mut self,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
        _offer: &mut DragOffer,
        _actions: DndAction,
    ) {
    }
}

sctk::delegate_data_device!(WinitState);

pub type SharedOutboundDrag = Arc<Mutex<OutboundDragState>>;
