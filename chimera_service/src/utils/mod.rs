use anyhow::Context;
use service_manager::ServiceManager;
use tracing_panic::panic_hook;

#[cfg(windows)]
pub mod acl;
pub mod os;

pub mod dirs;

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
        use whoami::username;
        username() == "root"
    }
}

pub fn get_service_manager() -> Result<Box<dyn ServiceManager>, anyhow::Error> {
    let manager = <dyn ServiceManager>::native()?;
    if !manager.available().context(
        "service manager is not available, please make sure you are running as root or administrator",
    )? {
        anyhow::bail!("service manager not available");
    }
    Ok(manager)
}
