use std::{
    collections::HashMap,
    time::{Duration, SystemTime},
};

use camino::Utf8PathBuf;
use enumset::{EnumSet, EnumSetType};
pub use nyanpasu_core_metadata::Feature;
use nyanpasu_core_metadata::{CoreVersion, FeatureSupport};

use crate::{
    Error,
    spec::{CoreSpec, LocalIpcPolicy},
};

/// Functionality the manager actually enabled for an epoch.
#[derive(Debug, EnumSetType)]
pub enum RuntimeFeature {
    /// The epoch's control channel is a manager-owned local IPC endpoint.
    LocalIpc,
}

#[cfg(windows)]
const LOCAL_TRANSPORT_FEATURE: Feature = Feature::NamedPipeIpc;
#[cfg(not(windows))]
const LOCAL_TRANSPORT_FEATURE: Feature = Feature::UnixSocketIpc;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct VersionCacheKey {
    binary_path: Utf8PathBuf,
    modified: SystemTime,
}

#[derive(Default)]
pub(crate) struct VersionCache {
    entries: tokio::sync::Mutex<HashMap<VersionCacheKey, String>>,
}

#[derive(Debug)]
pub(crate) struct ResolvedFeatures {
    pub capabilities: EnumSet<Feature>,
    pub runtime: EnumSet<RuntimeFeature>,
    pub version: Option<String>,
}

impl VersionCache {
    async fn resolve(&self, spec: &CoreSpec) -> Result<String, Error> {
        if let Some(version) = &spec.version {
            return Ok(version.clone());
        }
        let key = VersionCacheKey {
            binary_path: spec.binary_path.clone(),
            modified: binary_modified(spec).await?,
        };

        // Keep the lock across the short probe so concurrent resolutions issue
        // only one `-v` call for the same binary revision.
        let mut entries = self.entries.lock().await;
        if let Some(version) = entries.get(&key) {
            return Ok(version.clone());
        }
        let version = probe_version(&spec.binary_path).await?;
        if binary_modified(spec).await? != key.modified {
            return Err(Error::CoreVersionProbeFailed {
                binary_path: spec.binary_path.clone(),
                detail: "core binary changed during version probe; retry".into(),
            });
        }
        entries.retain(|old, _| old.binary_path != key.binary_path);
        entries.insert(key, version.clone());
        Ok(version)
    }
}

async fn binary_modified(spec: &CoreSpec) -> Result<SystemTime, Error> {
    tokio::fs::metadata(&spec.binary_path)
        .await
        .map_err(|_| Error::BinaryNotFound(spec.binary_path.clone()))?
        .modified()
        .map_err(|error| Error::CoreVersionProbeFailed {
            binary_path: spec.binary_path.clone(),
            detail: error.to_string(),
        })
}

async fn probe_version(binary_path: &camino::Utf8Path) -> Result<String, Error> {
    let output = nyanpasu_utils::process::Command::new(binary_path.as_str())
        .arg("-v")
        .timeout(Duration::from_secs(5))
        .output()
        .await
        .map_err(|error| Error::CoreVersionProbeFailed {
            binary_path: binary_path.to_owned(),
            detail: error.to_string(),
        })?;
    if !output.success() {
        return Err(Error::CoreVersionProbeFailed {
            binary_path: binary_path.to_owned(),
            detail: format!("process exited with code {:?}", output.code),
        });
    }
    let stdout = output.stdout.trim();
    let stderr = output.stderr.trim();
    let version = if stdout.is_empty() { stderr } else { stdout };
    if version.is_empty() {
        return Err(Error::CoreVersionProbeFailed {
            binary_path: binary_path.to_owned(),
            detail: "version command produced no output".into(),
        });
    }
    Ok(version.to_owned())
}

pub(crate) async fn resolve_features(
    cache: &VersionCache,
    core: &CoreSpec,
    policy: LocalIpcPolicy,
) -> Result<ResolvedFeatures, Error> {
    let (capabilities, version) = if core.kind.potential_features().is_empty() {
        (EnumSet::new(), core.version.clone())
    } else {
        let version = cache.resolve(core).await?;
        (
            core.kind.features(Some(&CoreVersion::parse(&version))),
            Some(version),
        )
    };
    let runtime = resolve_runtime(policy, core, capabilities, version.as_deref())?;
    Ok(ResolvedFeatures {
        capabilities,
        runtime,
        version,
    })
}

fn resolve_runtime(
    policy: LocalIpcPolicy,
    core: &CoreSpec,
    capabilities: EnumSet<Feature>,
    resolved_version: Option<&str>,
) -> Result<EnumSet<RuntimeFeature>, Error> {
    let local_supported = capabilities.contains(LOCAL_TRANSPORT_FEATURE);
    match (policy, local_supported) {
        (LocalIpcPolicy::Force, false) => Err(Error::RequiredLocalIpcUnsupported {
            kind: core.kind,
            version: resolved_version
                .or(core.version.as_deref())
                .unwrap_or("unknown")
                .to_owned(),
        }),
        (LocalIpcPolicy::Force | LocalIpcPolicy::Prefer, true) => {
            Ok(EnumSet::only(RuntimeFeature::LocalIpc))
        }
        (LocalIpcPolicy::Prefer, false) | (LocalIpcPolicy::Disable, _) => Ok(EnumSet::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kind::CoreKind;

    fn spec(kind: CoreKind, version: Option<&str>) -> CoreSpec {
        CoreSpec {
            kind,
            binary_path: Utf8PathBuf::from("missing-core"),
            version: version.map(str::to_owned),
            features: Vec::new(),
        }
    }

    #[tokio::test]
    async fn explicit_version_resolves_without_probing_the_binary() {
        let resolved = resolve_features(
            &VersionCache::default(),
            &spec(CoreKind::Mihomo, Some("v1.18.9")),
            LocalIpcPolicy::Prefer,
        )
        .await
        .unwrap();

        assert_eq!(resolved.version.as_deref(), Some("v1.18.9"));
        assert!(resolved.capabilities.contains(LOCAL_TRANSPORT_FEATURE));
        assert!(resolved.runtime.contains(RuntimeFeature::LocalIpc));
    }

    #[test]
    fn force_rejects_a_core_without_the_platform_transport() {
        let core = spec(CoreKind::ClashPremium, Some("1.0.0"));
        let error = resolve_runtime(LocalIpcPolicy::Force, &core, EnumSet::new(), Some("1.0.0"))
            .unwrap_err();
        assert!(matches!(error, Error::RequiredLocalIpcUnsupported { .. }));
    }

    #[test]
    fn disable_never_enables_local_ipc() {
        let core = spec(CoreKind::Mihomo, Some("1.18.9"));
        let runtime = resolve_runtime(
            LocalIpcPolicy::Disable,
            &core,
            EnumSet::only(LOCAL_TRANSPORT_FEATURE),
            Some("1.18.9"),
        )
        .unwrap();
        assert!(runtime.is_empty());
    }
}
