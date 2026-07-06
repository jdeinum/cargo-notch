use cargo_notch::run;
use tracing::error;
pub fn main() {
    tracing_subscriber::fmt::init();
    match run() {
        Ok(()) => {}
        Err(e) => error!("Error running the build tool: {e:?}"),
    }
}
