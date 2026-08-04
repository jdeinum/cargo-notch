use tracing::error;

pub fn main() {
    match cargo_notch::run() {
        Ok(()) => {}
        Err(e) => {
            error!("Error running the build tool: {e:?}");
            std::process::exit(1);
        }
    }
}
