use std::num::NonZeroU64;
use std::time::Duration;

use clap::builder::NonEmptyStringValueParser;
use clap::{arg, crate_description, crate_name, crate_version, value_parser, ArgAction, Command};
use fancy_regex::Regex;

use crate::logger::{self, log_default_target};

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, clap::ValueEnum)]
pub(crate) enum ClipboardType {
    Regular,
    Primary,
    Both,
}

impl ClipboardType {
    /// Whether the primary selection is activated.
    pub(crate) const fn primary(&self) -> bool {
        match self {
            Self::Primary | Self::Both => true,
            Self::Regular => false,
        }
    }

    /// Whether the regular selection is activated.
    pub(crate) const fn regular(&self) -> bool {
        match self {
            Self::Regular | Self::Both => true,
            Self::Primary => false,
        }
    }
}

/// The settings the program was started with.
#[derive(Debug, Clone)]
pub(crate) struct Settings {
    /// The clipboard types which are activated.
    pub(crate) clipboard_type: ClipboardType,
    /// The write timeout when writing the clipboard data to other clients.
    pub(crate) write_timeout: Duration,
    /// Whether selection events should be ignored when at least one error occurred.
    pub(crate) ignore_selection_event_on_error: bool,
    /// The selection size limit in bytes, or [`None`] if no limit.
    pub(crate) selection_size_limit_bytes: Option<NonZeroU64>,
    /// If [`None`], the selection events should not be filtered by a [`Regex`].
    /// Otherwise, all mime types have to match the regex for it to be not ignored.
    pub(crate) all_mime_type_regex: Option<Regex>,
}

/// Get the settings for the program.
pub(crate) fn get_settings() -> Settings {
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
                -w --"write-timeout" <MILLISECONDS> "Timeout for trying to send the current clipboard to other programs"
            )
            .required(false)
            .value_parser(clap::value_parser!(u64).range(1..=i32::MAX as u64))
            .default_value("3000"),
        )
        .arg(
            arg!(
                -e --"ignore-event-on-error" "Only handle selection events where no error occurred"
            )
            .required(false)
            .action(ArgAction::SetTrue),
        )
        .arg(
            arg!(
                -l --"selection-size-limit" <BYTES> "Only handle selection events whose total data size does not exceed the size limit"
            )
            .required(false)
            .value_parser(clap::value_parser!(NonZeroU64)),
        )
        .arg(
            arg!(
                -f --"all-mime-type-regex" <REGEX> "Only handle selection events where all offered MIME types have a match for the regex"
            )
            .required(false)
            .value_parser(NonEmptyStringValueParser::new()),
        )
        .arg(
            arg!(
                --"disable-timestamps" "Do not show timestamps in the log messages"
            )
            .required(false)
            .action(ArgAction::SetTrue),
        )
        .get_matches();

    // Initialize the logger here, because log is used to inform about invalid settings
    let disable_timestamps = matches.get_flag("disable-timestamps");
    logger::init_logger(!disable_timestamps);

    let clipboard_type = *matches.get_one::<ClipboardType>("clipboard").unwrap();
    let write_timeout = Duration::from_millis(*matches.get_one::<u64>("write-timeout").unwrap());
    let ignore_selection_event_on_error = matches.get_flag("ignore-event-on-error");
    let selection_size_limit_bytes = matches.get_one::<NonZeroU64>("selection-size-limit").copied();
    let all_mime_type_regex = matches
        .get_one::<String>("all-mime-type-regex")
        .map(|s| match Regex::new(s) {
            Ok(regex) => regex,
            Err(err) => {
                log::error!(
                    target: log_default_target(),
                    "Failed to parse the mime type regex: {}",
                    err
                );
                std::process::exit(1);
            }
        });

    Settings {
        clipboard_type,
        write_timeout,
        ignore_selection_event_on_error,
        selection_size_limit_bytes,
        all_mime_type_regex,
    }
}
