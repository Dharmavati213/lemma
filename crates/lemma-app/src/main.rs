//! The `lemma` binary: process-wide set-up, then [`lemma_app::app::run`].
// On Windows a GUI program, which opens no console window (`lemma_app::windows` attaches to the
// console of a terminal that starts it).
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::process::ExitCode;
use std::time::Instant;

fn main() -> ExitCode {
    let started = Instant::now();
    lemma_app::heap::tune();
    // An app started from Finder gets launchd's environment: the bundle's own libraries and data
    // must be named, and PATH lacks Homebrew, MacTeX and the user's own tools.
    #[cfg(target_os = "macos")]
    lemma_app::macos::prepare_environment();
    // A terminal's console for the output, and the TeX, Node.js and Git directories on PATH.
    #[cfg(windows)]
    lemma_app::windows::prepare_environment();
    init_tracing();
    quiet_environment_noise();

    let options = match lemma_app::cli::Options::parse(std::env::args_os().skip(1)) {
        Ok(lemma_app::cli::Parsed::Run(options)) => options,
        Ok(lemma_app::cli::Parsed::Help) => {
            print!("{}", lemma_app::cli::USAGE);
            return ExitCode::SUCCESS;
        }
        Ok(lemma_app::cli::Parsed::Version) => {
            println!("lemma {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("lemma: {e}\n\n{}", lemma_app::cli::USAGE);
            return ExitCode::from(2);
        }
    };
    lemma_app::app::run(options, started)
}

/// Logs to stderr, filtered by `LEMMA_LOG` (an `EnvFilter` directive such as `lemma_app=debug`);
/// warnings and errors only by default.
fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_env("LEMMA_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

/// Drops the GLib warnings that describe the machine rather than Lemma and passes everything else
/// to GLib's default writer: enchant (spell checking) warns once per process about every optional
/// provider plugin whose library is not installed (`Error loading plugin: libvoikko.so.1: …`),
/// and on Windows GApplication looks for a D-Bus session bus, which Lemma (one window per
/// process) does not use, and warns that it has no `gdbus.exe` to start one. GLib allows one
/// writer per process, so this is the only place that can filter them.
fn quiet_environment_noise() {
    glib::log_set_writer_func(|level, fields| {
        let field = |key: &str| {
            fields
                .iter()
                .find(|f| f.key() == key)
                .and_then(glib::LogField::value_str)
        };
        if field("GLIB_DOMAIN") == Some("libenchant")
            && field("MESSAGE").is_some_and(|m| m.contains("Error loading plugin"))
        {
            return glib::LogWriterOutput::Handled;
        }
        if cfg!(windows)
            && field("GLIB_DOMAIN") == Some("GLib-GIO")
            && field("MESSAGE").is_some_and(|m| m.contains("session dbus binary not found"))
        {
            return glib::LogWriterOutput::Handled;
        }
        glib::log_writer_default(level, fields)
    });
}
