use super::*;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;

#[test]
fn state_db_path_preserves_shipped_state_6_database() {
    let sqlite_home = std::env::temp_dir().join("codex-state-db-path-test");
    let sqlite = SqliteConfig::new_for_testing(sqlite_home.as_path().abs());

    assert_eq!(sqlite.state_db_path(), sqlite_home.join("state_6.sqlite"));
}
