/* Legacy core manager retired; retained temporarily for incremental deletion.
        let config_bytes = tokio::fs::read(&config_path).await?;
        let config_hash = fnv1a_hex(&config_bytes);
        let revision = ConfigRevisionInfo {
            epoch: self.next_epoch.fetch_add(1, Ordering::Relaxed),
            generation: 1,
            source_hash: config_hash.clone(),
            effective_hash: config_hash,
        };
        let infos = consts::RuntimeInfos::global();
        let app_dir = infos.nyanpasu_data_dir.clone();
        let binary_path = find_binary_path(core_type)?;
        let pid_path = crate::utils::dirs::service_core_pid_file();
        let app_dir = Utf8PathBuf::from_path_buf(app_dir)
            .map_err(|_| anyhow::anyhow!("failed to convert app_dir to Utf8PathBuf"))?;
        let binary_path = Utf8PathBuf::from_path_buf(binary_path)
            .map_err(|_| anyhow::anyhow!("failed to convert binary_path to Utf8PathBuf"))?;
        let pid_path = Utf8PathBuf::from_path_buf(pid_path)
            .map_err(|_| anyhow::anyhow!("failed to convert pid_path to Utf8PathBuf"))?;
        tracing::info!(
            core_type = ?core_type,
            app_dir = %app_dir,
            binary_path = %binary_path,
            pid_path = %pid_path,
            config_path = %config_path,
            "Starting Core"
        );
        let cancel_token = self.cancel_token.child_token();
        let instance = CoreInstanceBuilder::default()
            .core_type(core_type.clone())
            .app_dir(app_dir)
            .binary_path(binary_path)
            .config_path(config_path.clone())
            .pid_path(pid_path)
            .build()?;
        let instance = Arc::new(instance);

        // start the core instance
        let state_changed_at = self.state_changed_at.clone();
        let cancel_token_clone = cancel_token.clone();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<anyhow::Result<()>>(1); // use mpsc channel just to avoid type moved error, though it never fails
        let service = self.clone();
        let state_changed_notify = self.state_changed_notify.clone();
        let instance_clone = instance.clone();
        let tracker = TaskTracker::new();
        tracker.spawn(async move {
            match instance_clone.run().await {
                Ok((_, mut rx)) => {
                    let mut err_buf: Vec<String> = Vec::with_capacity(6);
                    let mut break_loop = false;

                    while let Some(event) = rx.recv().await {
                        Self::handle_command_event(
                            &mut break_loop,
                            &mut err_buf,
                            &state_changed_at,
                            &state_changed_notify,
                            &tx,
                            &cancel_token_clone,
                            service.clone(),
                            event,
                        )
                        .await;
                        if break_loop {
                            break;
                        }
                    }
                }
                Err(err) => {
                    spawn(async move {
                        tx.send(Err(err.into())).await.unwrap();
                    });
                }
            }
        });
        // Create a task to check cancel token called
        let cancel_token_clone = cancel_token.clone();
        let service = self.clone();
        tracker.spawn(async move {
            cancel_token_clone.cancelled().await;
            if service.manager.try_lock().is_ok() {
                let _ = service.stop().await;
            }
        });
        tracker.close();
        rx.recv().await.unwrap()?;
        drop(rx);
        Self::notify_state_changed(self.state_changed_notify.clone(), CoreState::Running);
        *manager = Some(CoreManager {
            instance,
            config_path: config_path.to_path_buf(),
            cancel_token,
            tracker: Some(tracker),
            revision,
        });
        Ok(())
    }

    pub async fn check(
        &self,
        core_type: &CoreType,
        config_path: &Utf8Path,
    ) -> Result<(), CoreOperationError> {
        let _slot = self
            .check_slots
            .try_acquire()
            .map_err(|_| CoreOperationError::CheckBusy)?;
        let config_path = config_path
            .canonicalize_utf8()
            .map_err(|error| CoreOperationError::ConfigNotFound(error.into()))?;
        tokio::fs::metadata(&config_path)
            .await
            .map_err(|error| CoreOperationError::ConfigNotFound(error.into()))?;
        let infos = consts::RuntimeInfos::global();
        let app_dir = Utf8PathBuf::from_path_buf(infos.nyanpasu_data_dir.clone()).map_err(|_| {
            CoreOperationError::ConfigCheckFailed(anyhow::anyhow!(
                "failed to convert app directory to UTF-8"
            ))
        })?;
        let binary_path = find_binary_path(core_type)
            .map_err(|error| CoreOperationError::BinaryNotFound(error.into()))?;
        let binary_path = Utf8PathBuf::from_path_buf(binary_path).map_err(|_| {
            CoreOperationError::BinaryNotFound(anyhow::anyhow!(
                "failed to convert core binary path to UTF-8"
            ))
        })?;
        run_config_check(core_type, &config_path, &binary_path, &app_dir).await
    }

    pub async fn apply(
        &self,
        core_type: &CoreType,
        config_path: &Utf8Path,
        expected_revision: Option<&RevisionIdInfo>,
    ) -> Result<CoreApplyData, CoreOperationError> {
        let (old_core_type, old_config_path, old_revision) = {
            let manager = self.manager.lock().await;
            let manager = manager.as_ref().ok_or(CoreOperationError::NotStarted)?;
            if !matches!(Self::state_(Some(manager)).as_ref(), CoreState::Running) {
                return Err(CoreOperationError::NotStarted);
            }
            if let Some(expected) = expected_revision
                && &manager.revision.id() != expected
            {
                return Err(CoreOperationError::RevisionConflict);
            }
            (
                manager.instance.core_type.clone(),
                manager.config_path.clone(),
                manager.revision.clone(),
            )
        };

        self.check(core_type, config_path).await?;
        let canonical_config = config_path
            .canonicalize_utf8()
            .map_err(|error| CoreOperationError::ConfigNotFound(error.into()))?;
        let desired_hash = tokio::fs::read(&canonical_config)
            .await
            .map(|bytes| fnv1a_hex(&bytes))
            .map_err(|error| CoreOperationError::ConfigNotFound(error.into()))?;
        if *core_type == old_core_type && desired_hash == old_revision.source_hash {
            return Ok(CoreApplyData {
                outcome: ApplyOutcomeKind::Noop,
                revision: old_revision,
                warning: None,
                failed_apply: None,
            });
        }

        self.stop()
            .await
            .map_err(CoreOperationError::ApplyFailed)?;
        match self.start(core_type, &canonical_config).await {
            Ok(()) => {
                let revision = self.status().await.revision.ok_or_else(|| {
                    CoreOperationError::ApplyFailed(anyhow::anyhow!(
                        "the restarted core published no revision"
                    ))
                })?;
                Ok(CoreApplyData {
                    outcome: ApplyOutcomeKind::Restarted,
                    revision,
                    warning: Some(
                        "legacy manager fallback performed a full process restart".to_string(),
                    ),
                    failed_apply: None,
                })
            }
            Err(apply_error) => match self.start(&old_core_type, &old_config_path).await {
                Ok(()) => {
                    let revision = self.status().await.revision.ok_or_else(|| {
                        CoreOperationError::ApplyFailed(anyhow::anyhow!(
                            "rollback succeeded but published no revision"
                        ))
                    })?;
                    Ok(CoreApplyData {
                        outcome: ApplyOutcomeKind::RolledBack,
                        revision,
                        warning: Some(
                            "legacy manager fallback restored the previous process".to_string(),
                        ),
                        failed_apply: Some(apply_error.to_string()),
                    })
                }
                Err(rollback_error) => Err(CoreOperationError::ApplyFailed(anyhow::anyhow!(
                    "desired config failed: {apply_error}; rollback failed: {rollback_error}"
                ))),
            },
        }
    }

    pub async fn recover(&self) -> Result<(), CoreOperationError> {
        // The legacy manager has no quarantine latch. Idempotent success keeps
        // the endpoint safe while preserving the S8 contract.
        Ok(())
    }

    pub async fn restart(&self) -> Result<(), anyhow::Error> {
        let (core_type, config_path, is_running) = {
            let manager = self.manager.lock().await;
            let manager = manager
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("core have not been started yet"))?;
            (
                manager.instance.core_type.clone(),
                manager.config_path.clone(),
                matches!(Self::state_(Some(manager)).as_ref(), CoreState::Running),
            )
        };
        if is_running {
            self.stop().await?;
        }
        self.start(&core_type, config_path.as_path()).await
    }

    pub async fn stop(&self) -> Result<(), anyhow::Error> {
        let mut manager = self.manager.lock().await;
        let state = Self::state_(manager.as_ref());
        if matches!(state.as_ref(), CoreState::Stopped(_)) {
            anyhow::bail!("core is already stopped");
        }

        if let Some(manager) = manager.as_mut() {
            manager.cancel_token.cancel();
            manager.instance.kill().await?;
            if let Some(tracker) = manager.tracker.take() {
                tracker.wait().await;
            }
        }

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
