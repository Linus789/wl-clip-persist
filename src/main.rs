pub(crate) mod logger;
pub(crate) mod settings;
pub(crate) mod states;
mod wayland;

use std::time::Duration;

use crate::logger::log_default_target;
use crate::settings::get_settings;
use crate::states::WaylandError;

const RECONNECT_TRIES: u8 = 30;
const RECONNECT_TIMEOUT: Duration = Duration::from_millis(100);

// One worker thread for writing the clipboard data
#[tokio::main(flavor = "multi_thread", worker_threads = 1)]
async fn main() {
    let settings = get_settings();
    let mut is_reconnect = false;
    let mut connection_tries = 0;

    loop {
        match wayland::run(settings.clone(), is_reconnect).await {
            Ok(_) => unreachable!(),
            Err(WaylandError::ConnectError(err)) => {
                if is_reconnect {
                    connection_tries += 1;

                    if connection_tries == RECONNECT_TRIES {
                        log::error!(
                            target: log_default_target(),
                            "Wayland connect error: {}",
                            err,
                        );
                        std::process::exit(1);
                    } else {
                        log::error!(
                            target: log_default_target(),
                            "Wayland connect error: {}\nAttempt {}/{} to reconnect in {} ms...",
                            err,
                            connection_tries + 1,
                            RECONNECT_TRIES,
                            RECONNECT_TIMEOUT.as_millis()
                        );
                        tokio::time::sleep(RECONNECT_TIMEOUT).await;
                    }
                } else {
                    log::error!(
                        target: log_default_target(),
                        "Failed to connect to wayland server: {}",
                        err,
                    );
                    std::process::exit(1);
                }
            }
            Err(WaylandError::IoError(err)) => {
                if RECONNECT_TRIES > 0 {
                    is_reconnect = true;
                    connection_tries = 0;

                    log::error!(
                        target: log_default_target(),
                        "Wayland IO error: {}\nAttempt {}/{} to reconnect will start immediately...",
                        err,
                        connection_tries + 1,
                        RECONNECT_TRIES,
                    );
                } else {
                    log::error!(
                        target: log_default_target(),
                        "Wayland IO error: {}",
                        err,
                    );
                    std::process::exit(1);
                }
            }
        }
    }
}
