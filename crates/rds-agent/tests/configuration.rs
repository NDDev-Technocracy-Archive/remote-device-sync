const BINARY: &str = env!("CARGO_BIN_EXE_rds-agent");
const TAIL: &[&str] = &[];
include!("../../../tests/support/endpoint_cli.rs");

#[test]
fn invalid_ssh_destination_fails_before_identity_or_bind() {
    for target in [":22", "host:0", "host:nope", "::1:22", "[::1]22"] {
        let dir = Scratch::new();
        let output = run(&["--no-relay", "--ssh", target], &dir);
        assert!(!output.status.success());
        assert!(!dir.0.join("endpoint.key").exists(), "{target}");
    }
}

#[test]
fn invalid_admission_budget_fails_before_identity_or_bind() {
    for flag in ["--max-connections", "--max-streams"] {
        for value in ["0", "65536", "-1", "unlimited"] {
            let dir = Scratch::new();
            let output = run(&["--no-relay", flag, value], &dir);
            assert!(!output.status.success());
            assert!(!dir.0.join("endpoint.key").exists(), "{flag} {value}");
        }
    }
}

#[test]
fn grant_mode_requires_control_capacity_before_identity_or_bind() {
    let dir = Scratch::new();
    let issuer = "ab".repeat(32);
    let output = run(
        &["--no-relay", "--issuer", &issuer, "--max-streams", "1"],
        &dir,
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--max-streams at least 2"));
    assert!(!dir.0.join("endpoint.key").exists());
}

fn run_agent_config(dir: &Scratch, json: &str, args: &[&str]) -> std::process::Output {
    let path = dir.0.join("agent.json");
    std::fs::write(&path, json).unwrap();
    let mut full = vec!["--no-relay", "--agent-config"];
    full.push(path.to_str().unwrap());
    full.extend_from_slice(args);
    run(&full, dir)
}

#[test]
fn agent_config_rejects_bad_schema_before_identity_or_bind() {
    for json in [
        r#"{"schema_version":2}"#,
        r#"{"schema_version":1,"mystery":true}"#,
        r#"{"schema_version":1,"role":"sync","services":["tcp"]}"#,
        r#"{"schema_version":1,"services":["audio"]}"#,
        r#"{"schema_version":1,"services":["sync"]}"#, // no sync_dir
        r#"{"schema_version":1,"timeouts":{"hello_secs":0}}"#,
        r#"{"schema_version":1,"authority":{"grant_ttl_secs":0}}"#,
        r#"{"schema_version":1,"authority":{"revocations":{"key":"ab"}}}"#,
        r#"{"schema_version":1,"service":{"ssh_target":"host:0"}}"#,
        r#"{"schema_version":1,"peers":{"allow":["not-an-id"]}}"#,
    ] {
        let dir = Scratch::new();
        let output = run_agent_config(&dir, json, &[]);
        assert!(!output.status.success(), "{json}");
        assert!(!dir.0.join("endpoint.key").exists(), "{json}");
    }
}

#[test]
fn service_flags_gate_before_identity_or_bind() {
    // `audio` is reserved in every build; `sync` without a directory and
    // out-of-bounds timeouts fail identically from flags or file.
    for args in [
        &["--service", "audio"][..],
        &["--service", "sync"][..],
        &["--no-relay", "--role", "sync"][..],
        &["--no-relay", "--hello-timeout", "0"][..],
        &["--no-relay", "--handshake-timeout", "7200"][..],
    ] {
        let dir = Scratch::new();
        let output = run(args, &dir);
        assert!(!output.status.success(), "{args:?}");
        assert!(!dir.0.join("endpoint.key").exists(), "{args:?}");
    }
}

#[test]
fn flag_role_overrides_file_services() {
    // The file enables tcp; the flag role reselects sync, which then fails
    // its sync_dir prerequisite — proving the flag replaced the file set.
    let dir = Scratch::new();
    let output = run_agent_config(
        &dir,
        r#"{"schema_version":1,"services":["tcp"]}"#,
        &["--role", "sync"],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("sync"));
    assert!(!dir.0.join("endpoint.key").exists());
}

#[test]
fn role_and_service_flags_conflict() {
    let output = run(
        &["--no-relay", "--role", "access", "--service", "tcp"],
        &Scratch::new(),
    );
    assert!(!output.status.success());
}
