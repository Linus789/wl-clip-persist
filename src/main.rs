mod pipe_io;

use std::borrow::Borrow;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs::File;
use std::ops::Deref;
use std::os::unix::prelude::{AsRawFd, FromRawFd, IntoRawFd, MetadataExt};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use clap::builder::NonEmptyStringValueParser;
use clap::{arg, crate_description, crate_name, crate_version, value_parser, Command, ValueEnum};
use fancy_regex::Regex;
use filedescriptor::{FileDescriptor, Pipe};
use wayland_client::protocol::wl_display::WlDisplay;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{
    Attached, DispatchData, Display, EventQueue, GlobalEvent, GlobalImplementor, GlobalManager, Interface, Main, Proxy,
};
use wayland_protocols::wlr::unstable::data_control::v1::client::zwlr_data_control_device_v1::{
    Event as DeviceEvent, ZwlrDataControlDeviceV1,
};
use wayland_protocols::wlr::unstable::data_control::v1::client::zwlr_data_control_manager_v1::ZwlrDataControlManagerV1;
use wayland_protocols::wlr::unstable::data_control::v1::client::zwlr_data_control_offer_v1::{
    Event as OfferEvent, ZwlrDataControlOfferV1,
};
use wayland_protocols::wlr::unstable::data_control::v1::client::zwlr_data_control_source_v1::{
    Event as SourceEvent, ZwlrDataControlSourceV1,
};

fn main() {
    // Initialize logger
    env_logger::builder()
        .filter_level(log::LevelFilter::Info)
        .parse_default_env()
        .format_timestamp_millis()
        .init();

    // Parse command line args
    let mut command = Command::new(crate_name!()).version(crate_version!());
    let description = crate_description!();

    if !description.is_empty() {
        command = command.about(description);
    }

    let matches = command
        .arg(
            arg!(
                -c --clipboard <TYPE> "The clipboard type to operate on"
            )
            .required(true)
            .value_parser(value_parser!(ClipboardType)),
        )
        .arg(
            arg!(
                -d --display <NAME> "The wayland display to operate on"
            )
            .required(false)
            .value_parser(NonEmptyStringValueParser::new()),
        )
        .arg(
            arg!(
                -r --"read-timeout" <MILLISECONDS> "Timeout for trying to get the current clipboard"
            )
            .required(false)
            .value_parser(clap::value_parser!(u64).range(1..=i32::MAX as u64))
            .default_value("500"),
        )
        .arg(
            arg!(
                -w --"write-timeout" <MILLISECONDS> "Timeout for trying to send the current clipboard to other programs"
            )
            .required(false)
            .value_parser(clap::value_parser!(u64).range(1..=i32::MAX as u64))
            .default_value("3000"),
        )
        .arg(
            arg!(
                -t --"ignore-event-on-timeout" "Only handle selection events where no timeout occurred"
            )
            .required(false),
        )
        .arg(
            arg!(
                -l --"selection-size-limit" <BYTES> "Only handle selection events whose total data size does not exceed the size limit"
            )
            .required(false)
            .value_parser(clap::value_parser!(u64).range(1..)),
        )
        .arg(
            arg!(
                -f --"all-mime-type-regex" <REGEX> "Only handle selection events where all offered MIME types have a match for the regex"
            )
            .required(false)
            .value_parser(NonEmptyStringValueParser::new()),
        )
        .get_matches();

    let clipboard_type = *matches.get_one::<ClipboardType>("clipboard").unwrap();
    let display_name = matches.get_one::<String>("display").map(|s| s.into());
    let read_timeout = Duration::from_millis(*matches.get_one::<u64>("read-timeout").unwrap());
    let write_timeout = Duration::from_millis(*matches.get_one::<u64>("write-timeout").unwrap());
    let ignore_selection_event_on_timeout = matches.contains_id("ignore-event-on-timeout");
    let selection_size_limit_bytes = matches
        .get_one::<u64>("selection-size-limit")
        .copied()
        .unwrap_or(u64::MAX);
    let all_mime_type_regex = matches
        .get_one::<String>("mime-type-regex")
        .map(|s| match Regex::new(s) {
            Ok(regex) => regex,
            Err(err) => {
                log::error!(
                    target: log_default_target(),
                    "Failed to parse the mime type regex. Error: {}",
                    err
                );
                std::process::exit(1);
            }
        });

    // Run main program
    handle_clipboard(Settings {
        clipboard_type,
        display_name,
        read_timeout,
        write_timeout,
        ignore_selection_event_on_timeout,
        selection_size_limit_bytes,
        all_mime_type_regex,
    });
}

/// The settings that have been specified via arguments.
struct Settings {
    clipboard_type: ClipboardType,
    display_name: Option<OsString>,
    read_timeout: Duration,
    write_timeout: Duration,
    ignore_selection_event_on_timeout: bool,
    selection_size_limit_bytes: u64,
    all_mime_type_regex: Option<Regex>,
}

/// Holds data for the primary and regular selection.
#[derive(Default)]
struct SelectionsData {
    primary: SingleSelectionData,
    regular: SingleSelectionData,
}

impl SelectionsData {
    /// Iterate over mutable primary and regular selection data.
    /// The boolean indicates whether it is the primary selection.
    fn iter_mut(&mut self) -> impl Iterator<Item = (&mut SingleSelectionData, bool)> {
        std::iter::once((&mut self.primary, true)).chain(std::iter::once((&mut self.regular, false)))
    }
}

#[derive(Default)]
struct SingleSelectionData {
    /// Currently available selection offer, if any.
    offer: Option<ZwlrDataControlOfferV1>,
    /// If available, mime types associated with pipes you can read the data from.
    pipes: Option<OfferPipeData>,
    /// A map that keeps track of file descriptors that are the writable end of the pipe.
    /// The boolean value expresses whether the file descriptor arrived at our
    /// own program to send some text to it. This way we can identify selection offer events
    /// that got triggered by ourselves.
    fd_from_own_app: Rc<RefCell<HashMap<FdIdentifier, bool>>>,
}

// The offer data hold the available mime types,
// the total size of the mime types text
// and whether it already exceeded the size limit.
struct OfferData {
    mime_types: HashSet<Box<str>>,
    mime_types_size_bytes: u64,
    exceeded_size_limit: bool,
}

impl Default for OfferData {
    fn default() -> Self {
        OfferData {
            mime_types: HashSet::with_capacity(32),
            mime_types_size_bytes: 0,
            exceeded_size_limit: false,
        }
    }
}

/// Get the clipboard data device for the given seat.
fn get_data_device(seat: &Proxy<WlSeat>) -> &RefCell<Option<Main<ZwlrDataControlDeviceV1>>> {
    seat.user_data()
        .get::<Rc<RefCell<Option<Main<ZwlrDataControlDeviceV1>>>>>()
        .unwrap()
        .deref()
}

/// Get the selections data for the given clipboard data device.
fn get_selections_data(data_device: &Proxy<ZwlrDataControlDeviceV1>) -> &RefCell<SelectionsData> {
    data_device.user_data().get::<RefCell<SelectionsData>>().unwrap()
}

/// Get the offer data for the given selection offer.
fn get_offer_data(offer: &Proxy<ZwlrDataControlOfferV1>) -> &RefCell<OfferData> {
    offer.user_data().get::<RefCell<OfferData>>().unwrap()
}

/// Create a GlobalManager that adds new seats to the list
/// and removes seats that should be removed.
/// Returns the global manager and a list of always up-to-date seats.
fn init_global_manager_with_seats(
    display: &Attached<WlDisplay>,
    clipboard_manager: &Rc<RefCell<Option<Main<ZwlrDataControlManagerV1>>>>,
    clipboard_type: ClipboardType,
    selection_size_limit_bytes: u64,
) -> (GlobalManager, Rc<RefCell<HashMap<u32, Main<WlSeat>>>>) {
    let seats = Rc::new(RefCell::new(HashMap::<u32, Main<WlSeat>>::new()));
    let seats2 = seats.clone();
    let clipboard_manager2 = clipboard_manager.clone();
    let global_manager = GlobalManager::new_with_cb(display, move |global_event, registry, dispatch_data| {
        const SEAT_INTERFACE_NAME: &str = WlSeat::NAME;
        const SEAT_MIN_VERSION: u32 = 2;

        let mut global_implementor = |seat: Main<WlSeat>, _: DispatchData| {
            if let Some(clipboard_manager_instance) = clipboard_manager2.deref().borrow().as_ref() {
                // Seems to be a seat that got added later, so initialize it
                init_seat(
                    &seat,
                    clipboard_manager_instance,
                    clipboard_type,
                    selection_size_limit_bytes,
                );
            }

            // Add seat to list
            let seat_id = seat.as_ref().id();

            if seats2.deref().borrow_mut().insert(seat_id, seat).is_some() {
                panic!("A seat with the same id {} was already present", seat_id);
            }
        };

        match global_event {
            // A new global was created
            GlobalEvent::New { id, interface, version } => {
                if interface == SEAT_INTERFACE_NAME {
                    if version < SEAT_MIN_VERSION {
                        GlobalImplementor::<WlSeat>::error(&mut global_implementor, version, dispatch_data);
                    } else {
                        let proxy = registry.bind::<WlSeat>(version, id);
                        GlobalImplementor::<WlSeat>::new_global(&mut global_implementor, proxy, dispatch_data);
                    }
                }
            }
            // A global was removed
            GlobalEvent::Removed { id, interface } => {
                if interface == SEAT_INTERFACE_NAME {
                    // Remove seat from list
                    if seats2.deref().borrow_mut().remove(&id).is_some() {
                        log::trace!(target: &log_seat_target(id), "Removed seat");
                    }
                }
            }
        }
    });
    (global_manager, seats)
}

/// Initializes a seat by assigning a handler for the selection offer events
/// and creating all the necessary infrastructure around it.
fn init_seat(
    seat: &Main<WlSeat>,
    clipboard_manager: &Main<ZwlrDataControlManagerV1>,
    clipboard_type: ClipboardType,
    selection_size_limit_bytes: u64,
) {
    // Create a data device that can be used to manage a seat's selection.
    let data_device: Rc<RefCell<Option<Main<ZwlrDataControlDeviceV1>>>> =
        Rc::new(RefCell::new(Some(clipboard_manager.get_data_device(seat))));

    {
        let data_device_ref = data_device.deref().borrow();
        let data_device_instance = data_device_ref.as_ref().unwrap();

        // Put available offers in the user data of the data device
        let selections_data: RefCell<SelectionsData> = RefCell::new(SelectionsData::default());
        data_device_instance.as_ref().user_data().set(move || selections_data);

        // Assign handler for selection offer events
        let data_device2 = data_device.clone();
        data_device_instance.quick_assign(move |data_device, event, _| match event {
            // This event is triggered immediately before
            // DeviceEvent::PrimarySelection or DeviceEvent::Selection
            // and is used to describe the mime type that are offered
            DeviceEvent::DataOffer { id } => {
                // Create HashSet where all available mime types are saved
                id.as_ref().user_data().set(move || RefCell::new(OfferData::default()));

                // Save mime type in that HashSet for each mime type we are notified about
                id.quick_assign(move |offer, event, _| {
                    if let OfferEvent::Offer { mime_type } = event {
                        let mut offer_data = get_offer_data(offer.as_ref()).borrow_mut();

                        if offer_data.exceeded_size_limit {
                            // We already exceeded the limit, we can ignore the rest.
                            return;
                        }

                        // Check size limit.
                        offer_data.mime_types_size_bytes += mime_type.len() as u64;

                        if offer_data.mime_types_size_bytes > selection_size_limit_bytes {
                            offer_data.exceeded_size_limit = true;
                            return;
                        }

                        offer_data.mime_types.insert(mime_type.into_boxed_str());
                    }
                });
            }
            // Advertises a new primary selection
            DeviceEvent::PrimarySelection { id } => {
                if !clipboard_type.primary() {
                    // We are not interested in the primary clipboard,
                    // so just destroy that offer, if any, and return.
                    if let Some(offer) = id {
                        offer.destroy();
                    }
                    return;
                }

                let mut selections_data = get_selections_data(data_device.as_ref()).borrow_mut();

                // If there is an old offer in the primary selection data, destroy it
                if let Some(old_offer) = selections_data.primary.offer.take() {
                    old_offer.destroy();
                }

                // Update primary selection offer
                selections_data.primary.offer = id;
            }
            // Advertises a new regular selection
            DeviceEvent::Selection { id } => {
                if !clipboard_type.regular() {
                    // We are not interested in the regular clipboard,
                    // so just destroy that offer, if any, and return.
                    if let Some(offer) = id {
                        offer.destroy();
                    }
                    return;
                }

                let mut selections_data = get_selections_data(data_device.as_ref()).borrow_mut();

                // If there is an old offer in the regular selection data, destroy it
                if let Some(old_offer) = selections_data.regular.offer.take() {
                    old_offer.destroy();
                }

                // Update regular selection offer
                selections_data.regular.offer = id;
            }
            // This data device is no longer valid and should be destroyed by the client
            DeviceEvent::Finished => {
                // Set the data device in the seat's user data to None,
                // so it is no longer available.
                *data_device2.deref().borrow_mut() = None;
                // Destroy this data device
                data_device.destroy();
            }
            _ => {}
        });
    }

    // Put data device in the user data of its seat
    seat.as_ref().user_data().set(move || data_device);
    log::trace!(target: &log_seat_target(seat.as_ref().id()), "Initialized seat");
}

/// Makes the selections for the given clipboard persistent.
fn handle_clipboard(mut settings: Settings) {
    // Tries to connect to a wayland server socket by either the given name
    // or if none given by using the environment variables
    let display = match settings.display_name.take() {
        Some(name) => match Display::connect_to_name(name) {
            Ok(display) => display,
            Err(err) => {
                log::error!(
                    target: log_default_target(),
                    "Failed to connect to a wayland server socket with the given name. Error: {}",
                    err
                );
                std::process::exit(1);
            }
        },
        None => match Display::connect_to_env() {
            Ok(display) => display,
            Err(err) => {
                log::error!(
                    target: log_default_target(),
                    "Failed to connect to a wayland server socket using the contents of the environment variables. Error: {}",
                    err
                );
                std::process::exit(1);
            }
        },
    };
    let mut event_queue = display.create_event_queue();
    let wl_display = display.attach(event_queue.token());
    let clipboard_manager: Rc<RefCell<Option<Main<ZwlrDataControlManagerV1>>>> = Rc::new(RefCell::new(None));
    let (global_manager, seats) = init_global_manager_with_seats(
        &wl_display,
        &clipboard_manager,
        settings.clipboard_type,
        settings.selection_size_limit_bytes,
    );

    // Retrieve the global interfaces, otherwise it is not possible to create a clipboard manager instance
    EventQueueMethod::SyncRoundtrip
        .run(&mut event_queue, &display)
        .unwrap_or_else(|err| panic!("{}", err));

    // Tries to create a clipboard manager instance
    let clipboard_manager_version = if settings.clipboard_type.primary() { 2 } else { 1 };
    let clipboard_manager_instance =
        match global_manager.instantiate_exact::<ZwlrDataControlManagerV1>(clipboard_manager_version) {
            Ok(instance) => instance,
            Err(err) => {
                let mut default = format!(
                    "Failed to get clipboard manager ({} with version {})",
                    ZwlrDataControlManagerV1::NAME,
                    clipboard_manager_version
                );
                if settings.clipboard_type.primary() {
                    default += "\nPerhaps the primary clipboard is not supported by your compositor?";
                }
                log::error!(target: log_default_target(), "{}\nError: {}", default, err);
                std::process::exit(1);
            }
        };

    // If there are currently no seats, stop
    if seats.deref().borrow().is_empty() {
        log::error!(target: log_default_target(), "No seats found. Stopping program...");
        std::process::exit(1);
    }

    // Initialize each currently available seat
    for seat in seats.deref().borrow().values() {
        init_seat(
            seat,
            &clipboard_manager_instance,
            settings.clipboard_type,
            settings.selection_size_limit_bytes,
        );
    }

    // Make the clipboard manager instance available to the global manager handler,
    // so that seats that get later added can be initialized
    *clipboard_manager.deref().borrow_mut() = Some(clipboard_manager_instance);
    let clipboard_manager_ref = clipboard_manager.deref().borrow();
    let clipboard_manager_instance = clipboard_manager_ref.as_ref().unwrap();

    loop {
        // Get new events and inform about new selections
        EventQueueMethod::Dispatch
            .run(&mut event_queue, &display)
            .unwrap_or_else(|err| panic!("{}", err));

        // Handle selection offers
        handle_offer_events(
            &display,
            &mut event_queue,
            &seats,
            clipboard_manager_instance,
            &settings,
        );
    }
}

/// Handles a selection event by trying to copy the selection data
/// and setting a new selection which reads the data from our own program.
/// If this event was triggered by ourselves, we do not set a new selection.
fn handle_offer_events(
    display: &Display,
    event_queue: &mut EventQueue,
    seats: &Rc<RefCell<HashMap<u32, Main<WlSeat>>>>,
    clipboard_manager: &Main<ZwlrDataControlManagerV1>,
    settings: &Settings,
) {
    let mut got_new_pipes = false;

    // First, create some pipes we can read from.
    seats.deref().borrow().values().for_each(|seat| {
        let data_device = get_data_device(seat.as_ref()).borrow();
        let data_device = if let Some(data_device) = data_device.as_ref() {
            data_device
        } else {
            return;
        };

        let seat_id = seat.as_ref().id();
        let mut selections_data = get_selections_data(data_device.as_ref()).borrow_mut();

        for (selection, is_primary_clipboard) in selections_data.iter_mut() {
            selection.pipes = selection.offer.take().and_then(|offer| {
                let result = get_mime_types_with_pipes_from_offer(
                    seat_id,
                    &offer,
                    &selection.fd_from_own_app,
                    settings,
                    is_primary_clipboard,
                );

                offer.destroy();

                if result.is_some() {
                    got_new_pipes = true;
                }

                result
            });
        }
    });

    if !got_new_pipes {
        // In case we have not gotten any new pipes to read,
        // we can just return because there is nothing more to do.
        return;
    }

    // Others programs need to know we want to read some data from the pipes,
    // so we can actually get the clipboard data.
    if let Err(err) = EventQueueMethod::SyncRoundtrip.run(event_queue, display) {
        log::error!(target: log_default_target(), "{}", err);
        return;
    }

    // Then, read the data from the pipes and update the selection if everything is okay.
    seats.deref().borrow().values().for_each(|seat| {
        let data_device = get_data_device(seat.as_ref()).borrow();
        let data_device = if let Some(data_device) = data_device.as_ref() {
            data_device
        } else {
            return;
        };

        let seat_id = seat.as_ref().id();
        let mut selections_data = get_selections_data(data_device.as_ref()).borrow_mut();

        for (selection, is_primary_clipboard) in selections_data.iter_mut() {
            // Check if there are pipes to read, otherwise skip.
            let offer_pipe_data = if let Some(pipe_data) = selection.pipes.take() {
                pipe_data
            } else {
                continue;
            };

            if selection.offer.is_some() {
                // We got a new selection offer during the synchronous roundtrip.
                // So do not overwrite the current selection with old data.
                log::trace!(
                    target: &log_seat_target(seat_id),
                    "{} clipboard got a new selection event, so ignore the old one",
                    get_clipboard_type_str(is_primary_clipboard, true)
                );
                continue;
            }

            // Check if the offer event was triggered by ourselves.
            // If yes, ignore this event and return.
            let is_from_own_app = is_offer_event_from_own_app(&selection.fd_from_own_app);

            if is_from_own_app {
                log::trace!(
                    target: &log_seat_target(seat_id),
                    "{} clipboard selection event was triggered by ourselves, so ignore it",
                    get_clipboard_type_str(is_primary_clipboard, true)
                );
                continue;
            }

            // Read pipes to data.
            let data = match read_pipes_to_mime_types_with_data(
                seat_id,
                offer_pipe_data.pipes,
                settings,
                offer_pipe_data.mime_types_size_bytes,
            ) {
                Some(data) if data.is_empty() => {
                    // If we failed to get the data for at least one mime type,
                    // there is nothing for us to do, so skip.
                    continue;
                }
                Some(data) => data,
                None => {
                    // Some error occurred. There is nothing for us to do, so skip.
                    continue;
                }
            };

            // Create data source from data.
            let data_source = create_data_source(
                seat_id,
                data,
                clipboard_manager,
                &selection.fd_from_own_app,
                settings.write_timeout,
                is_primary_clipboard,
            );

            // Update selection to data source.
            update_selection(seat_id, data_device, data_source, is_primary_clipboard);
        }
    });
}

/// A unique identifier for file descriptors.
/// The device id and inode number should make a
/// file descriptor uniquely identifiable.
#[derive(PartialEq, Eq, Hash)]
struct FdIdentifier {
    /// The device id of the file descriptor
    dev: u64,
    /// The inode number of the file descriptor
    ino: u64,
}

impl<T: Borrow<File>> From<T> for FdIdentifier {
    /// Creates a file descriptor identifier from a file.
    fn from(file: T) -> Self {
        let file_metadata = file.borrow().metadata().expect("Failed to get metadata for file.");
        FdIdentifier {
            dev: file_metadata.dev(),
            ino: file_metadata.ino(),
        }
    }
}

/// Offer parsed into pipes to read from.
struct OfferPipeData {
    /// A list of tuples containing a mime type and a pipe to read from.
    pipes: Vec<(Box<str>, FileDescriptor)>,
    /// The total size of the mime type texts.
    mime_types_size_bytes: u64,
}

/// Returns mime types associated to pipes you can read the data from.
fn get_mime_types_with_pipes_from_offer(
    seat_id: u32,
    offer_event: &ZwlrDataControlOfferV1,
    fd_from_own_app: &Rc<RefCell<HashMap<FdIdentifier, bool>>>,
    settings: &Settings,
    is_primary_clipboard: bool,
) -> Option<OfferPipeData> {
    log::trace!(
        target: &log_seat_target(seat_id),
        "Handle new {} clipboard selection event",
        get_clipboard_type_str(is_primary_clipboard, false)
    );

    // Get all available mime types for that offer
    let offer_data = get_offer_data(offer_event.as_ref()).replace(OfferData {
        mime_types: HashSet::with_capacity(0),
        mime_types_size_bytes: 0,
        exceeded_size_limit: false,
    });

    // Check size limit.
    if offer_data.exceeded_size_limit {
        return None;
    }

    let mime_types = offer_data.mime_types;

    if mime_types.is_empty() {
        // Offer has no mime types, so ignore it
        return None;
    }

    // Log all available mime types.
    for mime_type in &mime_types {
        log::trace!(
            target: &log_seat_target(seat_id),
            "Current selection event offers mime type: {}",
            mime_type,
        );
    }

    if let Some(regex) = settings.all_mime_type_regex.as_ref() {
        // Only keep this offer, if all mime types have
        // a match for this regex.
        let match_all_regex = mime_types.iter().all(|mime_type| {
            match regex.is_match(mime_type) {
                Ok(has_match) => {
                    if !has_match {
                        log::trace!(
                            target: &log_seat_target(seat_id),
                            "Ignoring selection event because this mime type does not have a match for the regex: {}",
                            mime_type,
                        );
                    }
                    has_match
                }
                Err(err) => {
                    log::trace!(
                        target: &log_seat_target(seat_id),
                        "Checking regex returned an error: {}\nThis mime type has been checked: {}",
                        err,
                        mime_type,
                    );
                    // Just assume that the mime type has a match.
                    true
                }
            }
        });

        if !match_all_regex {
            return None;
        }
    }

    // The file descriptors from the last offer are invalid.
    // We only want to check for the file descriptors of this offer,
    // otherwise there might be false positives or false negatives.
    // Therefore, clear the map.
    let mut fd_from_own_app_mut = fd_from_own_app.deref().borrow_mut();
    fd_from_own_app_mut.clear();

    // Create a pipe to read the data for each mime type
    let mime_types_with_pipes: Vec<(Box<str>, FileDescriptor)> = mime_types
        .into_iter()
        .map(|mime_type| {
            let Pipe { read, write } = Pipe::new().expect("Failed to create pipe");

            // Save file descriptor identifier of the writable end of the pipe,
            // so we can check if we are writing to our own pipe.
            let write_file = unsafe { std::fs::File::from_raw_fd(write.into_raw_fd()) };
            let fd_identifier = FdIdentifier::from(&write_file);
            fd_from_own_app_mut.insert(fd_identifier, false);

            // We want to receive the data for this mime type
            offer_event.receive(mime_type.to_string(), write_file.as_raw_fd());

            // Explicitly close the file descriptor of the writable end of the pipe?
            drop(write_file);

            (mime_type, read)
        })
        .collect();

    Some(OfferPipeData {
        pipes: mime_types_with_pipes,
        mime_types_size_bytes: offer_data.mime_types_size_bytes,
    })
}

/// Checks if the offer event was triggered by ourselves.
fn is_offer_event_from_own_app(fd_from_own_app: &Rc<RefCell<HashMap<FdIdentifier, bool>>>) -> bool {
    // We need to check only one value, since either all file descriptors are
    // from our own app (true) or not (false).
    *fd_from_own_app.deref().borrow_mut().values().next().unwrap()
}

/// Read the data from the pipes and return it.
fn read_pipes_to_mime_types_with_data(
    seat_id: u32,
    mime_types_with_pipes: Vec<(Box<str>, FileDescriptor)>,
    settings: &Settings,
    mime_types_size_bytes: u64,
) -> Option<HashMap<Box<str>, Box<[u8]>>> {
    pipe_io::read_with_timeout(
        mime_types_with_pipes,
        settings.read_timeout,
        settings.ignore_selection_event_on_timeout,
        settings.selection_size_limit_bytes,
        mime_types_size_bytes,
    )
    .map_err(|err| {
        log::warn!(
            target: &log_seat_target(seat_id),
            "{}\nIgnoring selection event...",
            err
        );
    })
    .ok()?
    .into_iter()
    .filter_map(|(mime_type, data)| match data {
        Ok(data) => Some((mime_type, data)),
        Err(err) => {
            log::warn!(
                target: &log_seat_target(seat_id),
                "{}\nIgnoring mime type: {}",
                err,
                mime_type
            );
            None
        }
    })
    .collect::<HashMap<_, _>>()
    .into()
}

/// Creates a data source from the according clipboard contents.
fn create_data_source(
    seat_id: u32,
    mime_types_to_data: HashMap<Box<str>, Box<[u8]>>,
    clipboard_manager: &Main<ZwlrDataControlManagerV1>,
    fd_from_own_app: &Rc<RefCell<HashMap<FdIdentifier, bool>>>,
    write_timout: Duration,
    is_primary_clipboard: bool,
) -> Main<ZwlrDataControlSourceV1> {
    let clipboard_type_str_title = get_clipboard_type_str(is_primary_clipboard, true);
    let clipboard_type_str_lower = get_clipboard_type_str(is_primary_clipboard, false);

    // Create our own data source
    let data_source = clipboard_manager.create_data_source();

    // These are the mime types we offer
    for mime_type in mime_types_to_data.keys() {
        data_source.offer(mime_type.to_string());
    }

    let mime_types_to_data = Arc::new(mime_types_to_data);
    let fd_from_own_app2 = fd_from_own_app.clone();

    data_source.quick_assign(move |data_source, source_event, _| {
        match source_event {
            // Request for data from the client.
            // Send the data as the specified mime type over the passed file descriptor, then close it.
            SourceEvent::Send { mime_type, fd } => {
                log::trace!(
                    target: &log_seat_target(seat_id),
                    "{} clipboard data source {} received new request for mime type: {}",
                    clipboard_type_str_title,
                    data_source.as_ref().id(),
                    mime_type
                );

                // Check if the file descriptor comes from our own app
                let fd_file = unsafe { File::from_raw_fd(fd) };
                let fd_identifier = FdIdentifier::from(&fd_file);

                if let Some(is_from_own_app) = fd_from_own_app2.deref().borrow_mut().get_mut(&fd_identifier) {
                    // File descriptor is from our own app, so we update the information
                    *is_from_own_app = true;
                    // Explicitly close file descriptor
                    drop(fd_file);
                    // Also we do not send any data, because we do not want to read it anyway.
                    // Therefore return.
                    return;
                }

                let mime_type_boxed = mime_type.into_boxed_str();

                if !mime_types_to_data.contains_key(&mime_type_boxed) {
                    // Mime type not available, so return
                    drop(fd_file); // Explicitly close file descriptor
                    return;
                }

                // Write the data to the pipe in a new thread,
                // so we do not block the main thread.
                let fd = fd_file.into_raw_fd();
                let mime_types_to_data2 = mime_types_to_data.clone();

                std::thread::spawn(move || {
                    let data = mime_types_to_data2.get(&mime_type_boxed).unwrap();
                    let write = unsafe { FileDescriptor::from_raw_fd(fd) };

                    if let Err(err) = pipe_io::write_with_timeout(write, data, write_timout) {
                        log::warn!(
                            target: &log_seat_target(seat_id),
                            "{}\nFailed to send {} clipboard data for mime type: {}",
                            err,
                            clipboard_type_str_lower,
                            mime_type_boxed
                        );
                    }
                });
            }
            // This data source is no longer valid.
            // The data source has been replaced by another data source.
            // The client should clean up and destroy this data source.
            SourceEvent::Cancelled => {
                let data_source_id = data_source.as_ref().id();
                // Destroy the current data source.
                data_source.destroy();
                log::trace!(
                    target: &log_seat_target(seat_id),
                    "Destroyed {} clipboard data source {}",
                    clipboard_type_str_lower,
                    data_source_id
                );
            }
            _ => {}
        }
    });

    data_source
}

/// Set selection to our data source.
fn update_selection(
    seat_id: u32,
    data_device: &Main<ZwlrDataControlDeviceV1>,
    data_source: Main<ZwlrDataControlSourceV1>,
    is_primary_clipboard: bool,
) {
    if is_primary_clipboard {
        data_device.set_primary_selection(Some(&data_source));
    } else {
        data_device.set_selection(Some(&data_source));
    }

    log::trace!(
        target: &log_seat_target(seat_id),
        "Created {} clipboard data source {}",
        get_clipboard_type_str(is_primary_clipboard, false),
        data_source.as_ref().id(),
    );
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
enum ClipboardType {
    Regular,
    Primary,
    Both,
}

impl ClipboardType {
    const fn primary(&self) -> bool {
        match self {
            Self::Primary | Self::Both => true,
            Self::Regular => false,
        }
    }

    const fn regular(&self) -> bool {
        match self {
            Self::Regular | Self::Both => true,
            Self::Primary => false,
        }
    }
}

enum EventQueueMethod {
    SyncRoundtrip,
    Dispatch,
}

impl EventQueueMethod {
    /// Runs the given event queue method.
    fn run(&self, event_queue: &mut EventQueue, display: &Display) -> Result<(), String> {
        let result = match self {
            EventQueueMethod::SyncRoundtrip => event_queue.sync_roundtrip(&mut (), |_, _, _| {}),
            EventQueueMethod::Dispatch => event_queue.dispatch(&mut (), |_, _, _| {}),
        };

        if let Err(err) = result {
            return Err(format!("{}. Error: {}", self.get_event_queue_error(display), err));
        }

        Ok(())
    }

    /// Constructs an error message for cases where the event queue has failed.
    fn get_event_queue_error(&self, display: &Display) -> String {
        let mut default = String::from("Event Queue: failed ");

        match self {
            EventQueueMethod::SyncRoundtrip => default += "synchronous roundtrip",
            EventQueueMethod::Dispatch => default += "dispatch",
        }

        if let Some(protocol_error) = display.protocol_error() {
            let _ = write!(default, ". Last Protocol Error: {:?}", protocol_error);
        }

        default
    }
}

/// Returns a formatted target for logging purposes.
fn log_default_target() -> &'static str {
    crate_name!()
}

/// Returns a formatted target for logging purposes.
fn log_seat_target(seat_id: u32) -> String {
    format!("{} Seat {}", crate_name!(), seat_id)
}

/// If title_case is false, lower case is used instead of title case.
fn get_clipboard_type_str(is_primary_clipboard: bool, title_case: bool) -> &'static str {
    match (is_primary_clipboard, title_case) {
        (true, true) => "Primary",
        (true, false) => "primary",
        (false, true) => "Regular",
        (false, false) => "regular",
    }
}
