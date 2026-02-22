use tracing_panic::panic_hook;

pub mod os;

/// Register a panic hook to log the panic message and location, then exit the process.
pub fn register_panic_hook() {
    std::panic::set_hook(Box::new(panic_hook));
}

pub fn deadlock_detection() {
    #[cfg(feature = "deadlock_detection")]
    {
        todo!()
    }
}

pub fn must_check_elevation() -> bool {
    #[cfg(windows)]
    {
        use check_elevation::is_elevated;
        is_elevated().unwrap()
    }
    #[cfg(not(windows))]
    {
        todo!()
    }
}
