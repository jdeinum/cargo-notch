use tracing::error;

// a comment that doesn't actually do anything, just for the demo

pub fn main() {
    match cargo_notch::run() {
        Ok(()) => {}
        Err(e) => {
            error!("Error running the build tool: {e:?}");
            std::process::exit(1);
        }
    }
}
