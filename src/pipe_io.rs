use std::borrow::Cow;
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::prelude::AsRawFd;
use std::time::{Duration, Instant};

use filedescriptor::FileDescriptor;

/// Reads the entire data from each file descriptors with a timeout.
pub(crate) fn read_with_timeout(
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
                    if pfd.revents == 0 {
                        // Keep this pfd, since we still have not read everything.
                        return true;
                    }

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
                                            Err(format!("Error while reading from file descriptor: {}", err).into()),
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

/// Writes the entire data to a file descriptor with a timeout.
pub(crate) fn write_with_timeout(
    mut file: FileDescriptor,
    mut data: &[u8],
    timeout: Duration,
) -> Result<(), Cow<'static, str>> {
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
