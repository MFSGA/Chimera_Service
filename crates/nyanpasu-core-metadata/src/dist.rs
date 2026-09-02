//! Identity of a distributed core artifact: behavioral family plus open variant tags.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use specta::Type;

use crate::ClashCoreKind;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Type, Serialize, Deserialize)]
#[serde(from = "String")]
#[specta(transparent)]
pub struct VariantTag(String);

impl VariantTag {
    pub const GROUP_CHANNEL: &'static str = "channel";

    pub fn new(tag: impl AsRef<str>) -> Self {
        Self(tag.as_ref().trim().to_ascii_lowercase())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn group(&self) -> Option<&str> {
        self.0.split_once(':').map(|(group, _)| group)
    }

    pub fn value(&self) -> &str {
        self.0
            .split_once(':')
            .map_or(self.0.as_str(), |(_, value)| value)
    }
}

impl From<String> for VariantTag {
    fn from(tag: String) -> Self {
        Self::new(tag)
    }
}

impl std::fmt::Display for VariantTag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Type, Serialize, Deserialize)]
pub struct CoreDistribution {
    pub kind: ClashCoreKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub variant: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    #[specta(optional)]
    pub tags: BTreeSet<VariantTag>,
}

impl CoreDistribution {
    pub fn new(kind: ClashCoreKind) -> Self {
        Self {
            kind,
            variant: None,
            tags: BTreeSet::new(),
        }
    }

    pub fn tag_value(&self, group: &str) -> Option<&str> {
        self.tags
            .iter()
            .find(|tag| tag.group() == Some(group))
            .map(VariantTag::value)
    }

    pub fn channel(&self) -> Option<&str> {
        self.tag_value(VariantTag::GROUP_CHANNEL)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag(value: &str) -> VariantTag {
        VariantTag::new(value)
    }

    #[test]
    fn tags_normalize_and_split_on_only_the_first_colon() {
        let tag = tag("  Future:One:Two  ");
        assert_eq!(tag.as_str(), "future:one:two");
        assert_eq!(tag.group(), Some("future"));
        assert_eq!(tag.value(), "one:two");
        assert_eq!(
            serde_json::from_str::<VariantTag>(r#"" Channel:ALPHA ""#).unwrap(),
            VariantTag::new("channel:alpha")
        );
    }

    #[test]
    fn unknown_tags_roundtrip_and_deduplicate() {
        let decoded = serde_json::from_str::<BTreeSet<VariantTag>>(
            r#"["vendor:zeta","future:neon","vendor:zeta"]"#,
        )
        .unwrap();
        assert_eq!(decoded.len(), 2);
        assert_eq!(
            serde_json::to_string(&decoded).unwrap(),
            r#"["future:neon","vendor:zeta"]"#
        );
    }

    #[test]
    fn distribution_omits_unknown_provenance_and_queries_open_groups() {
        let plain = CoreDistribution::new(ClashCoreKind::Mihomo);
        assert_eq!(
            serde_json::to_string(&plain).unwrap(),
            r#"{"kind":"mihomo"}"#
        );

        let distribution = CoreDistribution {
            kind: ClashCoreKind::Mihomo,
            variant: Some("alpha-goamd64-v3".into()),
            tags: BTreeSet::from([tag("channel:alpha"), tag("goamd64:v3"), tag("portable")]),
        };
        assert_eq!(distribution.channel(), Some("alpha"));
        assert_eq!(distribution.tag_value("goamd64"), Some("v3"));
        assert_eq!(distribution.tag_value("missing"), None);
        assert_eq!(
            serde_json::from_str::<CoreDistribution>(
                &serde_json::to_string(&distribution).unwrap()
            )
            .unwrap(),
            distribution
        );
    }
}
