use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Typed subset of Mihomo's running `GET /configs` response used by Chimera.
///
/// Fields are optional/defaulted so older or forked controllers can omit
/// capabilities without making read-back verification fail to deserialize.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[serde(rename = "ClashRuntimeConfig", rename_all = "kebab-case")]
pub struct RuntimeConfig {
    pub port: Option<i64>,
    pub mode: Option<String>,
    pub ipv6: Option<bool>,
    pub socket_port: Option<i64>,
    pub allow_lan: Option<bool>,
    pub log_level: Option<String>,
    pub mixed_port: Option<i64>,
    pub redir_port: Option<i64>,
    pub socks_port: Option<i64>,
    pub tproxy_port: Option<i64>,
    pub external_controller: Option<String>,
    pub secret: Option<String>,
    #[cfg_attr(feature = "specta", specta(skip))]
    pub bind_address: Option<String>,
    #[cfg_attr(feature = "specta", specta(skip))]
    pub sniffing: Option<bool>,
    #[cfg_attr(feature = "specta", specta(skip))]
    pub tcp_concurrent: Option<bool>,
    #[cfg_attr(feature = "specta", specta(skip))]
    pub find_process_mode: Option<String>,
    #[cfg_attr(feature = "specta", specta(skip))]
    pub interface_name: Option<String>,
    #[cfg_attr(feature = "specta", specta(skip))]
    pub tun: Option<RuntimeTun>,
    #[cfg_attr(feature = "specta", specta(skip))]
    pub tuic_server: Option<RuntimeTuicServer>,
    #[cfg_attr(feature = "specta", specta(skip))]
    pub ss_config: Option<String>,
    #[cfg_attr(feature = "specta", specta(skip))]
    pub vmess_config: Option<String>,
    #[cfg_attr(feature = "specta", specta(skip))]
    pub tcptun_config: Option<String>,
    #[cfg_attr(feature = "specta", specta(skip))]
    pub udptun_config: Option<String>,
    #[cfg_attr(feature = "specta", specta(skip))]
    pub authentication: Option<Vec<String>>,
    #[cfg_attr(feature = "specta", specta(skip))]
    pub skip_auth_prefixes: Option<Vec<String>>,
    #[cfg_attr(feature = "specta", specta(skip))]
    pub lan_allowed_ips: Option<Vec<String>>,
    #[cfg_attr(feature = "specta", specta(skip))]
    pub lan_disallowed_ips: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct RuntimeTun {
    pub enable: bool,
    pub device: String,
    pub stack: String,
    pub dns_hijack: Vec<String>,
    pub auto_route: bool,
    pub auto_detect_interface: bool,
    pub mtu: u32,
    pub gso: bool,
    pub gso_max_size: u32,
    pub inet4_address: Vec<String>,
    pub inet6_address: Vec<String>,
    pub iproute2_table_index: i64,
    pub iproute2_rule_index: i64,
    pub auto_redirect: bool,
    pub auto_redirect_input_mark: u32,
    pub auto_redirect_output_mark: u32,
    pub auto_redirect_iproute2_fallback_rule_index: i64,
    pub loopback_address: Vec<String>,
    pub strict_route: bool,
    pub route_address: Vec<String>,
    pub route_address_set: Vec<String>,
    pub route_exclude_address: Vec<String>,
    pub route_exclude_address_set: Vec<String>,
    pub include_interface: Vec<String>,
    pub exclude_interface: Vec<String>,
    pub include_uid: Vec<u32>,
    pub include_uid_range: Vec<String>,
    pub exclude_uid: Vec<u32>,
    pub exclude_uid_range: Vec<String>,
    pub include_android_user: Vec<i64>,
    pub include_package: Vec<String>,
    pub exclude_package: Vec<String>,
    pub include_mac_address: Vec<String>,
    pub exclude_mac_address: Vec<String>,
    pub endpoint_independent_nat: bool,
    pub udp_timeout: i64,
    pub icmp_timeout: i64,
    pub file_descriptor: i64,
    pub inet4_route_address: Vec<String>,
    pub inet6_route_address: Vec<String>,
    pub inet4_route_exclude_address: Vec<String>,
    pub inet6_route_exclude_address: Vec<String>,
    pub recvmsgx: bool,
    pub sendmsgx: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct RuntimeTuicServer {
    pub enable: bool,
    pub listen: String,
    pub token: Vec<String>,
    pub users: BTreeMap<String, String>,
    pub certificate: String,
    pub private_key: String,
    pub congestion_controller: String,
    pub max_idle_time: i64,
    pub authentication_timeout: i64,
    pub alpn: Vec<String>,
    pub max_udp_relay_packet_size: i64,
    pub cwnd: i64,
    pub bbr_profile: String,
}

/// Typed subset of Mihomo's PATCH /configs body that Chimera can currently
/// verify against RuntimeConfig. Unsupported ref fields deliberately fall back
/// to Reload/Restart until their runtime projection is modeled here.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct ConfigPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub socks_port: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redir_port: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tproxy_port: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mixed_port: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tun: Option<TunPatch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tuic_server: Option<TuicServerPatch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ss_config: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vmess_config: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tcptun_config: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub udptun_config: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_lan: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip_auth_prefixes: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lan_allowed_ips: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lan_disallowed_ips: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bind_address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_level: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ipv6: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sniffing: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tcp_concurrent: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub find_process_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interface_name: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct TunPatch {
    pub enable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dns_hijack: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_route: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_detect_interface: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mtu: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gso: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gso_max_size: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inet6_address: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iproute2_table_index: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iproute2_rule_index: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_redirect: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_redirect_input_mark: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_redirect_output_mark: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_redirect_iproute2_fallback_rule_index: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loopback_address: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strict_route: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route_address: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route_address_set: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route_exclude_address: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route_exclude_address_set: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_interface: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclude_interface: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_uid: Option<Vec<u32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_uid_range: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclude_uid: Option<Vec<u32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclude_uid_range: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_android_user: Option<Vec<i64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_package: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclude_package: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_mac_address: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclude_mac_address: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint_independent_nat: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub udp_timeout: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icmp_timeout: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_descriptor: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inet4_route_address: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inet6_route_address: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inet4_route_exclude_address: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inet6_route_exclude_address: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recvmsgx: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sendmsgx: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct TuicServerPatch {
    pub enable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub listen: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub users: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub certificate: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub private_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub congestion_controller: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_idle_time: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authentication_timeout: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alpn: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_udp_relay_packet_size: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwnd: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bbr_profile: Option<String>,
}

/// Field-scoped expectation derived from a typed patch.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeProjection {
    expected: Vec<(Vec<String>, serde_json::Value)>,
}

impl RuntimeProjection {
    pub fn from_patch(patch: &ConfigPatch) -> Result<Self, serde_json::Error> {
        Self::from_serializable(patch)
    }

    pub fn from_serializable<T>(patch: &T) -> Result<Self, serde_json::Error>
    where
        T: Serialize + ?Sized,
    {
        let value = serde_json::to_value(patch)?;
        let mut expected = Vec::new();
        collect_leaves(&value, &mut Vec::new(), &mut expected);
        Ok(Self { expected })
    }

    pub fn verify(&self, actual: &RuntimeConfig) -> Result<bool, serde_json::Error> {
        let actual = serde_json::to_value(actual)?;
        Ok(self.expected.iter().all(|(path, expected)| {
            value_at(&actual, path).is_some_and(|actual| actual == expected)
        }))
    }
}

fn collect_leaves(
    value: &serde_json::Value,
    path: &mut Vec<String>,
    output: &mut Vec<(Vec<String>, serde_json::Value)>,
) {
    match value {
        serde_json::Value::Object(mapping) => {
            for (key, value) in mapping {
                path.push(key.clone());
                collect_leaves(value, path, output);
                path.pop();
            }
        }
        _ => output.push((path.clone(), value.clone())),
    }
}

fn value_at<'a>(value: &'a serde_json::Value, path: &[String]) -> Option<&'a serde_json::Value> {
    path.iter().try_fold(value, |value, key| value.get(key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projection_verifies_only_fields_carried_by_patch() {
        let patch = ConfigPatch {
            allow_lan: Some(true),
            mode: Some("rule".into()),
            ..ConfigPatch::default()
        };
        let projection = RuntimeProjection::from_patch(&patch).unwrap();
        let mut actual = RuntimeConfig {
            allow_lan: Some(true),
            mode: Some("rule".into()),
            mixed_port: Some(7890),
            ..RuntimeConfig::default()
        };
        assert!(projection.verify(&actual).unwrap());

        actual.mode = Some("global".into());
        assert!(!projection.verify(&actual).unwrap());
    }

    #[test]
    fn tun_projection_requires_enable_and_changed_nested_fields() {
        let patch = ConfigPatch {
            tun: Some(TunPatch {
                enable: true,
                stack: Some("mixed".into()),
                auto_route: Some(true),
                include_interface: Some(vec!["Ethernet".into()]),
                inet4_route_exclude_address: Some(vec!["192.168.0.0/16".into()]),
                ..TunPatch::default()
            }),
            ..ConfigPatch::default()
        };
        let projection = RuntimeProjection::from_patch(&patch).unwrap();
        let actual = RuntimeConfig {
            tun: Some(RuntimeTun {
                enable: true,
                stack: "mixed".into(),
                auto_route: true,
                include_interface: vec!["Ethernet".into()],
                inet4_route_exclude_address: vec!["192.168.0.0/16".into()],
                ..RuntimeTun::default()
            }),
            ..RuntimeConfig::default()
        };
        assert!(projection.verify(&actual).unwrap());
    }

    #[test]
    fn extended_patch_surface_verifies_tuic_and_access_ranges() {
        let patch = ConfigPatch {
            tuic_server: Some(TuicServerPatch {
                enable: true,
                listen: Some("127.0.0.1:10443".into()),
                alpn: Some(vec!["h3".into()]),
                ..TuicServerPatch::default()
            }),
            skip_auth_prefixes: Some(vec!["127.0.0.0/8".into()]),
            lan_allowed_ips: Some(vec!["192.168.0.0/16".into()]),
            ss_config: Some("ss://runtime".into()),
            ..ConfigPatch::default()
        };
        let projection = RuntimeProjection::from_patch(&patch).unwrap();
        let actual = RuntimeConfig {
            tuic_server: Some(RuntimeTuicServer {
                enable: true,
                listen: "127.0.0.1:10443".into(),
                alpn: vec!["h3".into()],
                ..RuntimeTuicServer::default()
            }),
            skip_auth_prefixes: Some(vec!["127.0.0.0/8".into()]),
            lan_allowed_ips: Some(vec!["192.168.0.0/16".into()]),
            ss_config: Some("ss://runtime".into()),
            ..RuntimeConfig::default()
        };
        assert!(projection.verify(&actual).unwrap());
    }
}
