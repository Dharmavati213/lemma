//! The `lemma` binary: process-wide set-up, then [`lemma_app::app::run`].

use std::process::ExitCode;
use std::time::Instant;

fn main() -> ExitCode {
    let started = Instant::now();
    tune_malloc();
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

/// Fewer glibc arenas and an mmap threshold of 1 MiB keep freed memory returnable to the OS
/// (ARCHITECTURE §7).
fn tune_malloc() {
    // SAFETY: mallopt only changes allocator tunables; it is called before any other thread exists.
    unsafe {
        libc::mallopt(libc::M_MMAP_THRESHOLD, 1 << 20);
        libc::mallopt(libc::M_ARENA_MAX, 2);
    }
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
/// provider plugin whose library is not installed (`Error loading plugin: libvoikko.so.1: …`).
/// GLib allows one writer per process, so this is the only place that can filter them.
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
        glib::log_writer_default(level, fields)
    });
}
