//! Helpers for mode-keyed config maps.

use std::collections::HashMap;
use std::fmt;
use std::marker::PhantomData;

use codex_protocol::config_types::ModeKind;
use serde::Deserialize;
use serde::Deserializer;
use serde::de::Error;
use serde::de::IgnoredAny;
use serde::de::MapAccess;
use serde::de::Visitor;

const CURRENT_MODE_KEYS: &[&str] = &["default", "plan"];
const LEGACY_MODE_ALIASES: &[&str] = &[
    "code",
    "orchestrator",
    "pair_programming",
    "execute",
    "custom",
    "continuous",
];

pub(crate) fn deserialize_current_mode_config_map<'de, D, V>(
    deserializer: D,
) -> Result<HashMap<ModeKind, V>, D::Error>
where
    D: Deserializer<'de>,
    V: Deserialize<'de>,
{
    struct ModeConfigMapVisitor<V> {
        _value: PhantomData<fn() -> V>,
    }

    impl<'de, V> Visitor<'de> for ModeConfigMapVisitor<V>
    where
        V: Deserialize<'de>,
    {
        type Value = HashMap<ModeKind, V>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a map keyed by current collaboration mode names")
        }

        fn visit_map<M>(self, mut access: M) -> Result<Self::Value, M::Error>
        where
            M: MapAccess<'de>,
        {
            let mut modes = HashMap::with_capacity(access.size_hint().unwrap_or_default());
            while let Some(key) = access.next_key::<String>()? {
                match key.as_str() {
                    "default" => {
                        modes.insert(ModeKind::Default, access.next_value()?);
                    }
                    "plan" => {
                        modes.insert(ModeKind::Plan, access.next_value()?);
                    }
                    legacy if LEGACY_MODE_ALIASES.contains(&legacy) => {
                        access.next_value::<IgnoredAny>()?;
                    }
                    unknown => {
                        return Err(M::Error::unknown_field(unknown, CURRENT_MODE_KEYS));
                    }
                }
            }
            Ok(modes)
        }
    }

    deserializer.deserialize_map(ModeConfigMapVisitor {
        _value: PhantomData,
    })
}
