#![cfg(feature = "server")]

//! Tests for the `mentisdbd` headless daemon binary's argument parsing and
//! delegation surface. The TUI, update-dialog, and first-run interactive
//! flows were removed when the daemon was refactored to be headless; tests
//! for those subsystems were removed along with the code.

use std::ffi::OsString;

#[allow(dead_code)]
#[path = "../src/bin/mentisdbd.rs"]
mod mentisdbd_impl;

#[test]
fn help_text_lists_all_supported_invocations() {
    let help = mentisdbd_impl::daemon_help_text();

    // Top-level daemon modes
    assert!(help.contains("mentisdbd"));
    assert!(help.contains("--mode stdio"));
    assert!(help.contains("--mode http"));
    assert!(help.contains("--mode both"));
    assert!(help.contains("--stdio-mcp"));
    assert!(help.contains("--help"));

    // Every CLI subcommand should be discoverable from the daemon help
    for sub in [
        "setup", "wizard", "add", "search", "agents", "backup", "restore",
    ] {
        assert!(
            help.contains(sub),
            "daemon help is missing subcommand '{sub}': {help}"
        );
    }
}

#[test]
fn no_args_starts_default_http_mode() {
    let mode = mentisdbd_impl::parse_daemon_args(std::iter::empty::<OsString>()).unwrap();
    assert_eq!(mode, mentisdbd_impl::DaemonArgMode::Run);
}

#[test]
fn help_flags_return_help_mode() {
    for flag in ["--help", "-h", "help"] {
        let mode = mentisdbd_impl::parse_daemon_args([OsString::from(flag)]).unwrap();
        assert_eq!(
            mode,
            mentisdbd_impl::DaemonArgMode::Help,
            "flag '{flag}' should parse as Help"
        );
    }
}

#[test]
fn mode_flag_selects_daemon_transport() {
    assert_eq!(
        mentisdbd_impl::parse_daemon_args([OsString::from("--mode"), OsString::from("stdio")])
            .unwrap(),
        mentisdbd_impl::DaemonArgMode::Stdio,
    );
    assert_eq!(
        mentisdbd_impl::parse_daemon_args([OsString::from("--mode"), OsString::from("http")])
            .unwrap(),
        mentisdbd_impl::DaemonArgMode::Run,
    );
    assert_eq!(
        mentisdbd_impl::parse_daemon_args([OsString::from("--mode"), OsString::from("both")])
            .unwrap(),
        mentisdbd_impl::DaemonArgMode::Both,
    );
}

#[test]
fn mode_flag_rejects_unknown_values() {
    let err = mentisdbd_impl::parse_daemon_args([
        OsString::from("--mode"),
        OsString::from("telegram"),
    ])
    .unwrap_err();
    assert!(err.contains("Invalid --mode value"));
}

#[test]
fn mode_flag_requires_a_value() {
    let err =
        mentisdbd_impl::parse_daemon_args([OsString::from("--mode")]).unwrap_err();
    assert!(err.contains("requires a value"));
}

#[test]
fn stdio_mcp_flag_is_alias_for_mode_stdio() {
    assert_eq!(
        mentisdbd_impl::parse_daemon_args([OsString::from("--stdio-mcp")]).unwrap(),
        mentisdbd_impl::DaemonArgMode::Stdio,
    );
}

#[test]
fn cli_subcommands_are_delegated_unchanged() {
    for sub in [
        "setup", "wizard", "add", "search", "agents", "backup", "restore",
    ] {
        let mode = mentisdbd_impl::parse_daemon_args([OsString::from(sub)]).unwrap();
        match mode {
            mentisdbd_impl::DaemonArgMode::CliSubcommand(args) => {
                assert_eq!(args[0], OsString::from("mentisdbd"));
                assert_eq!(args[1], OsString::from(sub));
            }
            other => panic!("subcommand '{sub}' should parse as CliSubcommand, got {other:?}"),
        }
    }
}

#[test]
fn cli_subcommand_preserves_all_trailing_arguments() {
    let argv: Vec<OsString> = [
        "add",
        "Hello world",
        "--type",
        "Insight",
        "--tag",
        "poc",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();

    match mentisdbd_impl::parse_daemon_args(argv).unwrap() {
        mentisdbd_impl::DaemonArgMode::CliSubcommand(args) => {
            assert_eq!(args[0], OsString::from("mentisdbd"));
            assert_eq!(args[1], OsString::from("add"));
            assert_eq!(args[2], OsString::from("Hello world"));
            assert_eq!(args[3], OsString::from("--type"));
            assert_eq!(args[4], OsString::from("Insight"));
            assert_eq!(args[5], OsString::from("--tag"));
            assert_eq!(args[6], OsString::from("poc"));
        }
        other => panic!("expected CliSubcommand, got {other:?}"),
    }
}

#[test]
fn unknown_arguments_are_rejected() {
    let err =
        mentisdbd_impl::parse_daemon_args([OsString::from("--frobnicate")]).unwrap_err();
    assert!(err.contains("Unexpected arguments"));
}
