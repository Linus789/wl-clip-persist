use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::ops::Deref;
use std::os::unix::prelude::{AsRawFd, FromRawFd, IntoRawFd, MetadataExt};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::builder::NonEmptyStringValueParser;
use clap::{arg, crate_description, crate_name, crate_version, value_parser, Command, ValueEnum};
use filedescriptor::{FileDescriptor, Pipe};
use wayland_client::protocol::wl_display::WlDisplay;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{
    Attached, DispatchData, Display, EventQueue, GlobalEvent, GlobalImplementor, GlobalManager, Interface, Main,
};
use wayland_protocols::wlr::unstable::data_control::v1::client::zwlr_data_control_device_v1::{
    Event as DeviceEvent, ZwlrDataControlDeviceV1,
};
use wayland_protocols::wlr::unstable::data_control::v1::client::zwlr_data_control_manager_v1::ZwlrDataControlManagerV1;
use wayland_protocols::wlr::unstable::data_control::v1::client::zwlr_data_control_offer_v1::{
    Event as OfferEvent, ZwlrDataControlOfferV1,
};
use wayland_protocols::wlr::unstable::data_control::v1::client::zwlr_data_control_source_v1::Event as SourceEvent;

/// Timeout for trying to get the current clipboard.
const READ_TIMEOUT: Duration = Duration::from_millis(500);

// Timeout for trying to send the current clipboard to other programs.
const WRITE_TIMEOUT: Duration = Duration::from_millis(3000);

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
        .get_matches();

    let clipboard_type = *matches.get_one::<ClipboardType>("clipboard").unwrap();
    let display_name: Option<OsString> = matches.get_one::<String>("display").map(|s| s.into());

    // Run main program
    handle_clipboard(clipboard_type, display_name);
}

/// Currently available selection offers, if any.
struct AvailableOffers {
    primary_selection_offer: Option<ZwlrDataControlOfferV1>,
    regular_selection_offer: Option<ZwlrDataControlOfferV1>,
}

/// Create a GlobalManager that adds new seats to the list
/// and removes seats that should be removed.
/// Returns the global manager and a list of always-up-to-date seats.
fn init_global_manager_with_seats(
    display: &Attached<WlDisplay>,
    clipboard_manager: &Rc<RefCell<Option<Main<ZwlrDataControlManagerV1>>>>,
    clipboard_type: ClipboardType,
) -> (GlobalManager, Rc<RefCell<HashMap<u32, Main<WlSeat>>>>) {
    let seats = Rc::new(RefCell::new(HashMap::<u32, Main<WlSeat>>::new()));
    let seats2 = seats.clone();
    let clipboard_manager2 = clipboard_manager.clone();
    let global_manager = GlobalManager::new_with_cb(display, move |global_event, registry, dispatch_data| {
        const SEAT_INTERFACE_NAME: &str = WlSeat::NAME;
        const SEAT_MIN_VERSION: u32 = 2;

        let mut global_implementor = |seat: Main<WlSeat>, _: DispatchData| {
            // Seems to be a seat that got added later, so initialize it
            if let Some(clipboard_manager_instance) = clipboard_manager2.deref().borrow().as_ref() {
                init_seat(&seat, clipboard_manager_instance, clipboard_type);
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
                        log::trace!(target: &seat_target(id), "Removed seat");
                    }
                }
            }
        }
    });
    (global_manager, seats)
}

/// Initializes a seat by assigning a handler for the selection offer events
/// and creating all the necessary infrastructure around it.
fn init_seat(seat: &Main<WlSeat>, clipboard_manager: &Main<ZwlrDataControlManagerV1>, clipboard_type: ClipboardType) {
    // Create a data device that can be used to manage a seat's selection.
    let data_device: Rc<RefCell<Option<Main<ZwlrDataControlDeviceV1>>>> =
        Rc::new(RefCell::new(Some(clipboard_manager.get_data_device(seat))));

    {
        let data_device_ref = data_device.deref().borrow();
        let data_device_instance = data_device_ref.as_ref().unwrap();

        // Put available offers in the user data of the data device
        let last_offers: RefCell<AvailableOffers> = RefCell::new(AvailableOffers {
            primary_selection_offer: None,
            regular_selection_offer: None,
        });
        data_device_instance.as_ref().user_data().set(move || last_offers);

        // Assign handler for selection offer events
        let data_device2 = data_device.clone();
        data_device_instance.quick_assign(move |data_device, event, _| match event {
            // This event is triggered immediately before
            // DeviceEvent::PrimarySelection or DeviceEvent::Selection
            // and is used to describe the mime type that are offered
            DeviceEvent::DataOffer { id } => {
                // Create HashSet where all available mime types are saved
                id.as_ref()
                    .user_data()
                    .set(move || RefCell::new(HashSet::<String>::with_capacity(32)));

                // Save mime type in that HashSet for each mime type we are notified about
                id.quick_assign(|offer, event, _| {
                    if let OfferEvent::Offer { mime_type } = event {
                        offer
                            .as_ref()
                            .user_data()
                            .get::<RefCell<HashSet<String>>>()
                            .unwrap()
                            .borrow_mut()
                            .insert(mime_type);
                    }
                })
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
                let mut available_offers = data_device
                    .as_ref()
                    .user_data()
                    .get::<RefCell<AvailableOffers>>()
                    .unwrap()
                    .borrow_mut();

                // If there is and old offer in the available offers, destroy it
                if let Some(old_offer) = available_offers.primary_selection_offer.take() {
                    old_offer.destroy();
                }

                // Update primary selection offer
                available_offers.primary_selection_offer = id;
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

                let mut available_offers = data_device
                    .as_ref()
                    .user_data()
                    .get::<RefCell<AvailableOffers>>()
                    .unwrap()
                    .borrow_mut();

                // If there is and old offer in the available offers, destroy it
                if let Some(old_offer) = available_offers.regular_selection_offer.take() {
                    old_offer.destroy();
                }

                // Update regular selection offer
                available_offers.regular_selection_offer = id;
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
    log::trace!(target: &seat_target(seat.as_ref().id()), "Initialized seat");
}

/// Makes the selections for the given clipboard persistent.
fn handle_clipboard(clipboard_type: ClipboardType, display_name: Option<OsString>) {
    // Tries to connect to a wayland server socket by either the given name
    // or if none given by using the environment variables
    let display = match display_name {
        Some(name) => match Display::connect_to_name(name) {
            Ok(display) => display,
            Err(err) => {
                log::error!(
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
    let (global_manager, seats) = init_global_manager_with_seats(&wl_display, &clipboard_manager, clipboard_type);

    // Retrieve the global interfaces, otherwise it is not possible to create a clipboard manager instance
    EventQueueMethod::SyncRoundtrip
        .run(&mut event_queue, &display)
        .unwrap_or_else(|err| panic!("{}", err));

    // Tries to create a clipboard manager instance
    let clipboard_manager_version = if clipboard_type.primary() { 2 } else { 1 };
    let clipboard_manager_instance =
        match global_manager.instantiate_exact::<ZwlrDataControlManagerV1>(clipboard_manager_version) {
            Ok(instance) => instance,
            Err(err) => {
                let mut default = format!(
                    "Failed to get clipboard manager ({} with version {})",
                    ZwlrDataControlManagerV1::NAME,
                    clipboard_manager_version
                );
                if clipboard_type.primary() {
                    default += "\nPerhaps the primary clipboard is not supported by your compositor?";
                }
                log::error!("{}\nError: {}", default, err);
                std::process::exit(1);
            }
        };

    // If there are currently no seats, stop
    if seats.deref().borrow().is_empty() {
        log::error!("No seats found. Stopping program...");
        std::process::exit(1);
    }

    // Initialize each currently available seat
    for seat in seats.deref().borrow().values() {
        init_seat(seat, &clipboard_manager_instance, clipboard_type);
    }

    // Make the clipboard manager instance available to the global manager handler,
    // so that seats that get later added can be initialized
    *clipboard_manager.deref().borrow_mut() = Some(clipboard_manager_instance);

    // A map that keeps track of file descriptors that are the writable end of the pipe.
    // The boolean value expresses whether the file descriptor arrived at our
    // own program to send some text to it. This way we can identify selection offer events
    // that got triggered by ourselves.
    let fd_from_own_app: Rc<RefCell<HashMap<FdIdentifier, bool>>> = Rc::new(RefCell::new(HashMap::new()));

    loop {
        // Get new events and inform about new selections
        EventQueueMethod::Dispatch
            .run(&mut event_queue, &display)
            .unwrap_or_else(|err| panic!("{}", err));

        // Handle selection offer for each seat
        for seat in seats.deref().borrow().values() {
            // Get data device if any (since it might have gotten destroyed)
            if let Some(data_device) = seat
                .as_ref()
                .user_data()
                .get::<Rc<RefCell<Option<Main<ZwlrDataControlDeviceV1>>>>>()
                .unwrap()
                .borrow()
                .as_ref()
            {
                // Get available offers and clipboard manager
                let mut available_offers = data_device
                    .as_ref()
                    .user_data()
                    .get::<RefCell<AvailableOffers>>()
                    .unwrap()
                    .borrow_mut();
                let clipboard_manager_ref = clipboard_manager.deref().borrow();
                let clipboard_manager_instance = clipboard_manager_ref.as_ref().unwrap();

                // If there is a primary selection offer, take it so it is only processed once
                if let Some(event) = available_offers.primary_selection_offer.take() {
                    handle_selection_event(
                        seat.as_ref().id(),
                        &display,
                        data_device,
                        &mut event_queue,
                        clipboard_manager_instance,
                        &event,
                        &fd_from_own_app,
                        true,
                    );

                    // Finally destroy selection offer
                    event.destroy();
                }

                // If there is a regular selection offer, take it so it is only processed once
                if let Some(event) = available_offers.regular_selection_offer.take() {
                    handle_selection_event(
                        seat.as_ref().id(),
                        &display,
                        data_device,
                        &mut event_queue,
                        clipboard_manager_instance,
                        &event,
                        &fd_from_own_app,
                        false,
                    );

                    // Finally destroy selection offer
                    event.destroy();
                }
            }
        }
    }
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

/// Handles a selection event by trying to copy the selection data
/// and setting a new selection which reads the data from our own program.
/// If this event was triggered by ourselves, we do not set a new selection.
fn handle_selection_event(
    seat_id: u32,
    display: &Display,
    data_device: &Main<ZwlrDataControlDeviceV1>,
    event_queue: &mut EventQueue,
    clipboard_manager: &Main<ZwlrDataControlManagerV1>,
    offer_event: &ZwlrDataControlOfferV1,
    fd_from_own_app: &Rc<RefCell<HashMap<FdIdentifier, bool>>>,
    primary_clipboard: bool,
) {
    let clipboard_type_str_start = if primary_clipboard { "Primary" } else { "Regular" };
    let clipboard_type_str_lower = if primary_clipboard { "primary" } else { "regular" };
    log::trace!(
        target: &seat_target(seat_id),
        "Handle new {} clipboard selection event",
        clipboard_type_str_lower
    );

    // Get all available mime types for that offer
    let mime_types = offer_event
        .as_ref()
        .user_data()
        .get::<RefCell<HashSet<String>>>()
        .unwrap()
        .take();

    if mime_types.is_empty() {
        // Offer has no mime types, so ignore it
        return;
    }

    // The file descriptors from the last offer are invalid.
    // We only want to check for the file descriptors of this offer,
    // otherwise there might be false positives or false negatives.
    // Therefore, clear the map.
    let mut fd_from_own_app_mut = fd_from_own_app.deref().borrow_mut();
    fd_from_own_app_mut.clear();

    // Create a pipe to read the data for each mime type
    let mime_types_with_pipes: Vec<(String, FileDescriptor)> = mime_types
        .into_iter()
        .map(|mime_type| {
            let Pipe { read, write } = Pipe::new().expect("Failed to create pipe");

            // Save file descriptor identifier of the writable end of the pipe,
            // so we can check if we are writing to our own pipe.
            let write_file = unsafe { std::fs::File::from_raw_fd(write.into_raw_fd()) };
            let write_file_metadata = write_file
                .metadata()
                .expect("Failed to get metadata of the writable end of the pipe");
            let fd_identifier = FdIdentifier {
                dev: write_file_metadata.dev(),
                ino: write_file_metadata.ino(),
            };
            fd_from_own_app_mut.insert(fd_identifier, false);

            // We want to receive the data for this mime type
            offer_event.receive(mime_type.to_string(), write_file.as_raw_fd());

            // Explicitly close the file descriptor of the writable end of the pipe?
            drop(write_file);

            (mime_type, read)
        })
        .collect();

    // Drop mutable borrow, because we need to borrow it mutably in the data source handler
    // if it is called when the synchronous roundtrip is run.
    drop(fd_from_own_app_mut);

    // Others programs need to know we want to read some data,
    // so we can actually get the clipboard data.
    if let Err(err) = EventQueueMethod::SyncRoundtrip.run(event_queue, display) {
        log::error!("{}", err);
        return;
    }

    // Check if the offer event was triggered by ourselves.
    // If yes, ignore this event and return.
    // We need to check only one value, since either all file descriptors are
    // from our own app (true) or not (false).
    let is_from_own_app = *fd_from_own_app.deref().borrow_mut().values().next().unwrap();

    if is_from_own_app {
        log::trace!(
            target: &seat_target(seat_id),
            "{} clipboard selection event was triggered by ourselves, so ignore it",
            clipboard_type_str_start
        );
        return;
    }

    // And finally read the data from the pipes...
    let mime_types_to_data: HashMap<String, Box<[u8]>> = read_with_timeout(mime_types_with_pipes, READ_TIMEOUT)
        .into_iter()
        .filter_map(|(mime_type, data)| match data {
            Ok(data) => Some((mime_type, data)),
            Err(err) => {
                log::warn!(
                    target: &seat_target(seat_id),
                    "{}\nIgnoring mime type: {}",
                    err,
                    mime_type
                );
                None
            }
        })
        .collect();

    // If we failed to get the data for at least one mime type,
    // there is nothing for us to do, so return.
    if mime_types_to_data.is_empty() {
        return;
    }

    // Create our own data source
    let data_source = clipboard_manager.create_data_source();

    // These are the mime types we offer
    for mime_type in mime_types_to_data.keys() {
        data_source.offer(mime_type.clone());
    }

    let mime_types_to_data = Arc::new(mime_types_to_data);
    let fd_from_own_app2 = fd_from_own_app.clone();

    data_source.quick_assign(move |data_source, source_event, _| {
        match source_event {
            // Request for data from the client.
            // Send the data as the specified mime type over the passed file descriptor, then close it.
            SourceEvent::Send { mime_type, fd } => {
                log::trace!(
                    target: &seat_target(seat_id),
                    "{} clipboard data source {} received new request for mime type: {}",
                    clipboard_type_str_start,
                    data_source.as_ref().id(),
                    mime_type
                );

                // Check if the file descriptor comes from our own app
                let fd_file = unsafe { File::from_raw_fd(fd) };
                let fd_file_metadata = fd_file
                    .metadata()
                    .expect("Failed to get metadata of the data source pipe");
                let fd_identifier = FdIdentifier {
                    dev: fd_file_metadata.dev(),
                    ino: fd_file_metadata.ino(),
                };

                if let Some(is_from_own_app) = fd_from_own_app2.deref().borrow_mut().get_mut(&fd_identifier) {
                    // File descriptor is from our own app, so we update the information
                    *is_from_own_app = true;
                    // Explicitly close file descriptor
                    drop(fd_file);
                    // Also we do not send any data, because we do not want to read it anyway.
                    // Therefore return.
                    return;
                }

                if !mime_types_to_data.contains_key(&mime_type) {
                    // Mime type not available, so return
                    drop(fd_file); // Explicitly close file descriptor
                    return;
                }

                // Write the data to the pipe in a new thread,
                // so we do not block the main thread.
                let fd = fd_file.into_raw_fd();
                let mime_types_to_data2 = mime_types_to_data.clone();

                std::thread::spawn(move || {
                    let data = mime_types_to_data2.get(&mime_type).unwrap();
                    let write = unsafe { FileDescriptor::from_raw_fd(fd) };

                    if let Err(err) = write_with_timeout(write, data, WRITE_TIMEOUT) {
                        log::warn!(
                            target: &seat_target(seat_id),
                            "{}\nFailed to send {} clipboard data for mime type: {}",
                            err,
                            clipboard_type_str_lower,
                            mime_type
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
                    target: &seat_target(seat_id),
                    "Destroyed {} clipboard data source {}",
                    clipboard_type_str_lower,
                    data_source_id
                );
            }
            _ => {}
        }
    });

    // Set selection to our data source
    if primary_clipboard {
        data_device.set_primary_selection(Some(&data_source));
    } else {
        data_device.set_selection(Some(&data_source));
    }

    log::trace!(
        target: &seat_target(seat_id),
        "Created {} clipboard data source {}",
        clipboard_type_str_lower,
        data_source.as_ref().id(),
    );
}

/// Reads the entire data from a file descriptor with timeout.
fn read_with_timeout(
    mime_types_with_files: Vec<(String, FileDescriptor)>,
    timeout: Duration,
) -> HashMap<String, Result<Box<[u8]>, Cow<'static, str>>> {
    // The return result. Maps an available mime type to either the read data or an error.
    let mut mime_type_to_data = HashMap::with_capacity(mime_types_with_files.len());

    // For each raw fd, we associate this data with it.
    struct FdData {
        mime_type: String,
        file: FileDescriptor,
        data: Vec<u8>,
    }

    let mut remaining_fd_to_data: HashMap<i32, FdData> = mime_types_with_files
        .into_iter()
        .filter_map(|(mime_type, mut file)| {
            if let Err(err) = file.set_non_blocking(true) {
                mime_type_to_data.insert(
                    mime_type,
                    Err(format!(
                        "Failed to enable the non-blocking IO mode of the file descriptor. Error: {}",
                        err
                    )
                    .into()),
                );
                return None;
            }

            let raw_fd = file.as_raw_fd();
            let fd_data = FdData {
                mime_type,
                file,
                data: Vec::with_capacity(32),
            };

            Some((raw_fd, fd_data))
        })
        .collect();

    // Create a list of remaining file descriptors to poll.
    let mut remaining_pfds: Vec<libc::pollfd> = remaining_fd_to_data
        .keys()
        .map(|&raw_fd| libc::pollfd {
            fd: raw_fd,
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();

    let mut buf = [0u8; 8192];
    let end_time = Instant::now() + timeout;

    // As long as there are still some remaining mime types...
    'outer: while !remaining_pfds.is_empty() {
        // Check if time has run out. If so, error out the remaining mime types.
        let remaining_time = end_time
            .saturating_duration_since(Instant::now())
            .as_millis()
            .try_into()
            .unwrap();

        if remaining_time == 0 {
            for FdData { mime_type, .. } in remaining_fd_to_data.into_values() {
                mime_type_to_data.insert(mime_type, Err("Timed out reading from file descriptor".into()));
            }
            break 'outer;
        }

        // Wait for changes of readability with timeout.
        match unsafe { libc::poll(remaining_pfds.as_mut_ptr(), remaining_pfds.len() as u64, remaining_time) } {
            0 => {
                // Timeout occurred, error out the remaining mime types.
                for FdData { mime_type, .. } in remaining_fd_to_data.into_values() {
                    mime_type_to_data.insert(mime_type, Err("Timed out reading from file descriptor".into()));
                }
                break 'outer;
            }
            -1 => {
                // Some other error occurred, error out the remaining mime types.
                let errno = std::io::Error::last_os_error();
                for FdData { mime_type, .. } in remaining_fd_to_data.into_values() {
                    mime_type_to_data.insert(
                        mime_type,
                        Err(format!("Error while polling to read from the file descriptor: {}", errno).into()),
                    );
                }
                break 'outer;
            }
            1.. => {
                // Readability might have changed, checking...
                remaining_pfds.retain_mut(|pfd| {
                    if pfd.revents != 0 {
                        // This file descriptor might have become readable.
                        match remaining_fd_to_data.entry(pfd.fd) {
                            Entry::Occupied(mut entry) => {
                                let FdData { file, data, .. } = entry.get_mut();

                                // Therefore read some data from the file descriptor.
                                'read: loop {
                                    match file.read(&mut buf) {
                                        Ok(size) if size == 0 => {
                                            // We reached the EOF of the reader.
                                            // Remove entry from data map and insert data to result.
                                            let removed_entry = entry.remove_entry();
                                            let owned_mime_type = removed_entry.1.mime_type;
                                            let data = removed_entry.1.data.into_boxed_slice();
                                            mime_type_to_data.insert(owned_mime_type, Ok(data));
                                            // Also remove this pfd from the remaining pfds.
                                            return false;
                                        }
                                        Ok(size) => {
                                            // Got some new data to read.
                                            data.extend_from_slice(&buf[..size]);
                                            break 'read;
                                        }
                                        Err(err) if err.kind() == ErrorKind::Interrupted => {
                                            // Got interrupted, therefore retry read.
                                            continue 'read;
                                        }
                                        Err(err) => {
                                            // Some error occurred.
                                            // Remove entry from data map and insert error to result.
                                            let removed_entry = entry.remove_entry();
                                            let owned_mime_type = removed_entry.1.mime_type;
                                            mime_type_to_data.insert(
                                                owned_mime_type,
                                                Err(format!(
                                                    "Error while reading from file descriptor: {}",
                                                    err
                                                ).into()),
                                            );
                                            // Also remove this pfd from the remaining pfds.
                                            return false;
                                        }
                                    }
                                }
                            }
                            Entry::Vacant(_) => unreachable!(),
                        }

                        // Make the pollfd re-usable for the next poll call
                        pfd.revents = 0;
                    }

                    // Keep this pfd, since we still have not read everything.
                    true
                });
            }
            return_value => {
                // Invalid return value, error out the remaining mime types.
                for FdData { mime_type, .. } in remaining_fd_to_data.into_values() {
                    mime_type_to_data.insert(
                        mime_type,
                        Err(format!(
                            "Received invalid return value {} from polling to read from the file descriptor",
                            return_value
                        )
                        .into()),
                    );
                }
                break 'outer;
            }
        }
    }

    mime_type_to_data
}

/// Writes the entire data to a file descriptor with timeout.
fn write_with_timeout(mut file: FileDescriptor, mut data: &[u8], timeout: Duration) -> Result<(), Cow<'static, str>> {
    if let Err(err) = file.set_non_blocking(true) {
        return Err(format!(
            "Failed to enable the non-blocking IO mode of the file descriptor. Error: {}",
            err
        )
        .into());
    }

    let mut pfd = libc::pollfd {
        fd: file.as_raw_fd(),
        events: libc::POLLOUT,
        revents: 0,
    };

    let end_time = Instant::now() + timeout;

    while !data.is_empty() {
        let remaining_time = end_time
            .saturating_duration_since(Instant::now())
            .as_millis()
            .try_into()
            .unwrap();

        if remaining_time == 0 {
            return Err("Timed out writing to file descriptor".into());
        }

        match unsafe { libc::poll(&mut pfd, 1, remaining_time) } {
            0 => {
                return Err("Timed out writing to file descriptor".into());
            }
            -1 => {
                let errno = std::io::Error::last_os_error();
                return Err(format!("Error while polling to write to the file descriptor: {}", errno).into());
            }
            1.. => 'write: loop {
                match file.write(data) {
                    Ok(size) if size == 0 => {
                        return Err("Failed to write whole buffer to the file descriptor".into());
                    }
                    Ok(size) => {
                        data = &data[size..];
                        break 'write;
                    }
                    Err(err) if err.kind() == ErrorKind::Interrupted => {
                        continue 'write;
                    }
                    Err(err) => {
                        return Err(format!("Error while writing to file descriptor: {}", err).into());
                    }
                }
            },
            return_value => {
                return Err(format!(
                    "Received invalid return value {} from polling to write to the file descriptor",
                    return_value
                )
                .into());
            }
        }
    }

    Ok(())
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
    // Runs the given event queue method.
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

fn seat_target(seat_id: u32) -> String {
    format!("Seat {}", seat_id)
}
