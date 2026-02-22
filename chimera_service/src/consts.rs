pub enum ExitCode {
    Normal = 0,
    PermissionDenied = 64,
    ServiceNotInstalled = 100,
    ServiceAlreadyInstalled = 101,
    ServiceAlreadyStopped = 102,
    ServiceAlreadyRunning = 103,
    Other = 1,
}

impl std::process::Termination for ExitCode {
    fn report(self) -> std::process::ExitCode {
        std::process::ExitCode::from(self as u8)
    }
}
