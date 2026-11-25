//! Fork-only tests verifying self-updates stay disabled on the local fork.

use super::*;

#[cfg(unix)]
#[test]
fn fork_never_self_updates() {
    const CHILD: &str = "HERDR_TEST_FORK_NO_SELF_UPDATE";
    if std::env::var_os(CHILD).is_none() {
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "update::fork_tests::fork_never_self_updates", "--nocapture"])
            .env(CHILD, "1")
            .env("HERDR_RELEASES_URL", "https://example.invalid")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    assert!(checks_app().is_none());
    assert_eq!(status()["app"]["updates"], serde_json::json!(false));
}
