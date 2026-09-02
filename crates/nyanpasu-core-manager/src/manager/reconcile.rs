use std::time::Duration;

use camino::Utf8Path;

use crate::{
    Error, ProbePhase, RuntimeInstance,
    config::mihomo::{ConfigChange, RuntimeProjection},
    spec::ResolvedController,
};

use super::Active;

pub(super) async fn reconcile_in_place(
    current: &Active,
    change: &ConfigChange,
    runtime_path: &Utf8Path,
    timeout: Duration,
) -> bool {
    match change {
        ConfigChange::Noop => return true,
        ConfigChange::Switch => return false,
        ConfigChange::Patch { patch, projection } => {
            if !patch_and_verify(current.instance.as_ref(), patch, projection, timeout).await {
                return false;
            }
        }
        ConfigChange::Reload => {
            let client = match build_client(current.instance.controller(), timeout) {
                Ok(client) => client,
                Err(error) => {
                    tracing::warn!("failed to build config control client: {error}");
                    return false;
                }
            };
            let request = clash_api::UpdateConfigRequest::from_path(runtime_path.to_string());
            if let Err(error) = client
                .update_config(&request, clash_api::UpdateConfigOptions { force: true })
                .await
            {
                tracing::warn!("config PUT failed: {error}");
                return false;
            }
        }
    }
    current
        .instance
        .probe_now(ProbePhase::Reconcile)
        .await
        .is_healthy()
}

pub(super) async fn patch_and_verify(
    instance: &dyn RuntimeInstance,
    patch: &clash_api::ConfigPatch,
    projection: &RuntimeProjection,
    timeout: Duration,
) -> bool {
    let client = match build_client(instance.controller(), timeout) {
        Ok(client) => client,
        Err(error) => {
            tracing::warn!("failed to build config control client: {error}");
            return false;
        }
    };
    if let Err(error) = client.patch_config(patch).await {
        // PATCH may have reached the core before the transport failed. GET is
        // authoritative, so verification decides whether we can keep running.
        tracing::warn!("config PATCH returned an uncertain result: {error}");
    }
    match client.get_config().await {
        Ok(runtime) => match projection.verify(&runtime) {
            Ok(true) => {}
            Ok(false) => return false,
            Err(error) => {
                tracing::warn!("failed to verify config projection: {error}");
                return false;
            }
        },
        Err(error) => {
            tracing::warn!("GET /configs verification failed: {error}");
            return false;
        }
    }
    true
}

fn build_client(
    controller: &ResolvedController,
    timeout: Duration,
) -> Result<clash_api::Client, Error> {
    let mut builder = clash_api::Client::builder(controller.host.clone()).timeout(timeout);
    if let Some(secret) = &controller.secret {
        builder = builder.secret(secret.as_str());
    }
    builder.build().map_err(Error::Api)
}
