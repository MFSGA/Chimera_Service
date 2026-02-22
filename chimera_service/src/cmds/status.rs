#[derive(Debug, clap::Args)]
pub struct StatusCommand {
    /// Output the result in JSON format
    #[clap(long, default_value = "false")]
    json: bool,

    /// Skip the service check
    #[clap(long, default_value = "false")]
    skip_service_check: bool,
}
