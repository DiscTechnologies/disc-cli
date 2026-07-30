use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

fn temporary_home(test_name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "disc-cli-integration-{test_name}-{}-{nonce}",
        std::process::id()
    ))
}

fn disc_command(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_disc"));
    command
        .env("HOME", home)
        .env_remove("DISC_API_KEY")
        .env_remove("DISC_HTTP_BASE_URL")
        .env_remove("DISC_WS_URL")
        .env_remove("DISC_CLIENT_ID");
    command
}

fn output_text(output: &Output) -> (String, String) {
    (
        String::from_utf8(output.stdout.clone()).expect("UTF-8 stdout"),
        String::from_utf8(output.stderr.clone()).expect("UTF-8 stderr"),
    )
}

#[test]
fn config_show_runs_through_the_binary_entry_point() {
    let home = temporary_home("config-show");
    fs::create_dir_all(&home).expect("create temporary home");

    let output = disc_command(&home)
        .args(["config", "show"])
        .output()
        .expect("run disc config show");
    let (stdout, stderr) = output_text(&output);

    assert!(output.status.success(), "stderr: {stderr}");
    assert!(stdout.contains("https://api.disc.tech (default)"));
    assert!(stdout.contains("wss://signals.disc.tech (default)"));
    assert!(stdout.contains("client_id:     (not set)"));

    fs::remove_dir_all(home).expect("remove temporary home");
}

#[test]
fn auth_api_key_can_be_stored_from_stdin_and_cleared() {
    let home = temporary_home("auth-stdin");
    fs::create_dir_all(&home).expect("create temporary home");

    let mut child = disc_command(&home)
        .args(["auth", "api-key", "set", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start API-key command");
    child
        .stdin
        .take()
        .expect("child stdin")
        .write_all(b"  integration-secret\n")
        .expect("write API key");
    let stored = child.wait_with_output().expect("wait for API-key command");
    let (stdout, stderr) = output_text(&stored);
    assert!(stored.status.success(), "stderr: {stderr}");
    assert!(stdout.contains("Stored API key"));

    let cleared = disc_command(&home)
        .args(["auth", "clear"])
        .output()
        .expect("run auth clear");
    let (stdout, stderr) = output_text(&cleared);
    assert!(cleared.status.success(), "stderr: {stderr}");
    assert!(stdout.contains("Cleared active Disc auth profile"));

    fs::remove_dir_all(home).expect("remove temporary home");
}

#[test]
fn command_errors_are_reported_with_a_failure_exit_code() {
    let home = temporary_home("failure");
    fs::create_dir_all(&home).expect("create temporary home");

    let output = disc_command(&home)
        .args(["signals", "passive", "list"])
        .output()
        .expect("run unauthenticated command");
    let (_, stderr) = output_text(&output);

    assert!(!output.status.success());
    assert!(stderr.contains("Authentication is not configured"));

    fs::remove_dir_all(home).expect("remove temporary home");
}

#[test]
fn legacy_typescript_cli_auth_flags_remain_parse_compatible() {
    let home = temporary_home("legacy-auth-flags");
    fs::create_dir_all(&home).expect("create temporary home");

    let output = disc_command(&home)
        .args([
            "auth",
            "login",
            "--api-url",
            "http://127.0.0.1:9",
            "--machine-label",
            "legacy-terminal",
            "--issuer",
            "http://127.0.0.1:9/realms/disc",
            "--no-browser",
        ])
        .output()
        .expect("run login with legacy flags");
    let (_, stderr) = output_text(&output);

    assert!(!output.status.success());
    assert!(!stderr.contains("unexpected argument"), "stderr: {stderr}");

    fs::remove_dir_all(home).expect("remove temporary home");
}
