use std::cell::RefCell;
use std::collections::HashMap;
use std::convert::Infallible;
use std::ffi::CStr;
use std::fs::File;
use std::ops::Deref;
use std::os::fd::IntoRawFd;
use std::os::unix::io::FromRawFd;
use std::rc::Rc;
use std::sync::Arc;

use futures_util::stream::FuturesUnordered;
use futures_util::StreamExt as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use wayrs_client::global::*;
use wayrs_client::object::ObjectId;
use wayrs_client::protocol::*;
use wayrs_client::proxy::Proxy;
use wayrs_client::{Connection, EventCtx};
use wayrs_protocols::wlr_data_control_unstable_v1::*;

use crate::logger::{log_default_target, log_seat_target};
use crate::settings::Settings;
use crate::states::*;

/// Runs the wayland client until a wayland error occurs.
pub(crate) async fn run(settings: Settings, is_reconnect: bool) -> Result<Infallible, WaylandError> {
    let (mut connection, globals) = Connection::async_connect_and_collect_globals()
        .await
        .map_err(WaylandError::ConnectError)?;
    connection.add_registry_cb(wl_registry_cb);

    if is_reconnect {
        log::info!(target: log_default_target(), "Connection to wayland server re-established");
    } else {
        log::trace!(target: log_default_target(), "Connection to wayland server established");
    }

    let data_control_manager_result = if settings.clipboard_type.primary() {
        globals.bind(&mut connection, 2)
    } else {
        globals.bind(&mut connection, 1..=2)
    };
    let data_control_manager = match data_control_manager_result {
        Ok(data_control_manager) => data_control_manager,
        Err(err) => {
            let mut default = "Failed to get clipboard manager (ZwlrDataControlManagerV1)".to_string();

            if settings.clipboard_type.primary() && matches!(err, BindError::UnsupportedVersion { actual: 1, min: _ }) {
                default += "\nPerhaps the primary clipboard is not supported by your compositor?";
            }

            log::error!(target: log_default_target(), "{}\nError: {}", default, err);
            std::process::exit(1);
        }
    };

    let seats = globals
        .iter()
        .filter(|global| global.is::<WlSeat>())
        .filter_map(|seat_global| {
            Seat::bind(&mut connection, data_control_manager, seat_global, &settings)
                .inspect(|_| {
                    log::trace!(
                        target: &log_seat_target(seat_global.name),
                        "Added seat"
                    );
                })
                .inspect_err(|err| {
                    log::debug!(
                        target: &log_seat_target(seat_global.name),
                        "Failed to bind seat: {}",
                        err,
                    )
                })
                .ok()
                .map(|seat| (seat_global.name, seat))
        })
        .collect::<HashMap<_, _>>();

    if seats.is_empty() {
        log::warn!(target: log_default_target(), "No seats found on startup");
    }

    let mut state = State {
        settings,
        data_control_manager,
        seats,
    };

    // Advertise the bindings to the wayland server
    connection.async_flush().await.map_err(WaylandError::IoError)?;

    loop {
        let received_wayland_events = 'wait: loop {
            // We use FuturesUnordered for this because we want to flush the
            // new clipboard source after each finished read.
            let mut set_clipboard_futures = FuturesUnordered::new();
            for seat in state.seats.values_mut() {
                let seat_name = seat.seat_name;
                let data_control_device = seat.data_control_device;

                for (selection_type, selection_state) in seat.selections_iter_mut() {
                    let Some(selection_state) = selection_state else {
                        continue;
                    };

                    let SeatSelectionState::GotPipes {
                        pipes: _,
                        bytes_read: _,
                    } = selection_state
                    else {
                        continue;
                    };

                    set_clipboard_futures.push(handle_pipes_selection_state(
                        seat_name,
                        data_control_device,
                        selection_type,
                        selection_state,
                        state.settings.selection_size_limit_bytes,
                        state.settings.ignore_selection_event_on_error,
                    ));
                }
            }

            tokio::select! {
                biased;

                recv_events = connection.async_recv_events() => {
                    recv_events.map_err(WaylandError::IoError)?;
                    drop(set_clipboard_futures);
                    break 'wait true;
                }
                mime_types_with_data = set_clipboard_futures.next(), if !set_clipboard_futures.is_empty() => {
                    drop(set_clipboard_futures);

                    match mime_types_with_data {
                        Some(Ok(mime_types_with_data)) => {
                            let data = Arc::new(mime_types_with_data.data);
                            let mime_type_data = Arc::clone(&data);
                            let source = data_control_manager.create_data_source_with_cb(&mut connection, move |event_context| {
                                data_source_cb(mime_types_with_data.seat_name, mime_types_with_data.selection_type, event_context, &data);
                            });
                            for mime_type in mime_type_data.keys() {
                                source.offer(&mut connection, mime_type.clone().into_c_string());
                            }
                            let source_id = source.id().as_u32();

                            match mime_types_with_data.selection_type {
                                SelectionType::Regular => mime_types_with_data.data_control_device.set_selection(&mut connection, Some(source)),
                                SelectionType::Primary => mime_types_with_data.data_control_device.set_primary_selection(&mut connection, Some(source)),
                            }

                            log::trace!(
                                target: &log_seat_target(mime_types_with_data.seat_name),
                                "Created {} clipboard data source {}",
                                mime_types_with_data.selection_type.get_clipboard_type_str(false),
                                source_id,
                            );

                            mime_types_with_data.selection_state.reset(&mut connection);
                            break 'wait false;
                        }
                        Some(Err(selection_state)) => {
                            // Selection event got ignored
                            selection_state.reset(&mut connection);
                        }
                        None => unreachable!(),
                    }
                }
            }
        };

        if received_wayland_events {
            connection.dispatch_events(&mut state);
            handle_new_selection_state(&mut connection, &mut state)
                .await
                .map_err(WaylandError::IoError)?;
        }

        connection.async_flush().await.map_err(WaylandError::IoError)?;
    }
}

/// Handles the registration of globals, in this case seats.
fn wl_registry_cb(connection: &mut Connection<State>, state: &mut State, event: &wl_registry::Event) {
    match event {
        wl_registry::Event::Global(global) if global.is::<WlSeat>() => {
            if let Ok(seat) = Seat::bind(connection, state.data_control_manager, global, &state.settings) {
                if let Some(old_seat) = state.seats.insert(global.name, seat) {
                    old_seat.destroy(connection);
                    log::error!(
                        target: &log_seat_target(global.name),
                        "Added seat, even though seat with same id was already present"
                    );
                } else {
                    log::trace!(
                        target: &log_seat_target(global.name),
                        "Added seat"
                    );
                }
            }
        }
        wl_registry::Event::Global(_) => {
            // Ignore all other globals that are advertised, like WlOutput
        }
        wl_registry::Event::GlobalRemove(name) => {
            if let Some(seat) = state.seats.remove(name) {
                seat.destroy(connection);
                log::trace!(target: &log_seat_target(*name), "Removed seat");
            }
        }
        fallback => {
            log::debug!(target: log_default_target(), "wl_registry::Event: unhandled event: {:?}", fallback);
        }
    }
}

/// Handles the selection events of a seat.
pub(crate) fn data_control_device_cb(seat_name: u32, event_context: EventCtx<State, ZwlrDataControlDeviceV1>) {
    let maybe_seat = event_context.state.seats.get_mut(&seat_name);

    let Some(seat) = maybe_seat else {
        log::warn!(
            target: log_default_target(),
            "Received ZwlrDataControlDeviceV1 event for unknown seat {}: {:?}",
            seat_name,
            event_context.event,
        );
        return;
    };

    match event_context.event {
        zwlr_data_control_device_v1::Event::DataOffer(data_offer) => {
            let offer = Offer::from(data_offer);

            if let Some(old_offer) = seat.selection_offers.insert(data_offer.id(), offer) {
                old_offer.data_control_offer.destroy(event_context.conn);
                log::error!(
                    target: &log_seat_target(seat_name),
                    "New selection event: new offer {} got introduced, even though offer with same id was already present",
                    data_offer.id().as_u32(),
                );
            } else {
                log::trace!(
                    target: &log_seat_target(seat_name),
                    "New selection event: new offer {} got introduced",
                    data_offer.id().as_u32(),
                );
            }

            event_context
                .conn
                .set_callback_for(data_offer, move |offer_event_context| {
                    data_control_offer_cb(seat_name, data_offer.id(), offer_event_context);
                });
        }
        zwlr_data_control_device_v1::Event::Selection(maybe_offer_id) => {
            let maybe_offer = maybe_offer_id.and_then(|offer_id| seat.selection_offers.remove(&offer_id));

            if let Some(offer_id) = &maybe_offer_id {
                if maybe_offer.is_some() {
                    log::trace!(
                        target: &log_seat_target(seat_name),
                        "New selection event: offer {} has been advertised as regular selection",
                        offer_id.as_u32(),
                    );
                } else {
                    log::warn!(
                        target: &log_seat_target(seat_name),
                        "New selection event: unknown offer {} has been advertised as regular selection",
                        offer_id.as_u32(),
                    );
                }
            }

            if let Some(regular_selection) = &mut seat.regular_selection {
                // Only set this flag to true if the selection type is activated,
                // to avoid unnecessary loops for handling the new state after the roundtrip
                seat.got_new_regular_selection = true;

                if maybe_offer_id.is_none() {
                    log::trace!(
                        target: &log_seat_target(seat_name),
                        "New selection event: regular selection got cleared",
                    );
                }

                if let Some(offer) = maybe_offer {
                    if should_ignore_offer(&event_context.state.settings, seat_name, SelectionType::Regular, &offer) {
                        regular_selection.got_ignored_event(event_context.conn);
                        offer.data_control_offer.destroy(event_context.conn);
                        return;
                    }

                    regular_selection.read_mimes(
                        event_context.conn,
                        offer.data_control_offer,
                        offer.mime_types,
                        offer.bytes_read,
                    );
                } else {
                    regular_selection.got_clear(event_context.conn);
                }
            } else {
                // We are not interested in the regular clipboard, so just destroy that offer, if any, and return.
                if let Some(offer) = maybe_offer {
                    offer.data_control_offer.destroy(event_context.conn);
                }
            }
        }
        zwlr_data_control_device_v1::Event::PrimarySelection(maybe_offer_id) => {
            let maybe_offer = maybe_offer_id.and_then(|offer_id| seat.selection_offers.remove(&offer_id));

            if let Some(offer_id) = &maybe_offer_id {
                if maybe_offer.is_some() {
                    log::trace!(
                        target: &log_seat_target(seat_name),
                        "New selection event: offer {} has been advertised as primary selection",
                        offer_id.as_u32(),
                    );
                } else {
                    log::warn!(
                        target: &log_seat_target(seat_name),
                        "New selection event: unknown offer {} has been advertised as primary selection",
                        offer_id.as_u32(),
                    );
                }
            }

            if let Some(primary_selection) = &mut seat.primary_selection {
                // Only set this flag to true if the selection type is activated,
                // to avoid unnecessary loops for handling the new state after the roundtrip
                seat.got_new_primary_selection = true;

                if maybe_offer_id.is_none() {
                    log::trace!(
                        target: &log_seat_target(seat_name),
                        "New selection event: primary selection got cleared",
                    );
                }

                if let Some(offer) = maybe_offer {
                    if should_ignore_offer(&event_context.state.settings, seat_name, SelectionType::Primary, &offer) {
                        primary_selection.got_ignored_event(event_context.conn);
                        offer.data_control_offer.destroy(event_context.conn);
                        return;
                    }

                    primary_selection.read_mimes(
                        event_context.conn,
                        offer.data_control_offer,
                        offer.mime_types,
                        offer.bytes_read,
                    );
                } else {
                    primary_selection.got_clear(event_context.conn);
                }
            } else {
                // We are not interested in the primary clipboard, so just destroy that offer, if any, and return.
                if let Some(offer) = maybe_offer {
                    offer.data_control_offer.destroy(event_context.conn);
                }
            }
        }
        zwlr_data_control_device_v1::Event::Finished => {
            if let Some(seat) = event_context.state.seats.remove(&seat_name) {
                seat.destroy(event_context.conn);
                log::trace!(
                    target: &log_seat_target(seat_name),
                    "Removed seat: due to data control being no longer valid"
                );
            } else {
                log::trace!(
                    target: &log_seat_target(seat_name),
                    "Data control is no longer valid"
                );
            }
        }
        fallback => {
            log::debug!(
                target: &log_seat_target(seat_name),
                "zwlr_data_control_device_v1::Event: unhandled event: {:?}",
                fallback,
            );
        }
    }
}

/// Handles the events for a selection offer, i.e. which mime types are offered.
fn data_control_offer_cb(
    seat_name: u32,
    data_offer_id: ObjectId,
    event_context: EventCtx<State, ZwlrDataControlOfferV1>,
) {
    match event_context.event {
        zwlr_data_control_offer_v1::Event::Offer(mime_type) => {
            let Some(seat) = event_context.state.seats.get_mut(&seat_name) else {
                log::warn!(
                    target: log_default_target(),
                    "New advertised mime type: got event for unknown seat {}",
                    seat_name
                );
                return;
            };

            let Some(offer) = seat.selection_offers.get_mut(&data_offer_id) else {
                log::warn!(
                    target: &log_seat_target(seat_name),
                    "New advertised mime type: got event for unknown offer {}",
                    data_offer_id.as_u32()
                );
                return;
            };

            if offer.bytes_exceeded_limit {
                return;
            }

            log::trace!(
                target: &log_seat_target(seat_name),
                "New advertised mime type: offer {} has mime type: '{}'",
                data_offer_id.as_u32(),
                mime_type.to_string_lossy(),
            );

            offer.bytes_read += mime_type.as_bytes().len() as u64;

            if offer.bytes_read > event_context.state.settings.selection_size_limit_bytes {
                log::trace!(
                    target: &log_seat_target(seat_name),
                    "New advertised mime type: exceeded selection size limit",
                );
                offer.bytes_exceeded_limit = true;
                return;
            }

            offer.mime_types.push(mime_type);
        }
        fallback => {
            log::debug!(
                target: &log_seat_target(seat_name),
                "zwlr_data_control_offer_v1::Event: unhandled event: {:?}",
                fallback,
            );
        }
    }
}

/// Whether the new offer should be ignored, based on mime types.
///
/// The offer is ignored if one of these cases is true:
/// * mime types exceed size limit
/// * no mime types were offered
/// * not all mime types match the regex
fn should_ignore_offer(settings: &Settings, seat_name: u32, selection_type: SelectionType, offer: &Offer) -> bool {
    if offer.bytes_exceeded_limit {
        log::trace!(
            target: &log_seat_target(seat_name),
            "Ignoring {} selection event: mime types exceeded specified size limit",
            selection_type.get_clipboard_type_str(false),
        );
        return true;
    }

    if offer.mime_types.is_empty() {
        log::trace!(
            target: &log_seat_target(seat_name),
            "Ignoring {} selection event: no mime types were offered",
            selection_type.get_clipboard_type_str(false),
        );
        return true;
    }

    // Log all available mime types.
    for mime_type in &offer.mime_types {
        log::trace!(
            target: &log_seat_target(seat_name),
            "Current {} selection event: offered mime types: '{}'",
            selection_type.get_clipboard_type_str(false),
            mime_type.to_string_lossy(),
        );
    }

    if let Some(regex) = settings.all_mime_type_regex.as_ref() {
        // Only keep this offer, if all mime types have a match for this regex.
        let match_all_regex = offer.mime_types.iter().all(|mime_type| {
            // TODO: Upstream issue: https://github.com/fancy-regex/fancy-regex/issues/84
            let mime_type_lossy = mime_type.to_string_lossy();

            match regex.is_match(&mime_type_lossy) {
                Ok(has_match) => {
                    if !has_match {
                        log::trace!(
                            target: &log_seat_target(seat_name),
                            "Ignoring {} selection event: mime type does not match the regex: '{}'",
                            selection_type.get_clipboard_type_str(false),
                            mime_type_lossy,
                        );
                    }

                    has_match
                }
                Err(err) => {
                    log::debug!(
                        target: &log_seat_target(seat_name),
                        "Current {} selection event: regex returned an error for mime type '{}': {}",
                        selection_type.get_clipboard_type_str(false),
                        mime_type_lossy,
                        err,
                    );

                    // Just assume that the mime type has a match.
                    true
                }
            }
        });

        if !match_all_regex {
            return true;
        }
    }

    false
}

/// Creates pipes for each mime type.
///
/// If there are no mime types to return (i.e. the
/// [`Vec`] is empty), `Ok(None)` is returned.
///
/// # Errors
///
/// If `ignore_selection_event_on_error` is `true`
/// and an error occurred while creating the pipe
/// or reading the metadata of the pipe, the error
/// is returned immediately in the [`Err`] variant.
///
/// If `ignore_selection_event_on_error` is `false`
/// and an error occurred the mime type is simply
/// ignored and will not appear in the return value.
///
/// # Side effects
///
/// The `mime_types` value is always replaced with
/// an empty [`Vec`].
fn create_pipes_for_mime_types(
    connection: &mut Connection<State>,
    seat_name: u32,
    selection_type: SelectionType,
    data_control_offer: ZwlrDataControlOfferV1,
    mime_types: &mut Vec<std::ffi::CString>,
    fd_from_own_app: &mut HashMap<FdIdentifier, bool>,
    ignore_selection_event_on_error: bool,
) -> std::io::Result<Option<Vec<MimeTypeAndPipe>>> {
    let mut mime_types_and_pipes = Vec::with_capacity(mime_types.len());

    for mime_type in std::mem::take(mime_types) {
        // Create a pipe to read the data for each mime type
        let (read, write) = match tokio_pipe::pipe() {
            Ok(pipe_ends) => pipe_ends,
            Err(err) => {
                if ignore_selection_event_on_error {
                    log::debug!(
                        target: &log_seat_target(seat_name),
                        "Ignoring {} selection event: failed to create pipe: {}",
                        selection_type.get_clipboard_type_str(false),
                        err
                    );
                    return Err(err);
                } else {
                    log::debug!(
                        target: &log_seat_target(seat_name),
                        "Current {} selection event: ignoring mime type '{}': failed to create pipe: {}",
                        selection_type.get_clipboard_type_str(false),
                        mime_type.to_string_lossy(),
                        err
                    );
                    continue;
                }
            }
        };

        // Save file descriptor identifier of the writable end of the pipe,
        // so we can check if we are writing to our own pipe.
        let write_file = unsafe { std::fs::File::from_raw_fd(write.into_raw_fd()) };
        let fd_identifier = match FdIdentifier::try_from(&write_file) {
            Ok(fd_identifier) => fd_identifier,
            Err(err) => {
                if ignore_selection_event_on_error {
                    log::debug!(
                        target: &log_seat_target(seat_name),
                        "Ignoring {} selection event: failed to get metadata for pipe: {}",
                        selection_type.get_clipboard_type_str(false),
                        err
                    );
                    return Err(err);
                } else {
                    log::debug!(
                        target: &log_seat_target(seat_name),
                        "Current {} selection event: ignoring mime type '{}': failed to get metadata for pipe: {}",
                        selection_type.get_clipboard_type_str(false),
                        mime_type.to_string_lossy(),
                        err
                    );
                    continue;
                }
            }
        };
        fd_from_own_app.insert(fd_identifier, false);

        // We want to receive the data for this mime type
        data_control_offer.receive(connection, mime_type.clone(), write_file.into());

        mime_types_and_pipes.push(MimeTypeAndPipe {
            mime_type: mime_type.into_boxed_c_str(),
            pipe: read,
            data_read: None,
            read_finished: false,
        });
    }

    if mime_types_and_pipes.is_empty() {
        log::trace!(
            target: &log_seat_target(seat_name),
            "Ignoring {} selection event: all pipe preparations resulted in an error",
            selection_type.get_clipboard_type_str(false)
        );
        return Ok(None);
    }

    Ok(Some(mime_types_and_pipes))
}

/// Handles the new selection state after receiving offers.
///
/// Specifically, it handles the cases [`SeatSelectionState::ReadMimes`],
/// [`SeatSelectionState::GotClear`] and [`SeatSelectionState::GotIgnoredEvent`].
///
/// # Errors
///
/// Only Wayland errors are returned.
async fn handle_new_selection_state(connection: &mut Connection<State>, state: &mut State) -> std::io::Result<()> {
    'handle_new_selection_state: loop {
        let selection_pipes = state
            .seats
            .iter_mut()
            .flat_map(|(seat_name, seat)| std::iter::repeat(seat_name).zip(seat.selections_iter_mut()))
            .filter_map(|(seat_name, (selection_type, selection_state))| {
                let Some(selection_state) = selection_state else {
                    return None;
                };

                match selection_state {
                    SeatSelectionState::WaitingForNewOffers => None,
                    SeatSelectionState::ReadMimes {
                        data_control_offer,
                        mime_types,
                        bytes_read: _,
                        fd_from_own_app,
                    } => {
                        let mime_types_with_pipes_result = create_pipes_for_mime_types(
                            connection,
                            *seat_name,
                            selection_type,
                            *data_control_offer,
                            mime_types,
                            fd_from_own_app,
                            state.settings.ignore_selection_event_on_error,
                        );
                        let mime_types_with_pipes = match mime_types_with_pipes_result {
                            Ok(Some(mime_types_with_pipes)) => mime_types_with_pipes,
                            Ok(None) => {
                                // No pipes, ignore
                                selection_state.reset(connection);
                                return None;
                            }
                            Err(_) => {
                                // Got error, ignore
                                selection_state.reset(connection);
                                return None;
                            }
                        };
                        Some((*seat_name, selection_type, mime_types_with_pipes))
                    }
                    SeatSelectionState::GotPipes {
                        pipes: _,
                        bytes_read: _,
                    } => None,
                    SeatSelectionState::GotClear => {
                        selection_state.reset(connection);
                        None
                    }
                    SeatSelectionState::GotIgnoredEvent => {
                        selection_state.reset(connection);
                        None
                    }
                }
            })
            .collect::<Vec<_>>();

        if selection_pipes.is_empty() {
            // There is nothing to do, therefore early return to avoid a roundtrip
            return Ok(());
        }

        // Set flags to false so we can check later if they got true
        for seat in state.seats.values_mut() {
            seat.got_new_regular_selection = false;
            seat.got_new_primary_selection = false;
        }

        // We need a roundtrip to know if these pipes are from ourselves
        connection.async_roundtrip().await?;
        connection.dispatch_events(state);

        // We have to check all seats, because `selection_pipes` might not cover every seat.
        // If there is a new offer in an unchecked seat, we should handle it in the next loop!
        let handle_new_selection_state_again = state
            .seats
            .values()
            .any(|seat| seat.got_new_regular_selection || seat.got_new_primary_selection);

        for (seat_name, selection_type, mime_types_with_pipes) in selection_pipes {
            let Some(seat) = state.seats.get_mut(&seat_name) else {
                // Seems like the seat got removed during the roundtrip, ignore
                continue;
            };

            let got_new_selection = match selection_type {
                SelectionType::Regular => seat.got_new_regular_selection,
                SelectionType::Primary => seat.got_new_primary_selection,
            };

            if got_new_selection {
                // We have got a new selection offer, the temporary pipes are outdated
                continue;
            }

            let maybe_selection_state = match selection_type {
                SelectionType::Regular => seat.regular_selection.as_mut(),
                SelectionType::Primary => seat.primary_selection.as_mut(),
            };

            let Some(selection_state) = maybe_selection_state else {
                unreachable!();
            };

            let SeatSelectionState::ReadMimes {
                data_control_offer: _,
                mime_types: _,
                bytes_read: _,
                fd_from_own_app,
            } = selection_state
            else {
                // This case cannot be triggered. If `got_new_selection` is false,
                // the state will not have changed and we will still be in the state
                // ReadMimes. If `got_new_selection` is true, then we would have
                // already `continue`-d.
                unreachable!();
            };

            if fd_from_own_app.values().next() == Some(&true) {
                log::trace!(
                    target: &log_seat_target(seat_name),
                    "Ignoring {} selection event: was triggered by ourselves",
                    selection_type.get_clipboard_type_str(false),
                );

                // Reset connection state
                selection_state.reset(connection);
                continue;
            }

            selection_state.got_pipes(connection, mime_types_with_pipes);
        }

        if !handle_new_selection_state_again {
            break 'handle_new_selection_state;
        }
    }

    Ok(())
}

/// Reads the data from a single pipe.
///
/// On success, [`PipeDataResult::data_result`] will be
/// [`Ok`].
///
/// # Errors
///
/// If an error while reading the pipe occurred,
/// or the size limit has been exceeded, the error
/// is returned in [`PipeDataResult::data_result`].
///
/// # Panics
///
/// This function panics if the pipe has already been completely
/// read or an error while reading it occurred before.
async fn read_pipe_to_data<'a>(
    mime_type_and_pipe: &'a mut MimeTypeAndPipe,
    bytes_read: Rc<RefCell<&mut u64>>,
    size_limit: u64,
) -> PipeDataResult<'a> {
    if mime_type_and_pipe.data_read.is_none() {
        mime_type_and_pipe.data_read = Some(Ok(Vec::with_capacity(32)));
    } else if mime_type_and_pipe.read_finished || matches!(mime_type_and_pipe.data_read, Some(Err(_))) {
        unreachable!();
    };

    let data = mime_type_and_pipe.data_read.as_mut().unwrap().as_mut().unwrap();
    let mut buf = [0u8; 8192];

    loop {
        match mime_type_and_pipe.pipe.read(&mut buf).await {
            Ok(0) => break,
            Ok(size) => {
                // Check size limit first
                **bytes_read.borrow_mut() += size as u64;

                if **bytes_read.borrow() > size_limit {
                    mime_type_and_pipe.read_finished = true;

                    return PipeDataResult {
                        mime_type_and_pipe,
                        data_result: Err(ReadToDataError::SizeLimitExceeded),
                    };
                }

                // Add data
                data.extend_from_slice(&buf[..size]);
            }
            Err(err) => {
                // Size limit: subtract this size from the current total size
                **bytes_read.borrow_mut() -= data.len() as u64;

                mime_type_and_pipe.read_finished = true;

                return PipeDataResult {
                    mime_type_and_pipe,
                    data_result: Err(ReadToDataError::IoError(err)),
                };
            }
        }
    }

    mime_type_and_pipe.read_finished = true;

    PipeDataResult {
        mime_type_and_pipe,
        data_result: Ok(()),
    }
}

/// Reads the pipes of all mime types.
///
/// If all pipes have been read already, and this function is called,
/// [`Ok`] is returned.
///
/// # Errors
///
/// If the size limit is exceeded while reading a pipe,
/// the [`ReadToDataError::SizeLimitExceeded`] is immediately
/// returned in the [`Err`] variant, regardless of the value
/// of `ignore_selection_event_on_error`.
///
/// If an error occurred while reading a pipe, and the value of
/// `ignore_selection_event_on_error` is true, this function
/// immediately returns the error in the [`Err`] variant.
///
/// Otherwise, if an error occured while reading a pipe, and
/// the value of `ignore_selection_event_on_error` is false,
/// the error is saved in the [`MimeTypeAndPipe::data_read`] field.
async fn read_pipes_to_data(
    pipes: &mut [MimeTypeAndPipe],
    bytes_read: &mut u64,
    size_limit: u64,
    ignore_selection_event_on_error: bool,
) -> Result<(), ReadToDataError> {
    let mut futures = FuturesUnordered::new();
    let shared_byted_read = Rc::new(RefCell::new(bytes_read));

    for mime_type_and_pipe in pipes {
        if mime_type_and_pipe.read_finished {
            continue;
        }

        futures.push(read_pipe_to_data(
            mime_type_and_pipe,
            Rc::clone(&shared_byted_read),
            size_limit,
        ));
    }

    while let Some(pipe_data_result) = futures.next().await {
        match pipe_data_result.data_result {
            Ok(_) => {}
            Err(err) => {
                if ignore_selection_event_on_error || matches!(err, ReadToDataError::SizeLimitExceeded) {
                    return Err(err);
                } else {
                    pipe_data_result.mime_type_and_pipe.data_read = Some(Err(err));
                }
            }
        }
    }

    Ok(())
}

/// Reads the pipes of all mime types until all are read completely.
///
/// If everything went well, the mime types are returned with the
/// corresponding data as a [`HashMap`] in the [`Ok`] variant.
/// The mime types where an error was returned while reading the pipe
/// are not included in the [`HashMap`].
///
/// # Errors
///
/// If an error occurred while reading one of the pipes,
/// and `ignore_selection_event_on_error` is true,
/// we return an [`Err`].
///
/// If all pipes returned an error while reading,
/// we will return an [`Err`] regardless of the
/// value of `ignore_selection_event_on_error`.
///
/// Possible errors while reading the pipe include exceeding
/// the given `size_limit`.
///
/// # Panics
///
/// This function panics if the given [SeatSelectionState]
/// argument is not [SeatSelectionState::GotPipes].
///
/// # Side effects
///
/// After all pipes have been read successfully
/// (i.e. [`read_pipes_to_data`] returned [`Ok`]),
/// the value in [`SeatSelectionState::GotPipes::pipes`]
/// is replaced with an empty [`Vec`].
async fn handle_pipes_selection_state(
    seat_name: u32,
    data_control_device: ZwlrDataControlDeviceV1,
    selection_type: SelectionType,
    selection_state: &mut SeatSelectionState,
    size_limit: u64,
    ignore_selection_event_on_error: bool,
) -> Result<MimeTypesWithData<'_>, &mut SeatSelectionState> {
    let SeatSelectionState::GotPipes { pipes, bytes_read } = selection_state else {
        unreachable!();
    };

    let data_result = read_pipes_to_data(pipes, bytes_read, size_limit, ignore_selection_event_on_error).await;
    let data = match data_result {
        Ok(_) => std::mem::take(pipes)
            .into_iter()
            .filter_map(|mime_type_and_pipe| match mime_type_and_pipe.data_read {
                Some(Ok(data)) => Some((mime_type_and_pipe.mime_type, data.into_boxed_slice())),
                Some(Err(err)) => {
                    log::trace!(
                        target: &log_seat_target(seat_name),
                        "Current {} selection event: ignoring mime type '{}': failed to read data: {}",
                        selection_type.get_clipboard_type_str(false),
                        mime_type_and_pipe.mime_type.to_string_lossy(),
                        err,
                    );
                    None
                }
                None => unreachable!(),
            })
            .collect::<HashMap<_, _>>(),
        Err(err) => {
            log::trace!(
                target: &log_seat_target(seat_name),
                "Ignoring {} selection event: failed to read data: {}",
                selection_type.get_clipboard_type_str(false),
                err
            );
            return Err(selection_state);
        }
    };

    if data.is_empty() {
        log::trace!(
            target: &log_seat_target(seat_name),
            "Ignoring {} selection event: all data reads returned an error",
            selection_type.get_clipboard_type_str(false),
        );
        return Err(selection_state);
    }

    Ok(MimeTypesWithData {
        seat_name,
        data_control_device,
        selection_state,
        selection_type,
        data,
    })
}

/// Handles clipboard data requests.
fn data_source_cb(
    seat_name: u32,
    selection_type: SelectionType,
    event_context: EventCtx<State, ZwlrDataControlSourceV1>,
    data_map: &Arc<HashMap<Box<CStr>, Box<[u8]>>>,
) {
    match event_context.event {
        zwlr_data_control_source_v1::Event::Send(send) => {
            log::trace!(
                target: &log_seat_target(seat_name),
                "{} clipboard data source {}: received new request for mime type '{}'",
                selection_type.get_clipboard_type_str(true),
                event_context.proxy.id().as_u32(),
                send.mime_type.to_string_lossy(),
            );

            // Check if the file descriptor comes from our own app
            let selection_state = event_context
                .state
                .seats
                .get_mut(&seat_name)
                .and_then(|seat| match selection_type {
                    SelectionType::Regular => seat.regular_selection.as_mut(),
                    SelectionType::Primary => seat.primary_selection.as_mut(),
                });

            let fd_file = unsafe { File::from_raw_fd(send.fd.into_raw_fd()) };
            let fd_identifier = match FdIdentifier::try_from(&fd_file) {
                Ok(fd_identifier) => fd_identifier,
                Err(err) => {
                    log::debug!(
                        target: &log_seat_target(seat_name),
                        "{} clipboard data source {}: could not get file metadata for mime type '{}': {}",
                        selection_type.get_clipboard_type_str(true),
                        event_context.proxy.id().as_u32(),
                        send.mime_type.to_string_lossy(),
                        err
                    );
                    drop(fd_file); // Explicitly close file descriptor
                    return;
                }
            };

            if let Some(SeatSelectionState::ReadMimes {
                data_control_offer: _,
                mime_types: _,
                bytes_read: _,
                fd_from_own_app,
            }) = selection_state
            {
                if let Some(is_from_own_app) = fd_from_own_app.get_mut(&fd_identifier) {
                    // File descriptor is from our own app, so we update the information
                    *is_from_own_app = true;
                    // Explicitly close file descriptor
                    drop(fd_file);
                    // Also we do not send any data, because we do not want to read it anyway.
                    // Therefore return.
                    return;
                }
            }

            let mime_type_boxed = send.mime_type.into_boxed_c_str();

            if !data_map.contains_key(&mime_type_boxed) {
                // Mime type not available, so return
                log::trace!(
                    target: &log_seat_target(seat_name),
                    "{} clipboard data source {}: mime type '{}' is not available",
                    selection_type.get_clipboard_type_str(true),
                    event_context.proxy.id().as_u32(),
                    mime_type_boxed.to_string_lossy(),
                );
                drop(fd_file); // Explicitly close file descriptor
                return;
            }

            // Write the data to the file descriptor in a worker thread,
            // so we do not block the main thread.
            let data_map_clone = Arc::clone(data_map);
            let mut tokio_file = tokio::fs::File::from_std(fd_file);
            let write_timeout = event_context.state.settings.write_timeout;

            tokio::spawn(async move {
                let data = data_map_clone.get(&mime_type_boxed).unwrap().deref();

                enum TimeoutResult {
                    Ok,
                    IoError(std::io::Error),
                    Timeout,
                }

                let deadline = tokio::time::Instant::now() + write_timeout;
                let write_result = tokio::select! {
                    biased;

                    res = tokio_file.write_all(data) => {
                        match res {
                            Ok(()) => TimeoutResult::Ok,
                            Err(err) => TimeoutResult::IoError(err),
                        }
                    }
                    _ = tokio::time::sleep_until(deadline) => {
                        TimeoutResult::Timeout
                    }
                };

                drop(data_map_clone);

                match write_result {
                    TimeoutResult::Ok => {
                        let flush_result = tokio::select! {
                            biased;

                            res = tokio_file.flush() => {
                                match res {
                                    Ok(()) => TimeoutResult::Ok,
                                    Err(err) => TimeoutResult::IoError(err),
                                }
                            }
                            _ = tokio::time::sleep_until(deadline) => {
                                TimeoutResult::Timeout
                            }
                        };

                        match flush_result {
                            TimeoutResult::Ok => {}
                            TimeoutResult::IoError(err) => {
                                log::debug!(
                                    target: &log_seat_target(seat_name),
                                    "{} clipboard data source {}: failed to flush clipboard data for mime type '{}': {}",
                                    selection_type.get_clipboard_type_str(true),
                                    event_context.proxy.id().as_u32(),
                                    mime_type_boxed.to_string_lossy(),
                                    err,
                                );
                            }
                            TimeoutResult::Timeout => {
                                log::debug!(
                                    target: &log_seat_target(seat_name),
                                    "{} clipboard data source {}: failed to flush clipboard data for mime type '{}': timed out",
                                    selection_type.get_clipboard_type_str(true),
                                    event_context.proxy.id().as_u32(),
                                    mime_type_boxed.to_string_lossy(),
                                );
                            }
                        }
                    }
                    TimeoutResult::IoError(err) => {
                        log::debug!(
                            target: &log_seat_target(seat_name),
                            "{} clipboard data source {}: failed to write clipboard data for mime type '{}': {}",
                            selection_type.get_clipboard_type_str(true),
                            event_context.proxy.id().as_u32(),
                            mime_type_boxed.to_string_lossy(),
                            err,
                        );
                    }
                    TimeoutResult::Timeout => {
                        log::debug!(
                            target: &log_seat_target(seat_name),
                            "{} clipboard data source {}: failed to write clipboard data for mime type '{}': timed out",
                            selection_type.get_clipboard_type_str(true),
                            event_context.proxy.id().as_u32(),
                            mime_type_boxed.to_string_lossy(),
                        );
                    }
                }

                // Explicitly close file descriptor
                drop(tokio_file);
            });
        }
        zwlr_data_control_source_v1::Event::Cancelled => {
            event_context.proxy.destroy(event_context.conn);

            log::trace!(
                target: &log_seat_target(seat_name),
                "{} clipboard data source {}: received cancelled event and destroyed clipboard data source",
                selection_type.get_clipboard_type_str(true),
                event_context.proxy.id().as_u32(),
            );
        }
        fallback => {
            log::debug!(
                target: &log_seat_target(seat_name),
                "zwlr_data_control_source_v1::Event: unhandled event: {:?}",
                fallback
            );
        }
    }
}
