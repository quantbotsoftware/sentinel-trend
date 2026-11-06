use std::path::PathBuf;

pub fn data_dir() -> PathBuf {
    dirs_fallback().join("sentinel-trend")
}

pub fn config_dir() -> PathBuf {
    data_dir().join("config")
}

pub fn logs_dir() -> PathBuf {
    data_dir().join("logs")
}

pub fn snapshots_dir() -> PathBuf {
    data_dir().join("snapshots")
}

pub fn journal_dir() -> PathBuf {
    data_dir().join("journal")
}

pub fn ensure_data_dirs() -> std::io::Result<()> {
    let dirs = [data_dir(), config_dir(), logs_dir(), snapshots_dir(), journal_dir()];
    for dir in &dirs {
        std::fs::create_dir_all(dir)?;
    }
    tracing::info!("Data directories ensured at {:?}", data_dir());
    Ok(())
}

fn dirs_fallback() -> PathBuf {
    if let Some(data) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(data);
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".local").join("share");
    }
    panic!(
        "SentinelTrend: cannot determine private data directory. \
         Set XDG_DATA_HOME or HOME environment variable."
    );
}
