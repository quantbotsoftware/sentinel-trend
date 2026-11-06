use tracing_subscriber::{fmt, fmt::writer::MakeWriterExt, EnvFilter};

/// GUI process: `<logs>/sentinel-trend.log.<date>`.
pub fn init() {
    init_named("sentinel-trend.log");
}

/// Daily-rolling file `<logs>/<prefix>.<date>` + stdout. The headless
/// MERIDIAN daemon uses its own prefix so two processes never append to
/// the same file (`meridian_daily_report.py` reads both).
pub fn init_named(prefix: &str) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,sentinel_trend=debug"));

    let file_appender = tracing_appender::rolling::daily(
        crate::paths::logs_dir(),
        prefix,
    );
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    // Leak the guard so file logging stays alive for the process lifetime.
    // Safe: the process owns this singleton and it must outlive all tracing calls.
    std::mem::forget(_guard);

    fmt()
        .with_env_filter(filter)
        .with_writer(non_blocking.and(std::io::stdout))
        .with_target(false)
        .with_thread_ids(false)
        .compact()
        .init();
}
