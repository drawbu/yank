//! One face over the two data-control protocols.
//!
//! `ext-data-control-v1` is the standardized version of what
//! `wlr-data-control-unstable-v1` pioneered; the requests and events we
//! use are identical, only the types differ. Compositors ship one, the
//! other, or both, so we bind whichever is there and route through the
//! enums below.
//!
//! The dispatch implementations are generated for both families by the
//! macros at the bottom, which translate each protocol's events into the
//! shared [`DeviceEvent`] and [`SourceEvent`].

use std::os::fd::{BorrowedFd, OwnedFd};

use wayland_client::{Dispatch, QueueHandle, protocol::wl_seat::WlSeat};
use wayland_protocols::ext::data_control::v1::client as ext;
use wayland_protocols_wlr::data_control::v1::client as wlr;

/// The bound manager global, and which protocol it speaks.
#[derive(Clone, Debug)]
pub enum Manager {
    Wlr(wlr::zwlr_data_control_manager_v1::ZwlrDataControlManagerV1),
    Ext(ext::ext_data_control_manager_v1::ExtDataControlManagerV1),
}

/// The per-seat device: where selection events arrive and where a new
/// selection is set.
#[derive(Clone, Debug)]
pub enum Device {
    Wlr(wlr::zwlr_data_control_device_v1::ZwlrDataControlDeviceV1),
    Ext(ext::ext_data_control_device_v1::ExtDataControlDeviceV1),
}

/// A selection we own: the compositor asks it for the bytes.
#[derive(Clone, Debug)]
pub enum Source {
    Wlr(wlr::zwlr_data_control_source_v1::ZwlrDataControlSourceV1),
    Ext(ext::ext_data_control_source_v1::ExtDataControlSourceV1),
}

/// A selection somebody else owns, which announces its mime types and
/// hands over the bytes on request.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Offer {
    Wlr(wlr::zwlr_data_control_offer_v1::ZwlrDataControlOfferV1),
    Ext(ext::ext_data_control_offer_v1::ExtDataControlOfferV1),
}

/// What a device tells us.
#[derive(Debug)]
pub enum DeviceEvent {
    /// A new offer object, whose mime types arrive next.
    NewOffer(Offer),
    /// The selection changed; `None` means it was emptied.
    Selection(Option<Offer>),
    /// The compositor withdrew the device: nothing more will arrive.
    Finished,
}

/// What a source we own tells us.
#[derive(Debug)]
pub enum SourceEvent {
    /// Somebody is pasting: write the bytes for `mime` into `fd`.
    Send { mime: String, fd: OwnedFd },
    /// Another client took the selection; the source is spent.
    Cancelled,
}

impl Manager {
    /// Gets the data device for `seat`.
    pub fn device<D>(&self, seat: &WlSeat, qh: &QueueHandle<D>) -> Device
    where
        D: Dispatch<wlr::zwlr_data_control_device_v1::ZwlrDataControlDeviceV1, ()> + 'static,
        D: Dispatch<ext::ext_data_control_device_v1::ExtDataControlDeviceV1, ()> + 'static,
    {
        match self {
            Manager::Wlr(manager) => Device::Wlr(manager.get_data_device(seat, qh, ())),
            Manager::Ext(manager) => Device::Ext(manager.get_data_device(seat, qh, ())),
        }
    }

    /// Creates a source to offer a selection with.
    pub fn source<D>(&self, qh: &QueueHandle<D>) -> Source
    where
        D: Dispatch<wlr::zwlr_data_control_source_v1::ZwlrDataControlSourceV1, ()> + 'static,
        D: Dispatch<ext::ext_data_control_source_v1::ExtDataControlSourceV1, ()> + 'static,
    {
        match self {
            Manager::Wlr(manager) => Source::Wlr(manager.create_data_source(qh, ())),
            Manager::Ext(manager) => Source::Ext(manager.create_data_source(qh, ())),
        }
    }

    /// The protocol in use, for the logs.
    pub fn protocol(&self) -> &'static str {
        match self {
            Manager::Wlr(_) => "wlr-data-control",
            Manager::Ext(_) => "ext-data-control",
        }
    }
}

impl Device {
    /// Takes the selection with `source`, or empties it with `None`.
    pub fn set_selection(&self, source: Option<&Source>) {
        match (self, source) {
            (Device::Wlr(device), Some(Source::Wlr(source))) => {
                device.set_selection(Some(source));
            }
            (Device::Wlr(device), None) => device.set_selection(None),
            (Device::Ext(device), Some(Source::Ext(source))) => {
                device.set_selection(Some(source));
            }
            (Device::Ext(device), None) => device.set_selection(None),
            // Both come from the same manager, so the families always
            // match; the arm exists because the types cannot say so.
            _ => unreachable!("device and source speak the same protocol"),
        }
    }
}

impl Source {
    /// Announces one mime type this source can produce.
    pub fn offer(&self, mime: String) {
        match self {
            Source::Wlr(source) => source.offer(mime),
            Source::Ext(source) => source.offer(mime),
        }
    }

    pub fn destroy(&self) {
        match self {
            Source::Wlr(source) => source.destroy(),
            Source::Ext(source) => source.destroy(),
        }
    }

    /// Whether this is the same source object.
    pub fn is(&self, other: &Source) -> bool {
        use wayland_client::Proxy as _;

        match (self, other) {
            (Source::Wlr(a), Source::Wlr(b)) => a.id() == b.id(),
            (Source::Ext(a), Source::Ext(b)) => a.id() == b.id(),
            _ => false,
        }
    }
}

impl Offer {
    /// Asks for the selection's bytes in `mime`, written to `fd`.
    pub fn receive(&self, mime: String, fd: BorrowedFd<'_>) {
        match self {
            Offer::Wlr(offer) => offer.receive(mime, fd),
            Offer::Ext(offer) => offer.receive(mime, fd),
        }
    }

    pub fn destroy(&self) {
        match self {
            Offer::Wlr(offer) => offer.destroy(),
            Offer::Ext(offer) => offer.destroy(),
        }
    }
}

/// Generates the no-op dispatch for the two manager globals, which have no
/// events.
macro_rules! impl_manager_dispatch {
    ($handler:ty) => {
        $crate::clip::wayland::protocol::impl_ignored_dispatch!(
            $handler;
            wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
            wayland_protocols::ext::data_control::v1::client::ext_data_control_manager_v1::ExtDataControlManagerV1,
            wayland_client::protocol::wl_seat::WlSeat,
        );
    };
}

macro_rules! impl_ignored_dispatch {
    ($handler:ty; $($iface:ty),* $(,)?) => { $(
        impl wayland_client::Dispatch<$iface, ()> for $handler {
            fn event(
                _state: &mut Self,
                _proxy: &$iface,
                _event: <$iface as wayland_client::Proxy>::Event,
                _data: &(),
                _conn: &wayland_client::Connection,
                _qh: &wayland_client::QueueHandle<Self>,
            ) {
            }
        }
    )* };
}

/// Generates the device dispatch for both protocols. `$handler` must have
/// an `on_device(&mut self, DeviceEvent)` method.
macro_rules! impl_device_dispatch {
    ($handler:ty) => {
        $crate::clip::wayland::protocol::impl_device_dispatch_for!(
            $handler;
            (
                wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_device_v1::ZwlrDataControlDeviceV1,
                wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE,
                wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_offer_v1::ZwlrDataControlOfferV1,
                Wlr
            ),
            (
                wayland_protocols::ext::data_control::v1::client::ext_data_control_device_v1::ExtDataControlDeviceV1,
                wayland_protocols::ext::data_control::v1::client::ext_data_control_device_v1::EVT_DATA_OFFER_OPCODE,
                wayland_protocols::ext::data_control::v1::client::ext_data_control_offer_v1::ExtDataControlOfferV1,
                Ext
            ),
        );
    };
}

macro_rules! impl_device_dispatch_for {
    ($handler:ty; $(($iface:ty, $opcode:path, $offer:ty, $family:ident)),* $(,)?) => { $(
        impl wayland_client::Dispatch<$iface, ()> for $handler {
            fn event(
                state: &mut Self,
                _proxy: &$iface,
                event: <$iface as wayland_client::Proxy>::Event,
                _data: &(),
                _conn: &wayland_client::Connection,
                _qh: &wayland_client::QueueHandle<Self>,
            ) {
                type Event = <$iface as wayland_client::Proxy>::Event;
                use $crate::clip::wayland::protocol::{DeviceEvent, Offer};

                let event = match event {
                    Event::DataOffer { id } => DeviceEvent::NewOffer(Offer::$family(id)),
                    Event::Selection { id } => DeviceEvent::Selection(id.map(Offer::$family)),
                    Event::Finished => DeviceEvent::Finished,
                    // The primary selection, and anything a later version
                    // of the protocol adds.
                    _ => return,
                };
                state.on_device(event);
            }

            wayland_client::event_created_child!($handler, $iface, [
                $opcode => ($offer, ()),
            ]);
        }
    )* };
}

/// Generates the offer dispatch for both protocols. `$handler` must have
/// an `on_offer_mime(&mut self, Offer, String)` method.
macro_rules! impl_offer_dispatch {
    ($handler:ty) => {
        $crate::clip::wayland::protocol::impl_offer_dispatch_for!(
            $handler;
            (wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_offer_v1::ZwlrDataControlOfferV1, Wlr),
            (wayland_protocols::ext::data_control::v1::client::ext_data_control_offer_v1::ExtDataControlOfferV1, Ext),
        );
    };
}

macro_rules! impl_offer_dispatch_for {
    ($handler:ty; $(($iface:ty, $family:ident)),* $(,)?) => { $(
        impl wayland_client::Dispatch<$iface, ()> for $handler {
            fn event(
                state: &mut Self,
                proxy: &$iface,
                event: <$iface as wayland_client::Proxy>::Event,
                _data: &(),
                _conn: &wayland_client::Connection,
                _qh: &wayland_client::QueueHandle<Self>,
            ) {
                type Event = <$iface as wayland_client::Proxy>::Event;
                use $crate::clip::wayland::protocol::Offer;

                if let Event::Offer { mime_type } = event {
                    state.on_offer_mime(&Offer::$family(proxy.clone()), mime_type);
                }
            }
        }
    )* };
}

/// Generates the source dispatch for both protocols. `$handler` must have
/// an `on_source(&mut self, Source, SourceEvent)` method.
macro_rules! impl_source_dispatch {
    ($handler:ty) => {
        $crate::clip::wayland::protocol::impl_source_dispatch_for!(
            $handler;
            (wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_source_v1::ZwlrDataControlSourceV1, Wlr),
            (wayland_protocols::ext::data_control::v1::client::ext_data_control_source_v1::ExtDataControlSourceV1, Ext),
        );
    };
}

macro_rules! impl_source_dispatch_for {
    ($handler:ty; $(($iface:ty, $family:ident)),* $(,)?) => { $(
        impl wayland_client::Dispatch<$iface, ()> for $handler {
            fn event(
                state: &mut Self,
                proxy: &$iface,
                event: <$iface as wayland_client::Proxy>::Event,
                _data: &(),
                _conn: &wayland_client::Connection,
                _qh: &wayland_client::QueueHandle<Self>,
            ) {
                type Event = <$iface as wayland_client::Proxy>::Event;
                use $crate::clip::wayland::protocol::{Source, SourceEvent};

                let event = match event {
                    Event::Send { mime_type, fd } => SourceEvent::Send {
                        mime: mime_type,
                        fd,
                    },
                    Event::Cancelled => SourceEvent::Cancelled,
                    _ => return,
                };
                state.on_source(&Source::$family(proxy.clone()), event);
            }
        }
    )* };
}

pub(super) use {
    impl_device_dispatch, impl_manager_dispatch, impl_offer_dispatch, impl_source_dispatch,
};
pub(crate) use {
    impl_device_dispatch_for, impl_ignored_dispatch, impl_offer_dispatch_for,
    impl_source_dispatch_for,
};
