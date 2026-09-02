use enumset::EnumSet;
use nyanpasu_core_metadata::Feature;

use crate::{
    Error, RuntimeFeature,
    capability::ResolvedFeatures,
    config::{
        ConfigSnapshot,
        mihomo::{self, RuntimeProjection},
        runtime_store::StagedRuntimeConfig,
    },
    spec::{InstanceSpec, ResolvedController},
    state::ConfigRevision,
};

use super::super::CoreManager;

pub(super) struct GracefulPlan {
    pub(super) source_spec: InstanceSpec,
    pub(super) effective_spec: InstanceSpec,
    pub(super) controller: ResolvedController,
    pub(super) revision: ConfigRevision,
    pub(super) capabilities: Vec<Feature>,
    pub(super) runtime_features: Vec<RuntimeFeature>,
    pub(super) source_document: serde_yaml_ng::Mapping,
    pub(super) effective_document: serde_yaml_ng::Mapping,
    pub(super) full_staged: StagedRuntimeConfig,
    pub(super) restoration: Option<(Box<clash_api::ConfigPatch>, RuntimeProjection)>,
}

impl CoreManager {
    pub(super) async fn prepare_graceful(
        &self,
        spec: InstanceSpec,
        epoch: u64,
        snapshot: &ConfigSnapshot,
        resolved: ResolvedFeatures,
    ) -> Result<GracefulPlan, Error> {
        let ResolvedFeatures {
            capabilities,
            runtime,
            version,
        } = resolved;
        let full = snapshot.prepare_full(
            self.inner.options.controller_template.as_deref(),
            self.inner.store.dir(),
            epoch,
            runtime,
        )?;
        let bootstrap = snapshot.prepare_bootstrap(
            self.inner.options.controller_template.as_deref(),
            self.inner.store.dir(),
            epoch,
            runtime,
        )?;
        if full.controller.host != bootstrap.controller.host
            || full.controller.secret != bootstrap.controller.secret
        {
            return Err(Error::InvalidConfig(
                "full and bootstrap configs resolved different controllers".into(),
            ));
        }
        let restoration = mihomo::restoration_patch(&bootstrap.document, &full.document)?;

        let full_staged = self.inner.store.stage(epoch, &full.bytes).await?;
        let mut check_spec = spec.clone();
        check_spec.config_path = full_staged.path().to_owned();
        self.inner.backend.check_config(&check_spec).await?;

        let bootstrap_staged = self.inner.store.stage(epoch, &bootstrap.bytes).await?;
        check_spec.config_path = bootstrap_staged.path().to_owned();
        self.inner.backend.check_config(&check_spec).await?;
        let runtime_path = self.inner.store.commit_new(bootstrap_staged, epoch).await?;

        let mut effective_spec = spec.clone();
        effective_spec.config_path = runtime_path.clone();
        effective_spec.pid_file = Some(self.inner.store.pid_path(epoch));
        if effective_spec.core.version.is_none() {
            effective_spec.core.version = version;
        }
        Ok(GracefulPlan {
            source_spec: spec,
            effective_spec,
            controller: full.controller,
            revision: ConfigRevision {
                epoch,
                generation: 1,
                source_hash: full.source_hash,
                effective_hash: full.effective_hash,
                runtime_path,
            },
            capabilities: enum_vec(capabilities),
            runtime_features: enum_vec(runtime),
            source_document: snapshot.document().clone(),
            effective_document: full.document,
            full_staged,
            restoration,
        })
    }
}

fn enum_vec<T>(set: EnumSet<T>) -> Vec<T>
where
    T: enumset::EnumSetType,
{
    set.iter().collect()
}
