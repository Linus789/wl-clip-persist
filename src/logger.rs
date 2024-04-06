use chrono::Local;
use env_logger::fmt::style::{AnsiColor, Color, Style};
use env_logger::Builder;
use log::{Level, LevelFilter};

pub(crate) fn init_logger(with_timestamps: bool) {
    custom_logger_builder("%Y-%m-%dT%H:%M:%S.%3f", with_timestamps)
        .filter_level(LevelFilter::Info)
        .parse_default_env()
        .init()
}

fn custom_logger_builder(fmt: &'static str, with_timestamps: bool) -> Builder {
    let mut builder = Builder::new();

    builder.format(move |f, record| {
        use std::io::Write as _;

        let target = record.target();

        if !target.starts_with(clap::crate_name!()) {
            return Ok(());
        }

        let time = with_timestamps.then(|| Local::now().format(fmt));
        let level = record.level();
        let level_text = level_text(&level);
        let level_style = level_style(&level);
        let display_target = target.strip_prefix(concat!(clap::crate_name!(), ' ')).unwrap_or("main");
        let message = record.args().to_string().lines().collect::<Vec<_>>().join("\n  ");

        if let Some(time) = time {
            writeln!(
                f,
                "{} {}{}{} {} > {}",
                time,
                level_style.render(),
                level_text,
                level_style.render_reset(),
                display_target,
                message
            )
        } else {
            writeln!(
                f,
                "{}{}{} {} > {}",
                level_style.render(),
                level_text,
                level_style.render_reset(),
                display_target,
                message
            )
        }
    });

    builder
}

fn level_text(level: &Level) -> &'static str {
    match level {
        Level::Trace => "TRACE",
        Level::Debug => "DEBUG",
        Level::Info => "INFO ",
        Level::Warn => "WARN ",
        Level::Error => "ERROR",
    }
}

fn level_style(level: &Level) -> Style {
    Style::new().fg_color(Some(match level {
        Level::Trace => Color::Ansi(AnsiColor::Magenta),
        Level::Debug => Color::Ansi(AnsiColor::Blue),
        Level::Info => Color::Ansi(AnsiColor::Green),
        Level::Warn => Color::Ansi(AnsiColor::Yellow),
        Level::Error => Color::Ansi(AnsiColor::Red),
    }))
}

/// Returns a formatted target for logging purposes.
pub(crate) const fn log_default_target() -> &'static str {
    clap::crate_name!()
}

/// Returns a formatted target for logging purposes.
pub(crate) fn log_seat_target(seat_name: u32) -> String {
    format!("{} Seat {}", clap::crate_name!(), seat_name)
}

/// If title_case is false, lower case is used instead of title case.
pub(crate) const fn get_clipboard_type_str(is_primary_clipboard: bool, title_case: bool) -> &'static str {
    match (is_primary_clipboard, title_case) {
        (true, true) => "Primary",
        (true, false) => "primary",
        (false, true) => "Regular",
        (false, false) => "regular",
    }
}
