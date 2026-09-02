//! Immutable YAML snapshots and deterministic controller preparation.

mod clash;
mod diff;
pub(crate) mod mihomo;
pub mod runtime_store;

pub(crate) use clash::LOCAL_TRANSPORT_FEATURE;

use camino::{Utf8Path, Utf8PathBuf};
use enumset::EnumSet;
use serde_yaml_ng::{Mapping, Value};

use crate::{Error, RuntimeFeature, spec::ResolvedController};

#[derive(Debug, Clone)]
pub(crate) struct ConfigSnapshot {
    source_path: Utf8PathBuf,
    document: Mapping,
    source_hash: String,
}

#[derive(Debug, Clone)]
pub(crate) struct PreparedConfig {
    pub bytes: Vec<u8>,
    pub document: Mapping,
    pub controller: ResolvedController,
    pub rewrote_controller: bool,
    pub source_hash: String,
    pub effective_hash: String,
}

#[derive(Debug)]
pub(crate) struct ConfigInfo {
    pub controller: Option<RawController>,
    pub secret: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RawController {
    Pipe(String),
    #[cfg_attr(windows, allow(dead_code))]
    Unix(String),
    Http(String),
}

impl ConfigSnapshot {
    pub(crate) async fn load(source_path: &Utf8Path) -> Result<Self, Error> {
        let raw = tokio::fs::read(source_path).await?;
        Self::from_bytes(source_path.to_owned(), &raw)
    }

    fn from_bytes(source_path: Utf8PathBuf, raw: &[u8]) -> Result<Self, Error> {
        let value: Value = serde_yaml_ng::from_slice(raw)?;
        let Value::Mapping(document) = value else {
            return Err(Error::InvalidConfig(
                "top-level YAML document must be a mapping".into(),
            ));
        };
        Self::from_document(source_path, document)
    }

    pub(crate) fn from_document(
        source_path: Utf8PathBuf,
        document: Mapping,
    ) -> Result<Self, Error> {
        let Value::Mapping(document) = canonicalize(Value::Mapping(document))? else {
            unreachable!("canonical mapping stays a mapping")
        };
        let bytes = serialize_mapping(&document)?;
        Ok(Self {
            source_path,
            document,
            source_hash: semantic_hash(&bytes),
        })
    }

    pub(crate) fn source_path(&self) -> &Utf8Path {
        &self.source_path
    }

    pub(crate) fn document(&self) -> &Mapping {
        &self.document
    }

    #[cfg(test)]
    pub(crate) fn info(&self) -> ConfigInfo {
        clash::inspect(&self.document)
    }

    pub(crate) fn prepare_full(
        &self,
        controller_template: Option<&str>,
        runtime_dir: &Utf8Path,
        epoch: u64,
        runtime: EnumSet<RuntimeFeature>,
    ) -> Result<PreparedConfig, Error> {
        self.prepare_inner(controller_template, runtime_dir, epoch, runtime, false)
    }

    pub(crate) fn prepare_bootstrap(
        &self,
        controller_template: Option<&str>,
        runtime_dir: &Utf8Path,
        epoch: u64,
        runtime: EnumSet<RuntimeFeature>,
    ) -> Result<PreparedConfig, Error> {
        self.prepare_inner(controller_template, runtime_dir, epoch, runtime, true)
    }

    fn prepare_inner(
        &self,
        controller_template: Option<&str>,
        runtime_dir: &Utf8Path,
        epoch: u64,
        runtime: EnumSet<RuntimeFeature>,
        zero_inbounds: bool,
    ) -> Result<PreparedConfig, Error> {
        let mut document = self.document.clone();
        if zero_inbounds {
            mihomo::zero_inbounds(&mut document);
        }
        let rewrote_controller = runtime.contains(RuntimeFeature::LocalIpc);
        if rewrote_controller {
            clash::rewrite_managed_controller(
                &mut document,
                managed_endpoint_path(runtime_dir, controller_template, epoch)?,
            );
        }
        let Value::Mapping(document) = canonicalize(Value::Mapping(document))? else {
            unreachable!("canonical mapping stays a mapping")
        };
        let info = if rewrote_controller {
            clash::inspect(&document)
        } else {
            clash::inspect_http(&document)
        };
        let controller = resolve_controller(&info)?;
        let bytes = serialize_mapping(&document)?;
        Ok(PreparedConfig {
            effective_hash: semantic_hash(&bytes),
            source_hash: self.source_hash.clone(),
            bytes,
            document,
            controller,
            rewrote_controller,
        })
    }
}

pub(crate) fn resolve_controller(info: &ConfigInfo) -> Result<ResolvedController, Error> {
    let raw = info.controller.as_ref().ok_or(Error::ControllerMissing)?;
    let host = match raw {
        RawController::Pipe(path) => clash_api::Host::named_pipe(path),
        RawController::Unix(path) => clash_api::Host::unix_socket(path),
        RawController::Http(address) => clash_api::Host::http(probe_address(address))?,
    };
    Ok(ResolvedController {
        host,
        secret: info.secret.clone(),
    })
}

pub(crate) fn managed_endpoint_path(
    runtime_dir: &Utf8Path,
    template: Option<&str>,
    epoch: u64,
) -> Result<String, Error> {
    if template.is_some_and(|value| !value.contains("{epoch}")) {
        return Err(Error::InvalidManagerOptions(
            "controller_template must contain `{epoch}`".into(),
        ));
    }
    if let Some(template) = template {
        let endpoint = template.replace("{epoch}", &epoch.to_string());
        #[cfg(windows)]
        return Ok(endpoint);
        #[cfg(unix)]
        return managed_unix_endpoint(runtime_dir, &endpoint);
    }
    #[cfg(windows)]
    {
        let _ = runtime_dir;
        Ok(format!(r"\\.\pipe\nyanpasu\core-{epoch}"))
    }
    #[cfg(not(windows))]
    {
        Ok(runtime_dir.join(format!("core-{epoch}.sock")).to_string())
    }
}

#[cfg(unix)]
fn managed_unix_endpoint(runtime_dir: &Utf8Path, endpoint: &str) -> Result<String, Error> {
    let endpoint = Utf8Path::new(endpoint);
    let candidate = if endpoint.is_absolute() {
        endpoint.to_owned()
    } else {
        runtime_dir.join(endpoint)
    };
    let parent = candidate.parent().ok_or_else(|| {
        Error::InvalidManagerOptions("managed Unix controller has no parent directory".into())
    })?;
    let canonical_parent = std::fs::canonicalize(parent).map_err(|error| {
        Error::InvalidManagerOptions(format!(
            "managed Unix controller parent `{parent}` cannot be canonicalized: {error}"
        ))
    })?;
    let canonical_parent = Utf8PathBuf::from_path_buf(canonical_parent).map_err(|_| {
        Error::InvalidManagerOptions("managed Unix controller path is not UTF-8".into())
    })?;
    if !canonical_parent.starts_with(runtime_dir) {
        return Err(Error::InvalidManagerOptions(format!(
            "managed Unix controller `{candidate}` escapes runtime directory `{runtime_dir}`"
        )));
    }
    let file_name = candidate.file_name().ok_or_else(|| {
        Error::InvalidManagerOptions("managed Unix controller must name a socket file".into())
    })?;
    Ok(canonical_parent.join(file_name).to_string())
}

fn canonicalize(value: Value) -> Result<Value, Error> {
    match value {
        Value::Mapping(mapping) => {
            let mut entries = Vec::with_capacity(mapping.len());
            for (key, value) in mapping {
                let Value::String(key) = key else {
                    return Err(Error::InvalidConfig(
                        "all YAML mapping keys must be strings".into(),
                    ));
                };
                entries.push((key, canonicalize(value)?));
            }
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            let mut canonical = Mapping::new();
            for (key, value) in entries {
                canonical.insert(Value::String(key), value);
            }
            Ok(Value::Mapping(canonical))
        }
        Value::Sequence(sequence) => sequence
            .into_iter()
            .map(canonicalize)
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Sequence),
        Value::Tagged(_) => Err(Error::InvalidConfig(
            "tagged YAML values are not supported".into(),
        )),
        scalar => Ok(scalar),
    }
}

fn serialize_mapping(document: &Mapping) -> Result<Vec<u8>, Error> {
    Ok(serde_yaml_ng::to_string(document)?.into_bytes())
}

fn semantic_hash(bytes: &[u8]) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn str_value(document: &Mapping, key: &str) -> Option<String> {
    document
        .get(Value::String(key.to_owned()))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|value| !value.is_empty())
}

fn probe_address(address: &str) -> String {
    match address.rsplit_once(':') {
        Some(("0.0.0.0" | "::" | "[::]" | "", port)) => format!("127.0.0.1:{port}"),
        _ => address.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(raw: &str) -> ConfigSnapshot {
        ConfigSnapshot::from_bytes("source.yaml".into(), raw.as_bytes()).unwrap()
    }

    #[test]
    fn canonical_hash_ignores_mapping_order() {
        let first = snapshot("secret: token\nexternal-controller: 127.0.0.1:9090\n");
        let second = snapshot("external-controller: 127.0.0.1:9090\nsecret: token\n");
        let runtime = EnumSet::new();
        let first = first
            .prepare_full(None, Utf8Path::new("runtime"), 1, runtime)
            .unwrap();
        let second = second
            .prepare_full(None, Utf8Path::new("runtime"), 1, runtime)
            .unwrap();
        assert_eq!(first.source_hash, second.source_hash);
        assert_eq!(first.effective_hash, second.effective_hash);
        assert_eq!(first.bytes, second.bytes);
    }

    #[test]
    fn wildcard_http_controller_is_probed_through_loopback() {
        let prepared = snapshot("external-controller: 0.0.0.0:9090\n")
            .prepare_full(None, Utf8Path::new("runtime"), 2, EnumSet::new())
            .unwrap();
        let clash_api::Host::Http(url) = prepared.controller.host else {
            panic!("expected HTTP controller");
        };
        assert_eq!(url.as_str(), "http://127.0.0.1:9090/");
    }

    #[test]
    fn managed_controller_is_epoch_scoped_and_replaces_http() {
        let runtime = EnumSet::only(RuntimeFeature::LocalIpc);
        let prepared = snapshot("external-controller: 127.0.0.1:9090\nsecret: token\n")
            .prepare_full(
                Some("managed-{epoch}.sock"),
                Utf8Path::new("runtime"),
                7,
                runtime,
            )
            .unwrap();
        let text = String::from_utf8(prepared.bytes).unwrap();
        assert!(!text.contains("external-controller: 127.0.0.1:9090"));
        assert!(text.contains("managed-7.sock"));
        assert_eq!(prepared.controller.secret.as_deref(), Some("token"));
        #[cfg(windows)]
        assert!(matches!(
            prepared.controller.host,
            clash_api::Host::NamedPipe(_)
        ));
        #[cfg(not(windows))]
        assert!(matches!(
            prepared.controller.host,
            clash_api::Host::UnixSocket(_)
        ));
    }

    #[test]
    fn extracts_http_controller_and_secret() {
        let info = snapshot("external-controller: 127.0.0.1:9090\nsecret: s3cret\n").info();
        assert_eq!(
            info.controller,
            Some(RawController::Http("127.0.0.1:9090".into()))
        );
        assert_eq!(info.secret.as_deref(), Some("s3cret"));
    }

    #[test]
    fn http_mode_ignores_a_user_declared_local_controller() {
        #[cfg(windows)]
        let source = snapshot(r"external-controller-pipe: \\.\pipe\source");
        #[cfg(not(windows))]
        let source = snapshot("external-controller-unix: /tmp/source.sock");

        let error = source
            .prepare_full(None, Utf8Path::new("runtime"), 1, EnumSet::new())
            .unwrap_err();
        assert!(matches!(error, Error::ControllerMissing));
    }

    #[test]
    fn source_path_is_retained_for_revision_identity() {
        let snapshot = snapshot("external-controller: 127.0.0.1:9090\n");
        assert_eq!(snapshot.source_path(), Utf8Path::new("source.yaml"));
    }
}
