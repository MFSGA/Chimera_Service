use std::sync::LazyLock;

use super::{CoreVersion, Support};
use enumset::{EnumSet, EnumSetType};
use schemars::JsonSchema;
use semver::VersionReq;
use serde::{Deserialize, Serialize};
use specta::Type;

#[derive(Debug, EnumSetType, Type, JsonSchema, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Feature {
    /// Supports a Windows named pipe for IPC.
    NamedPipeIpc,
    /// Supports a Unix domain socket for IPC.
    UnixSocketIpc,
    /// Supports running without a TCP external controller.
    DisableTcpController,
}

pub trait FeatureSupport {
    fn supports(&self, feature: Feature, version: Option<&CoreVersion>) -> Support;

    fn features(&self, version: Option<&CoreVersion>) -> EnumSet<Feature> {
        EnumSet::all()
            .iter()
            .filter(|feature| matches!(self.supports(*feature, version), Support::Yes))
            .collect()
    }

    fn potential_features(&self) -> EnumSet<Feature> {
        EnumSet::all()
            .iter()
            .filter(|feature| !matches!(self.supports(*feature, None), Support::No))
            .collect()
    }
}

static MIHOMO_UNIX: LazyLock<VersionReq> =
    LazyLock::new(|| VersionReq::parse(">=1.18.4").expect("valid Mihomo unix feature floor"));
static MIHOMO_PIPE: LazyLock<VersionReq> =
    LazyLock::new(|| VersionReq::parse(">=1.18.9").expect("valid Mihomo pipe feature floor"));
static CLASH_RS_UNIX: LazyLock<VersionReq> =
    LazyLock::new(|| VersionReq::parse(">=0.9.1").expect("valid clash-rs unix feature floor"));
static CLASH_RS_PIPE: LazyLock<VersionReq> =
    LazyLock::new(|| VersionReq::parse(">=0.9.7").expect("valid clash-rs pipe feature floor"));

impl FeatureSupport for crate::kind::ClashCoreKind {
    fn supports(&self, feature: Feature, version: Option<&CoreVersion>) -> Support {
        match self {
            crate::kind::ClashCoreKind::Mihomo => match feature {
                Feature::NamedPipeIpc => since(&MIHOMO_PIPE, version),
                Feature::UnixSocketIpc => since(&MIHOMO_UNIX, version),
                Feature::DisableTcpController => Support::No,
            },
            crate::kind::ClashCoreKind::ClashRust => match feature {
                Feature::NamedPipeIpc => since(&CLASH_RS_PIPE, version),
                Feature::UnixSocketIpc => since(&CLASH_RS_UNIX, version),
                Feature::DisableTcpController => Support::No,
            },
            crate::kind::ClashCoreKind::ClashPremium | crate::kind::ClashCoreKind::Meow => {
                match feature {
                    Feature::NamedPipeIpc
                    | Feature::UnixSocketIpc
                    | Feature::DisableTcpController => Support::No,
                }
            }
        }
    }
}

fn since(req: &LazyLock<VersionReq>, version: Option<&CoreVersion>) -> Support {
    match version {
        Some(CoreVersion::Release(version)) if req.matches(version) => Support::Yes,
        Some(CoreVersion::Nightly) => Support::Yes,
        Some(CoreVersion::Release(_) | CoreVersion::Unknown) => Support::No,
        None => Support::Since((**req).clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ClashCoreKind;

    fn version(raw: &str) -> Option<CoreVersion> {
        Some(CoreVersion::parse(raw))
    }

    #[test]
    fn parses_release_banners_and_nightly_builds() {
        assert_eq!(
            CoreVersion::parse("Mihomo Meta v1.18.9 linux amd64"),
            CoreVersion::Release(semver::Version::new(1, 18, 9))
        );
        assert_eq!(CoreVersion::parse("alpha-deadbeef"), CoreVersion::Nightly);
        assert_eq!(CoreVersion::parse("unknown"), CoreVersion::Unknown);
    }

    #[test]
    fn transport_floors_bracket_the_first_supported_release() {
        for (floor, last_without, first_with) in [
            (&MIHOMO_UNIX, "1.18.3", "1.18.4"),
            (&MIHOMO_PIPE, "1.18.8", "1.18.9"),
            (&CLASH_RS_UNIX, "0.9.0", "0.9.1"),
            (&CLASH_RS_PIPE, "0.9.6", "0.9.7"),
        ] {
            assert!(matches!(since(floor, version(last_without).as_ref()), Support::No));
            assert!(matches!(since(floor, version(first_with).as_ref()), Support::Yes));
        }
    }

    #[test]
    fn unresolved_versions_return_requirements() {
        for kind in [ClashCoreKind::Mihomo, ClashCoreKind::ClashRust] {
            for feature in [Feature::NamedPipeIpc, Feature::UnixSocketIpc] {
                assert!(matches!(kind.supports(feature, None), Support::Since(_)));
            }
        }
    }

    #[test]
    fn nightly_builds_enable_version_gated_transports() {
        for kind in [ClashCoreKind::Mihomo, ClashCoreKind::ClashRust] {
            assert!(matches!(
                kind.supports(Feature::NamedPipeIpc, Some(&CoreVersion::Nightly)),
                Support::Yes
            ));
            assert!(matches!(
                kind.supports(Feature::UnixSocketIpc, Some(&CoreVersion::Nightly)),
                Support::Yes
            ));
        }
    }

    #[test]
    fn legacy_and_meow_cores_expose_no_local_ipc_features() {
        for kind in [ClashCoreKind::ClashPremium, ClashCoreKind::Meow] {
            assert!(kind.potential_features().is_empty());
            assert!(kind.features(version("99.0.0").as_ref()).is_empty());
        }
    }

    #[test]
    fn disable_tcp_controller_is_reserved_but_never_enabled() {
        for kind in [
            ClashCoreKind::Mihomo,
            ClashCoreKind::ClashRust,
            ClashCoreKind::ClashPremium,
            ClashCoreKind::Meow,
        ] {
            assert!(matches!(
                kind.supports(Feature::DisableTcpController, Some(&CoreVersion::Nightly)),
                Support::No
            ));
        }
    }
}
