use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::fmt::Display;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::prelude::AsRawFd;
use std::rc::Rc;
use std::time::{Duration, Instant};

use filedescriptor::FileDescriptor;

/// Reads the entire data from each file descriptors with a timeout.
pub(crate) fn read_with_timeout(
    mime_types_with_files: Vec<(Box<str>, FileDescriptor)>,
    timeout: Duration,
    return_err_on_timeout: bool,
    return_err_on_error: bool,
    limit_bytes: u64,
    already_used_bytes: u64,
) -> Result<HashMap<Box<str>, Result<Box<[u8]>, Rc<ReadWithTimeoutError>>>, ReadWithTimeoutError> {
    // Check size limit.
    let mut current_size = already_used_bytes;
    let mut exceeded_size_limit = current_size > limit_bytes;

    if exceeded_size_limit {
        return Err(ReadWithTimeoutError::SizeLimitExceeded);
    }

    // The return result. Maps an available mime type to either the read data or an error.
    let mut mime_type_to_data = HashMap::with_capacity(mime_types_with_files.len());

    // For each raw fd, we associate this data with it.
    struct FdData {
        mime_type: Box<str>,
        file: FileDescriptor,
        data: Vec<u8>,
    }

    let mut remaining_fd_to_data: HashMap<i32, FdData> = HashMap::with_capacity(mime_types_with_files.len());

    for (mime_type, mut file) in mime_types_with_files {
        if let Err(err) = file.set_non_blocking(true) {
            let final_err = ReadWithTimeoutError::NonBlockingIOMode(err);

            if return_err_on_error {
                return Err(final_err);
            }

            mime_type_to_data.insert(mime_type, Err(Rc::new(final_err)));
            continue;
        }

        let raw_fd = file.as_raw_fd();
        let fd_data = FdData {
            mime_type,
            file,
            data: Vec::with_capacity(32),
        };

        remaining_fd_to_data.insert(raw_fd, fd_data);
    }

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
    let mut read_error: Option<ReadWithTimeoutError> = None;

    // Convenience function to error out the remaining mime types.
    let error_out_remaining_mime_types =
        |mime_type_to_data: &mut HashMap<Box<str>, Result<Box<[u8]>, Rc<ReadWithTimeoutError>>>,
         remaining_fd_to_data: HashMap<i32, FdData>,
         error: ReadWithTimeoutError| {
            let rc_error = Rc::new(error);
            for FdData { mime_type, .. } in remaining_fd_to_data.into_values() {
                mime_type_to_data.insert(mime_type, Err(rc_error.clone()));
            }
        };

    // As long as there are still some remaining mime types...
    'outer: while !remaining_pfds.is_empty() {
        // Check if time has run out. If so, error out the remaining mime types.
        let remaining_time = end_time
            .saturating_duration_since(Instant::now())
            .as_millis()
            .try_into()
            .unwrap();

        if remaining_time == 0 {
            let final_err = ReadWithTimeoutError::Timeout;

            if return_err_on_timeout {
                return Err(final_err);
            }

            error_out_remaining_mime_types(&mut mime_type_to_data, remaining_fd_to_data, final_err);
            break 'outer;
        }

        // Wait for changes of readability with timeout.
        match unsafe { libc::poll(remaining_pfds.as_mut_ptr(), remaining_pfds.len() as u64, remaining_time) } {
            0 => {
                let final_err = ReadWithTimeoutError::Timeout;

                if return_err_on_timeout {
                    return Err(final_err);
                }

                // Timeout occurred, error out the remaining mime types.
                error_out_remaining_mime_types(&mut mime_type_to_data, remaining_fd_to_data, final_err);
                break 'outer;
            }
            -1 => {
                // Some other error occurred, error out the remaining mime types.
                let err = std::io::Error::last_os_error();
                let final_err = ReadWithTimeoutError::Poll(err);

                if return_err_on_error {
                    return Err(final_err);
                }

                error_out_remaining_mime_types(&mut mime_type_to_data, remaining_fd_to_data, final_err);
                break 'outer;
            }
            1.. => {
                // Readability might have changed, checking...
                remaining_pfds.retain_mut(|pfd| {
                    if exceeded_size_limit {
                        // Just ignore reading the data when the size limit has been exceeded.
                        return true;
                    }

                    if read_error.is_some() {
                        // Just ignore reading the data when a read error has occurred.
                        return true;
                    }

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
                                        // Check size limit first.
                                        current_size += size as u64;

                                        if current_size > limit_bytes {
                                            exceeded_size_limit = true;
                                            return true;
                                        }

                                        // Add data.
                                        data.extend_from_slice(&buf[..size]);
                                        break 'read;
                                    }
                                    Err(err) if err.kind() == ErrorKind::Interrupted => {
                                        // Got interrupted, therefore retry read.
                                        continue 'read;
                                    }
                                    Err(err) => {
                                        // Some error occurred.
                                        let final_err = ReadWithTimeoutError::Read(err);

                                        if return_err_on_error {
                                            read_error = Some(final_err);
                                            return true;
                                        }

                                        // Remove entry from data map and insert error to result.
                                        let removed_entry = entry.remove_entry();
                                        let owned_mime_type = removed_entry.1.mime_type;
                                        mime_type_to_data.insert(owned_mime_type, Err(Rc::new(final_err)));

                                        // Size limit: subtract this size from the current total size.
                                        current_size -= removed_entry.1.data.len() as u64;

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

                if exceeded_size_limit {
                    return Err(ReadWithTimeoutError::SizeLimitExceeded);
                }

                if let Some(read_error) = read_error {
                    return Err(read_error);
                }
            }
            return_value => {
                // Invalid return value, error out the remaining mime types.
                let final_err = ReadWithTimeoutError::PollInvalidReturnValue(return_value);

                if return_err_on_error {
                    return Err(final_err);
                }

                error_out_remaining_mime_types(&mut mime_type_to_data, remaining_fd_to_data, final_err);
                break 'outer;
            }
        }
    }

    Ok(mime_type_to_data)
}

/// Possible errors for the read_with_timeout function.
pub(crate) enum ReadWithTimeoutError {
    NonBlockingIOMode(filedescriptor::Error),
    Poll(std::io::Error),
    PollInvalidReturnValue(i32),
    Read(std::io::Error),
    Timeout,
    SizeLimitExceeded,
}

impl Display for ReadWithTimeoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadWithTimeoutError::NonBlockingIOMode(err) => write!(
                f,
                "Failed to enable the non-blocking IO mode of the file descriptor. Error: {}",
                err
            ),
            ReadWithTimeoutError::Poll(err) => {
                write!(f, "Error while polling to read from the file descriptor: {}", err)
            }
            ReadWithTimeoutError::PollInvalidReturnValue(return_value) => write!(
                f,
                "Received invalid return value {} from polling to read from the file descriptor",
                return_value
            ),
            ReadWithTimeoutError::Read(err) => write!(f, "Error while reading from file descriptor: {}", err),
            ReadWithTimeoutError::Timeout => write!(f, "Timed out reading from file descriptor"),
            ReadWithTimeoutError::SizeLimitExceeded => write!(f, "Offer exceeded specified size limit"),
        }
    }
}

/// Writes the entire data to a file descriptor with a timeout.
pub(crate) fn write_with_timeout(
    mut file: FileDescriptor,
    mut data: &[u8],
    timeout: Duration,
    interrupt_read: Option<&FileDescriptor>,
) -> Result<(), WriteWithTimeoutError> {
    if let Err(err) = file.set_non_blocking(true) {
        return Err(WriteWithTimeoutError::NonBlockingIOMode(err));
    }

    let write_pfd = libc::pollfd {
        fd: file.as_raw_fd(),
        events: libc::POLLOUT,
        revents: 0,
    };

    let mut pfds: Box<[libc::pollfd]> = if let Some(interrupt_read) = interrupt_read {
        let interrupt_pfd = libc::pollfd {
            fd: interrupt_read.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        Box::new([write_pfd, interrupt_pfd])
    } else {
        Box::new([write_pfd])
    };

    let end_time = Instant::now() + timeout;

    'outer: while !data.is_empty() {
        let remaining_time = end_time
            .saturating_duration_since(Instant::now())
            .as_millis()
            .try_into()
            .unwrap();

        if remaining_time == 0 {
            return Err(WriteWithTimeoutError::Timeout);
        }

        match unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as u64, remaining_time) } {
            0 => {
                return Err(WriteWithTimeoutError::Timeout);
            }
            -1 => {
                let err = std::io::Error::last_os_error();
                return Err(WriteWithTimeoutError::Poll(err));
            }
            1.. => {
                if interrupt_read.is_some() && pfds.last().unwrap().revents & libc::POLLIN != 0 {
                    return Err(WriteWithTimeoutError::Interrupt);
                }

                let write_pfd = pfds.first_mut().unwrap();

                if write_pfd.revents == 0 {
                    // We are not able to write data, so skip
                    continue 'outer;
                }

                'write: loop {
                    match file.write(data) {
                        Ok(size) if size == 0 => {
                            return Err(WriteWithTimeoutError::WriteStopped);
                        }
                        Ok(size) => {
                            data = &data[size..];
                            break 'write;
                        }
                        Err(err) if err.kind() == ErrorKind::Interrupted => {
                            continue 'write;
                        }
                        Err(err) => {
                            return Err(WriteWithTimeoutError::Write(err));
                        }
                    };
                }

                // Make the pollfd re-usable for the next poll call
                write_pfd.revents = 0;
            }
            return_value => {
                return Err(WriteWithTimeoutError::PollInvalidReturnValue(return_value));
            }
        }
    }

    Ok(())
}

/// Possible errors for the write_with_timeout function.
pub(crate) enum WriteWithTimeoutError {
    NonBlockingIOMode(filedescriptor::Error),
    Poll(std::io::Error),
    PollInvalidReturnValue(i32),
    Write(std::io::Error),
    WriteStopped,
    Timeout,
    Interrupt,
}

impl Display for WriteWithTimeoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WriteWithTimeoutError::NonBlockingIOMode(err) => write!(
                f,
                "Failed to enable the non-blocking IO mode of the file descriptor. Error: {}",
                err
            ),
            WriteWithTimeoutError::Poll(err) => {
                write!(f, "Error while polling to write to the file descriptor: {}", err)
            }
            WriteWithTimeoutError::PollInvalidReturnValue(return_value) => write!(
                f,
                "Received invalid return value {} from polling to write to the file descriptor",
                return_value
            ),
            WriteWithTimeoutError::Write(err) => write!(f, "Error while writing to file descriptor: {}", err),
            WriteWithTimeoutError::WriteStopped => write!(f, "Failed to write whole buffer to the file descriptor"),
            WriteWithTimeoutError::Timeout => write!(f, "Timed out writing to file descriptor"),
            WriteWithTimeoutError::Interrupt => write!(f, "Received interrupt request"),
        }
    }
}
