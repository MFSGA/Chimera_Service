//! Immutable YAML snapshots and deterministic controller preparation.

use camino::{Utf8Path, Utf8PathBuf};
use enumset::EnumSet;
use serde_yaml_ng::{Mapping, Value};

use crate::{
    Error, RuntimeFeature,
    spec::ResolvedController,
};

const EXTERNAL_CONTROLLER: &str = "external-controller";
const EXTERNAL_CONTROLLER_PIPE: &str = "external-controller-pipe";
const EXTERNAL_CONTROLLER_UNIX: &str = "external-controller-unix";
const SECRET: &str = "secret";

#[derive(Debug, Clone)]
pub(crate) struct ConfigSnapshot {
    source_path: Utf8PathBuf,
    document: Mapping,
    source_hash: String,
}

#[derive(Debug, Clone)]
pub(crate) struct PreparedConfig {
    pub bytes: Vec<u8>,
    pub controller: ResolvedController,
    pub source_hash: String,
    pub effective_hash: String,
}

impl ConfigSnapshot {
    pub(crate) async fn load(source_path: &Utf8Path) -> Result<Self, Error> {
        let raw = tokio::fs::read(source_path).await?;
        Self::from_bytes(source_path.to_owned(), &raw)
    }

    fn from_bytes(source_path: Utf8PathBuf, raw: &[u8]) -> Result<Self, Error> {
        let value: Value = serde_yaml_ng::from_slice(raw)?;
        let Value::Mapping(document) = canonicalize(value)? else {
            return Err(Error::InvalidConfig(
                "top-level YAML document must be a mapping".into(),
            ));
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

    pub(crate) fn prepare(
        &self,
        controller_template: Option<&str>,
        runtime_dir: &Utf8Path,
        epoch: u64,
        runtime: EnumSet<RuntimeFeature>,
    ) -> Result<PreparedConfig, Error> {
        let mut document = self.document.clone();
        let local = runtime.contains(RuntimeFeature::LocalIpc);
        if local {
            rewrite_managed_controller(
                &mut document,
                managed_endpoint_path(runtime_dir, controller_template, epoch)?,
            );
        }
        let Value::Mapping(document) = canonicalize(Value::Mapping(document))? else {
            unreachable!("canonical mapping stays a mapping")
        };
        let controller = resolve_controller(&document, local)?;
        let bytes = serialize_mapping(&document)?;
        Ok(PreparedConfig {
            effective_hash: semantic_hash(&bytes),
            source_hash: self.source_hash.clone(),
            bytes,
            controller,
        })
    }
}

fn resolve_controller(document: &Mapping, allow_local: bool) -> Result<ResolvedController, Error> {
    let secret = str_value(document, SECRET);
    let host = if allow_local {
        #[cfg(windows)]
        let local = str_value(document, EXTERNAL_CONTROLLER_PIPE).map(clash_api::Host::named_pipe);
        #[cfg(not(windows))]
        let local = str_value(document, EXTERNAL_CONTROLLER_UNIX).map(clash_api::Host::unix_socket);
        local.or_else(|| {
            str_value(document, EXTERNAL_CONTROLLER)
                .and_then(|address| clash_api::Host::http(probe_address(&address)).ok())
        })
    } else {
        str_value(document, EXTERNAL_CONTROLLER)
            .and_then(|address| clash_api::Host::http(probe_address(&address)).ok())
    }
    .ok_or(Error::ControllerMissing)?;
    Ok(ResolvedController { host, secret })
}

fn rewrite_managed_controller(document: &mut Mapping, endpoint: String) {
    for field in [
        EXTERNAL_CONTROLLER,
        EXTERNAL_CONTROLLER_PIPE,
        EXTERNAL_CONTROLLER_UNIX,
    ] {
        document.remove(Value::String(field.to_owned()));
    }
    #[cfg(windows)]
    let field = EXTERNAL_CONTROLLER_PIPE;
    #[cfg(not(windows))]
    let field = EXTERNAL_CONTROLLER_UNIX;
    document.insert(Value::String(field.to_owned()), Value::String(endpoint));
}

fn managed_endpoint_path(
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
        #[cfg(not(windows))]
        {
            let endpoint = Utf8Path::new(&endpoint);
            return Ok(if endpoint.is_absolute() {
                endpoint.to_string()
            } else {
                runtime_dir.join(endpoint).to_string()
            });
        }
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
            .prepare(None, Utf8Path::new("runtime"), 1, runtime)
            .unwrap();
        let second = second
            .prepare(None, Utf8Path::new("runtime"), 1, runtime)
            .unwrap();
        assert_eq!(first.source_hash, second.source_hash);
        assert_eq!(first.effective_hash, second.effective_hash);
        assert_eq!(first.bytes, second.bytes);
    }

    #[test]
    fn wildcard_http_controller_is_probed_through_loopback() {
        let prepared = snapshot("external-controller: 0.0.0.0:9090\n")
            .prepare(None, Utf8Path::new("runtime"), 2, EnumSet::new())
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
            .prepare(
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
        assert!(matches!(prepared.controller.host, clash_api::Host::NamedPipe(_)));
        #[cfg(not(windows))]
        assert!(matches!(prepared.controller.host, clash_api::Host::UnixSocket(_)));
    }

    #[test]
    fn source_path_is_retained_for_revision_identity() {
        let snapshot = snapshot("external-controller: 127.0.0.1:9090\n");
        assert_eq!(snapshot.source_path(), Utf8Path::new("source.yaml"));
    }
}
