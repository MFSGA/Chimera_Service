/* Legacy core manager retired; retained temporarily for incremental deletion.
        Self::notify_state_changed(self.state_changed_notify.clone(), CoreState::Stopped(None));
        Ok(())
    }
}

// TODO: support system path search via a config or flag
/// Search the binary path of the core: Data Dir -> Sidecar Dir
pub fn find_binary_path(core_type: &CoreType) -> std::io::Result<PathBuf> {
    let infos = consts::RuntimeInfos::global();
    let binary_name = resource_variant(core_type)
        .map(|variant| variant.binary_name())
        .unwrap_or_else(|| core_type.get_executable_name());
    let data_dir = &infos.nyanpasu_data_dir;
    let binary_path = data_dir.join(binary_name);
    if binary_path.exists() {
        return Ok(binary_path);
    }
    let app_dir = &infos.nyanpasu_app_dir;
    let binary_path = app_dir.join(binary_name);
    if binary_path.exists() {
        return Ok(binary_path);
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("{binary_name} not found"),
    ))
}

fn resource_variant(core_type: &CoreType) -> Option<ClashCoreResourceVariant> {
    match core_type {
        CoreType::Clash(ClashCoreType::Mihomo) => Some(ClashCoreResourceVariant::Mihomo),
        CoreType::Clash(ClashCoreType::MihomoAlpha) => {
            Some(ClashCoreResourceVariant::MihomoAlpha)
        }
        CoreType::Clash(ClashCoreType::ClashRust) => Some(ClashCoreResourceVariant::ClashRust),
        CoreType::Clash(ClashCoreType::ClashRustAlpha) => {
            Some(ClashCoreResourceVariant::ClashRustAlpha)
        }
        CoreType::Clash(ClashCoreType::ClashPremium) => {
            Some(ClashCoreResourceVariant::ClashPremium)
        }
        CoreType::Clash(ClashCoreType::ChimeraClient) | CoreType::SingBox => None,
    }
}

async fn run_config_check(
    core_type: &CoreType,
    config_path: &Utf8Path,
    binary_path: &Utf8Path,
    app_dir: &Utf8Path,
) -> Result<(), CoreOperationError> {
    let config_dir = config_path.parent().ok_or_else(|| {
        CoreOperationError::ConfigCheckFailed(anyhow::anyhow!(
            "config path has no parent directory"
        ))
    })?;
    let mut command = Command::new(binary_path.as_std_path());
    command
        .arg("-t")
        .arg("-d")
        .arg(app_dir.as_std_path())
        .arg("-f")
        .arg(config_path.as_std_path())
        .env(
            MIHOMO_SAFE_PATHS_ENV_NAME,
            CoreInstance::get_mihomo_safe_paths(app_dir, config_dir, None),
        )
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);

    let output = tokio::time::timeout(CONFIG_CHECK_TIMEOUT, command.output())
        .await
        .map_err(|_| {
            CoreOperationError::ConfigCheckFailed(anyhow::anyhow!(
                "config check timed out after {} seconds",
                CONFIG_CHECK_TIMEOUT.as_secs()
            ))
        })?
        .map_err(|error| CoreOperationError::ConfigCheckFailed(error.into()))?;

    if output.status.success() {
        return Ok(());
    }
    let diagnostic = if !matches!(core_type, CoreType::Clash(ClashCoreType::ClashRust)) {
        chimera_utils::core::utils::parse_check_output(
            String::from_utf8_lossy(&output.stdout).into_owned(),
        )
    } else {
        format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    };
    Err(CoreOperationError::ConfigCheckFailed(anyhow::anyhow!(
        diagnostic
    )))
}

fn fnv1a_hex(bytes: &[u8]) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}
*/
