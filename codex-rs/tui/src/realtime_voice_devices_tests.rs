use super::display_device_name;
use super::format_device_aliases;
use super::is_reserved_device_alias;
use super::normalize_device_alias;
use super::resolve_device_name;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;

#[test]
fn normalizes_aliases_for_case_insensitive_commands() {
    assert_eq!(
        normalize_device_alias(" AirPods "),
        Some("airpods".to_string())
    );
    assert_eq!(normalize_device_alias("desk speakers"), None);
}

#[test]
fn reserves_help_aliases_for_command_help() {
    assert!(is_reserved_device_alias("help"));
    assert!(is_reserved_device_alias("HELP"));
    assert!(is_reserved_device_alias("?"));
    assert_eq!(normalize_device_alias("help"), None);
    assert_eq!(normalize_device_alias("?"), None);
}

#[test]
fn resolves_aliases_and_full_device_names_case_insensitively() {
    let devices = vec!["AirPods Pro".to_string(), "Built-in Microphone".to_string()];
    let aliases = BTreeMap::from([("airpods".to_string(), "AirPods Pro".to_string())]);

    assert_eq!(
        resolve_device_name("AIRPODS", &devices, &aliases),
        Some("AirPods Pro".to_string())
    );
    assert_eq!(
        resolve_device_name("built-in microphone", &devices, &aliases),
        Some("Built-in Microphone".to_string())
    );
    assert_eq!(resolve_device_name("missing", &devices, &aliases), None);
    let reserved_aliases = BTreeMap::from([("help".to_string(), "AirPods Pro".to_string())]);
    assert_eq!(
        resolve_device_name("help", &devices, &reserved_aliases),
        None
    );
}

#[test]
fn includes_aliases_in_display_and_listing() {
    let aliases = BTreeMap::from([
        ("airpods".to_string(), "AirPods Pro".to_string()),
        ("portable".to_string(), "AirPods Pro".to_string()),
    ]);

    assert_eq!(
        display_device_name("AirPods Pro", &aliases),
        "AirPods Pro (airpods, portable)"
    );
    assert_eq!(
        format_device_aliases(&aliases),
        "airpods -> AirPods Pro\nportable -> AirPods Pro"
    );
}
