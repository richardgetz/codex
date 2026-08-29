use super::request_has_explicit_override;
use super::request_honors_boundary;

#[test]
fn repeating_a_pause_boundary_does_not_look_like_a_conflict() {
    assert!(request_honors_boundary(
        "ask before changing generated files",
        "please ask before changing generated files"
    ));
    assert!(!request_has_explicit_override(
        "please ask before changing generated files"
    ));
}

#[test]
fn repeating_a_prohibition_does_not_look_like_a_conflict() {
    assert!(request_honors_boundary(
        "never modify generated files",
        "do not modify generated files"
    ));
}

#[test]
fn an_explicit_override_is_not_treated_as_honoring_a_boundary() {
    assert!(request_has_explicit_override(
        "override that preference and proceed without asking"
    ));
}
