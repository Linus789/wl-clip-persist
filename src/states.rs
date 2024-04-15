use std::collections::HashMap;
use std::ffi::CStr;
use std::fs::File;
use std::os::unix::fs::MetadataExt;

use wayrs_client::global::{BindError, Global, GlobalExt as _};
use wayrs_client::object::ObjectId;
use wayrs_client::protocol::WlSeat;
use wayrs_client::proxy::Proxy as _;
use wayrs_client::{ConnectError, Connection};
use wayrs_protocols::wlr_data_control_unstable_v1::{
    ZwlrDataControlDeviceV1, ZwlrDataControlManagerV1, ZwlrDataControlOfferV1,
};

use crate::logger;
use crate::settings::Settings;
use crate::wayland::data_control_device_cb;

#[derive(Debug)]
pub(crate) enum WaylandError {
    ConnectError(ConnectError),
    IoError(std::io::Error),
}

#[derive(Debug)]
pub(crate) struct State {
    pub(crate) settings: Settings,
    pub(crate) data_control_manager: ZwlrDataControlManagerV1,
    pub(crate) seats: HashMap<u32, Seat>,
}

#[derive(Debug)]
pub(crate) struct Seat {
    pub(crate) seat_name: u32,
    pub(crate) wl_seat: WlSeat,
    pub(crate) data_control_device: ZwlrDataControlDeviceV1,
    /// We temporarily save the offers because it is unknown whether
    /// they are regular or primary selection offers.
    pub(crate) selection_offers: HashMap<ObjectId, Offer>,
    /// The current regular selection state. Is [`None`], if not activated in settings.
    pub(crate) regular_selection: Option<SeatSelectionState>,
    /// The current primary selection state. Is [`None`], if not activated in settings.
    pub(crate) primary_selection: Option<SeatSelectionState>,
    /// Used to check if we got a new regular selection during the roundtrip.
    pub(crate) got_new_regular_selection: bool,
    /// Used to check if we got a new primary selection during the roundtrip.
    pub(crate) got_new_primary_selection: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum SelectionType {
    Regular,
    Primary,
}

impl SelectionType {
    pub(crate) const fn get_clipboard_type_str(&self, title_case: bool) -> &'static str {
        match self {
            SelectionType::Regular => logger::get_clipboard_type_str(false, title_case),
            SelectionType::Primary => logger::get_clipboard_type_str(true, title_case),
        }
    }
}

impl Seat {
    /// Binds a [`Global`] to create a new [`Seat`].
    ///
    /// The global should be a seat.
    pub(crate) fn bind(
        connection: &mut Connection<State>,
        data_control_manager: ZwlrDataControlManagerV1,
        global: &Global,
        settings: &Settings,
    ) -> Result<Self, BindError> {
        let seat = global.bind(connection, 1..=9)?;
        let seat_name = global.name;
        let data_control_device =
            data_control_manager.get_data_device_with_cb(connection, seat, move |event_context| {
                data_control_device_cb(seat_name, event_context)
            });

        Ok(Self {
            seat_name: global.name,
            wl_seat: seat,
            data_control_device,
            selection_offers: HashMap::with_capacity(2),
            regular_selection: settings.clipboard_type.regular().then(SeatSelectionState::default),
            primary_selection: settings.clipboard_type.primary().then(SeatSelectionState::default),
            got_new_regular_selection: false,
            got_new_primary_selection: false,
        })
    }

    /// Iterates over mutable regular and primary selection data.
    pub(crate) fn selections_iter_mut(
        &mut self,
    ) -> impl Iterator<Item = (SelectionType, Option<&mut SeatSelectionState>)> {
        std::iter::once((SelectionType::Regular, self.regular_selection.as_mut())).chain(std::iter::once((
            SelectionType::Primary,
            self.primary_selection.as_mut(),
        )))
    }

    /// Iterates over mutable regular and primary selection data, with the selection offers also being present.
    pub(crate) fn selections_iter_mut_with_selection_offers(
        &mut self,
    ) -> impl Iterator<
        Item = (
            SelectionType,
            Option<&mut SeatSelectionState>,
            &HashMap<ObjectId, Offer>,
        ),
    > {
        std::iter::once((
            SelectionType::Regular,
            self.regular_selection.as_mut(),
            &self.selection_offers,
        ))
        .chain(std::iter::once((
            SelectionType::Primary,
            self.primary_selection.as_mut(),
            &self.selection_offers,
        )))
    }

    /// Destroys the seat.
    pub(crate) fn destroy(self, conn: &mut Connection<State>) {
        for offer in self.selection_offers.into_values() {
            offer.data_control_offer.destroy(conn);
        }

        if let Some(mut regular_selection) = self.regular_selection {
            regular_selection.destroy(conn);
        }

        if let Some(mut primary_selection) = self.primary_selection {
            primary_selection.destroy(conn);
        }

        self.data_control_device.destroy(conn);

        if self.wl_seat.version() >= 5 {
            self.wl_seat.release(conn);
        }
    }
}

#[derive(Debug)]
pub(crate) struct Offer {
    pub(crate) data_control_offer: ZwlrDataControlOfferV1,
    pub(crate) mime_types: Vec<std::ffi::CString>,
    pub(crate) bytes_read: u64,
    pub(crate) bytes_exceeded_limit: bool,
}

impl From<ZwlrDataControlOfferV1> for Offer {
    fn from(data_control_offer: ZwlrDataControlOfferV1) -> Self {
        Self {
            data_control_offer,
            mime_types: Vec::with_capacity(32),
            bytes_read: 0,
            bytes_exceeded_limit: false,
        }
    }
}

#[derive(Debug)]
pub(crate) struct MimeTypeAndPipe {
    pub(crate) mime_type: Box<CStr>,
    pub(crate) pipe: tokio_pipe::PipeRead,
    pub(crate) data_read: Option<Result<Vec<u8>, ReadToDataError>>,
    pub(crate) read_finished: bool,
}

#[derive(Debug)]
pub(crate) struct PipeDataResult<'a> {
    pub(crate) mime_type_and_pipe: &'a mut MimeTypeAndPipe,
    pub(crate) data_result: Result<(), ReadToDataError>,
}

#[derive(Debug)]
pub(crate) enum ReadToDataError {
    IoError(std::io::Error),
    SizeLimitExceeded,
}

#[derive(Debug)]
pub(crate) struct MimeTypesWithData<'a> {
    pub(crate) seat_name: u32,
    pub(crate) data_control_device: ZwlrDataControlDeviceV1,
    pub(crate) selection_offers: &'a HashMap<ObjectId, Offer>,
    pub(crate) selection_state: &'a mut SeatSelectionState,
    pub(crate) selection_type: SelectionType,
    pub(crate) data: HashMap<Box<CStr>, Box<[u8]>>,
}

/// Describes the current state of handling the selection event.
#[derive(Debug, Default)]
pub(crate) enum SeatSelectionState {
    /// Waiting for new offers.
    #[default]
    WaitingForNewOffers,
    /// We read the mime types of the offer.
    ReadMimes {
        data_control_offer: ZwlrDataControlOfferV1,
        mime_types: Vec<std::ffi::CString>,
        bytes_read: u64,
        fd_from_own_app: HashMap<FdIdentifier, bool>,
    },
    /// We got the pipes for the current offer.
    /// The pipes are not from ourselves.
    GotPipes {
        pipes: Vec<MimeTypeAndPipe>,
        bytes_read: u64,
    },
    /// We got the data, but cannot set the clipboard yet.
    /// We use this state, to minimize the risk of data races.
    /// The data race we avoid would be triggered by the following sequence of events:
    ///
    /// 1. Read pipes of new clipboard event
    /// 2. We receive a new offer, but do not know whether the offer will be a regular or primary selection event yet
    /// 3. While we wait for that info, the pipes have been fully read, and we set the data as the new clipboard
    ///    => By doing that, we overwrite the new offer if it is from the same selection type
    ///    => Therefore, if there is an offer whose selection type is unknown, we should wait before setting the clipboard
    GotData { data: HashMap<Box<CStr>, Box<[u8]>> },
    /// The selection was cleared.
    GotClear,
    /// The selection was updated, but we will not save the data in memory.
    GotIgnoredEvent,
}

impl SeatSelectionState {
    /// Switches to the initial state [`SeatSelectionState::WaitingForNewOffers`].
    pub(crate) fn reset(&mut self, conn: &mut Connection<State>) {
        // Destroy old wayland objects
        self.destroy(conn);

        *self = SeatSelectionState::WaitingForNewOffers;
    }

    /// Switches to the [`SeatSelectionState::ReadMimes`] state.
    pub(crate) fn read_mimes(
        &mut self,
        conn: &mut Connection<State>,
        data_control_offer: ZwlrDataControlOfferV1,
        mime_types: Vec<std::ffi::CString>,
        bytes_read: u64,
    ) {
        // Destroy old wayland objects
        self.destroy(conn);

        *self = SeatSelectionState::ReadMimes {
            data_control_offer,
            mime_types,
            bytes_read,
            fd_from_own_app: HashMap::with_capacity(32),
        };
    }

    /// Switches to the [`SeatSelectionState::GotPipes`] state.
    pub(crate) fn got_pipes(&mut self, conn: &mut Connection<State>, pipes: Vec<MimeTypeAndPipe>) {
        let SeatSelectionState::ReadMimes {
            data_control_offer,
            mime_types: _,
            bytes_read,
            fd_from_own_app: _,
        } = self
        else {
            // The state already got updated to something newer, therefore return early
            return;
        };

        // Destroy unneeded wayland object
        data_control_offer.destroy(conn);

        *self = SeatSelectionState::GotPipes {
            pipes,
            bytes_read: *bytes_read,
        };
    }

    /// Switches to the [`SeatSelectionState::GotData`] state.
    pub(crate) fn got_data(&mut self, data: HashMap<Box<CStr>, Box<[u8]>>) {
        let SeatSelectionState::GotPipes {
            pipes: _,
            bytes_read: _,
        } = self
        else {
            // The state already got updated to something newer, therefore return early
            return;
        };

        *self = SeatSelectionState::GotData { data };
    }

    /// Switches to the [`SeatSelectionState::GotClear`] state.
    pub(crate) fn got_clear(&mut self, conn: &mut Connection<State>) {
        // Destroy old wayland objects
        self.destroy(conn);

        *self = SeatSelectionState::GotClear;
    }

    /// Switches to the [`SeatSelectionState::GotIgnoredEvent`] state.
    pub(crate) fn got_ignored_event(&mut self, conn: &mut Connection<State>) {
        // Destroy old wayland objects
        self.destroy(conn);

        *self = SeatSelectionState::GotIgnoredEvent;
    }

    /// Destroys wayland objects in current state.
    pub(crate) fn destroy(&mut self, conn: &mut Connection<State>) {
        match self {
            SeatSelectionState::WaitingForNewOffers => {}
            SeatSelectionState::ReadMimes {
                data_control_offer,
                mime_types: _,
                bytes_read: _,
                fd_from_own_app: _,
            } => {
                data_control_offer.destroy(conn);
            }
            SeatSelectionState::GotPipes {
                pipes: _,
                bytes_read: _,
            } => {}
            SeatSelectionState::GotData { data: _ } => {}
            SeatSelectionState::GotClear => {}
            SeatSelectionState::GotIgnoredEvent => {}
        };
    }
}

/// A unique identifier for file descriptors.
/// The device id and inode number should make a
/// file descriptor uniquely identifiable.
#[derive(Debug, PartialEq, Eq, Hash)]
pub(crate) struct FdIdentifier {
    /// The device id of the file descriptor
    dev: u64,
    /// The inode number of the file descriptor
    ino: u64,
}

impl TryFrom<&File> for FdIdentifier {
    type Error = std::io::Error;

    fn try_from(file: &File) -> Result<Self, Self::Error> {
        let file_metadata = file.metadata()?;
        Ok(FdIdentifier {
            dev: file_metadata.dev(),
            ino: file_metadata.ino(),
        })
    }
}
