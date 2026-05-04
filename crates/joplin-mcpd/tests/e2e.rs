use joplin_mcpd::observability::{request_context::new_test_id, victorialogs::VictoriaLogsHarness};
use std::time::Duration;

#[test]
#[ignore = "requires explicit e2e-test command"]
fn local_compose_declares_required_disposable_services() {
    let compose = include_str!("../../../tests/compose.local.yaml");

    for required in [
        "postgres:",
        "fake-joplin-auth:",
        "victorialogs:",
        "127.0.0.1:55432:5432",
        "127.0.0.1:58080:8080",
        "127.0.0.1:59428:9428",
    ] {
        assert!(compose.contains(required), "missing {required}");
    }
}

#[test]
#[ignore = "requires explicit e2e-test command"]
fn local_e2e_command_does_not_enable_live_secret_path() {
    let justfile = include_str!("../../../Justfile");

    assert!(justfile.contains("test-e2e:"));
    assert!(justfile.contains("cargo test -p joplin-mcpd --test e2e -- --ignored"));
    assert!(!command_body(justfile, "test-e2e:").contains("JP_MCP_LIVE_JOPLIN"));
    assert!(!command_body(justfile, "test-all-local:").contains("sops"));
}

#[test]
#[ignore = "requires explicit e2e-test command"]
fn victorialogs_harness_uses_test_id_queries() {
    let test_id = new_test_id();
    let harness = VictoriaLogsHarness::new("http://127.0.0.1:59428", Duration::from_secs(1))
        .expect("harness");
    let query = VictoriaLogsHarness::query_for_test_id(&test_id);

    assert_eq!(harness.base_url().as_str(), "http://127.0.0.1:59428/");
    assert!(query.contains(&test_id));
    assert!(query.starts_with(r#"{test.id=""#));
}

fn command_body<'a>(justfile: &'a str, command: &str) -> &'a str {
    let start = justfile.find(command).expect("command exists") + command.len();
    let rest = &justfile[start..];
    let end = rest
        .find("\n\n")
        .map_or(rest.len(), |separator| separator + 1);
    &rest[..end]
}
