use cargo_notch::run;
use tracing::error;
use tracing_subscriber::EnvFilter;
pub fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    match run() {
        Ok(()) => {}
        Err(e) => error!("Error running the build tool: {e:?}"),
    }
}
