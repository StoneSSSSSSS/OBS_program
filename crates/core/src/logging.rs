//! Log files in `%LOCALAPPDATA%\ClipVault\logs` (daily rotation, auto-pruned).

use std::time::{Duration, SystemTime};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

pub struct LogGuard {
    _guard: tracing_appender::non_blocking::WorkerGuard,
}

/// Initialise logging for `app` ("service" / "gui"). Safe to call once per process.
pub fn init(app: &str, level: &str, retention_days: u32) -> Option<LogGuard> {
    let dir = crate::paths::logs_dir();
    std::fs::create_dir_all(&dir).ok()?;
    prune(&dir, app, retention_days);
    let appender = tracing_appender::rolling::daily(&dir, format!("{app}.log"));
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let filter = EnvFilter::try_from_env("CLIPVAULT_LOG")
        .unwrap_or_else(|_| EnvFilter::new(format!("{level},tungstenite=warn,rustls=warn")));
    let file_layer = fmt::layer().with_ansi(false).with_target(false).with_thread_names(true).with_writer(writer);
    let stderr_layer = std::env::var_os("CLIPVAULT_STDERR_LOG")
        .map(|_| fmt::layer().with_target(false).with_writer(std::io::stderr));
    let _ = tracing_subscriber::registry().with(filter).with(file_layer).with(stderr_layer).try_init();
    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!("panic: {info}");
        default_panic(info);
    }));
    Some(LogGuard { _guard: guard })
}

fn prune(dir: &std::path::Path, app: &str, retention_days: u32) {
    let cutoff = SystemTime::now() - Duration::from_secs(retention_days.max(1) as u64 * 86_400);
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.starts_with(app) {
                continue;
            }
            if let Ok(m) = e.metadata().and_then(|m| m.modified()) {
                if m < cutoff {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }
}
