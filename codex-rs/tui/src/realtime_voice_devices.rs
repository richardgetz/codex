use std::collections::BTreeMap;

pub(crate) fn is_reserved_device_alias(alias: &str) -> bool {
    alias.eq_ignore_ascii_case("help") || alias == "?"
}

pub(crate) fn normalize_device_alias(alias: &str) -> Option<String> {
    let alias = alias.trim();
    if alias.is_empty() || alias.chars().any(char::is_whitespace) || is_reserved_device_alias(alias)
    {
        return None;
    }
    Some(alias.to_ascii_lowercase())
}

pub(crate) fn resolve_device_name(
    requested: &str,
    devices: &[String],
    aliases: &BTreeMap<String, String>,
) -> Option<String> {
    let requested = requested.trim();
    devices
        .iter()
        .find(|device| device.eq_ignore_ascii_case(requested))
        .cloned()
        .or_else(|| {
            aliases
                .iter()
                .find(|(alias, _)| {
                    !is_reserved_device_alias(alias) && alias.eq_ignore_ascii_case(requested)
                })
                .and_then(|(_, target)| {
                    devices
                        .iter()
                        .find(|device| device.eq_ignore_ascii_case(target))
                        .cloned()
                })
        })
}

pub(crate) fn display_device_name(device: &str, aliases: &BTreeMap<String, String>) -> String {
    let aliases = aliases
        .iter()
        .filter(|(alias, target)| {
            !is_reserved_device_alias(alias) && target.eq_ignore_ascii_case(device)
        })
        .map(|(alias, _)| alias.as_str())
        .collect::<Vec<_>>();
    if aliases.is_empty() {
        device.to_string()
    } else {
        format!("{device} ({})", aliases.join(", "))
    }
}

pub(crate) fn format_device_aliases(aliases: &BTreeMap<String, String>) -> String {
    aliases
        .iter()
        .filter(|(alias, _)| !is_reserved_device_alias(alias))
        .map(|(alias, target)| format!("{alias} -> {target}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
#[path = "realtime_voice_devices_tests.rs"]
mod tests;
