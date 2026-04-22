//! Headless MentisDb daemon.
//!
//! Runs the MCP server (HTTP + optional HTTPS) and REST surface. All operator
//! output goes to stdout/stderr via `env_logger`. No TUI.
//!
//! Configuration is read from environment variables:
//!
//! - `MENTISDB_DIR`
//! - `MENTISDB_DEFAULT_CHAIN_KEY` (deprecated alias: `MENTISDB_DEFAULT_KEY`)
//! - `MENTISDB_STORAGE_ADAPTER`
//! - `MENTISDB_AUTO_FLUSH` (defaults to `true`)
//! - `MENTISDB_VERBOSE` (defaults to `true`)
//! - `MENTISDB_LOG_FILE`
//! - `MENTISDB_BIND_HOST`
//! - `MENTISDB_MCP_PORT`
//! - `MENTISDB_REST_PORT`
//! - `MENTISDB_HTTPS_MCP_PORT` (set to 0 to disable; default 9473)
//! - `MENTISDB_HTTPS_REST_PORT` (set to 0 to disable; default 9474)
//! - `MENTISDB_DASHBOARD_PORT` (set to 0 to disable; default 9475)
//! - `MENTISDB_TLS_CERT` / `MENTISDB_TLS_KEY`
//! - `MENTISDB_UPDATE_CHECK` (default `true`; logs a warning when a newer release is available)
//! - `MENTISDB_UPDATE_REPO` (default `CloudLLM-ai/mentisdb`)
//! - `RUST_LOG`

use env_logger::Env;
use mcp::ToolProtocol;
use mentisdb::server::{
    adopt_legacy_default_mentisdb_dir, start_servers, MentisDbMcpProtocol, MentisDbServerConfig,
    MentisDbService,
};
use mentisdb::{
    migrate_chain_hash_algorithm, migrate_registered_chains_with_adapter, migrate_skill_registry,
    refresh_registered_chain_counts, MentisDbMigrationEvent,
};
use serde::Deserialize;
use std::ffi::OsString;
use std::io::{self, BufRead, Write};
use std::process::ExitCode;
use std::process::{Command, Stdio};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Embedded MentisDB skill markdown for stdio-proxy resource reads.
const MENTISDB_SKILL_MD: &str = include_str!("../../MENTISDB_SKILL.md");

pub(crate) const DEFAULT_UPDATE_REPO: &str = "CloudLLM-ai/mentisdb";
const GITHUB_API_BASE: &str = "https://api.github.com";
const UPDATE_CRATE_NAME: &str = "mentisdb";

// ── File descriptors ────────────────────────────────────────────────────────

/// Raise the open-file-descriptor limit to the OS hard cap (up to 65 535).
#[cfg(unix)]
fn raise_fd_limit() {
    // SAFETY: `rlimit` is a plain C struct; zero-initialization is valid.
    // `RLIMIT_NOFILE` is a standard POSIX constant always defined on Unix.
    // `getrlimit` and `setrlimit` are pure C FFI calls with no lifetime
    // requirements beyond the `rlimit` pointer being valid during the call.
    // The target value is bounded to 65_535, well within kernel limits.
    unsafe {
        let mut rlim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut rlim) != 0 {
            return;
        }
        let target = rlim.rlim_max.min(65_535);
        if rlim.rlim_cur < target {
            rlim.rlim_cur = target;
            libc::setrlimit(libc::RLIMIT_NOFILE, &rlim);
        }
    }
}

#[cfg(not(unix))]
fn raise_fd_limit() {}

// ── Logging ─────────────────────────────────────────────────────────────────

fn init_logger() {
    let mut builder = env_logger::Builder::from_env(Env::default().default_filter_or("info"));
    builder.format_timestamp_millis();
    let _ = builder.try_init();
}

// ── Env helpers ─────────────────────────────────────────────────────────────

fn env_var_truthy(name: &str, default: bool) -> bool {
    match std::env::var(name) {
        Ok(value) => match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => true,
            "0" | "false" | "no" | "off" => false,
            _ => default,
        },
        Err(_) => default,
    }
}

// ── Update check (log-only, non-interactive) ────────────────────────────────

#[derive(Debug, Deserialize)]
struct GitHubReleaseResponse {
    tag_name: String,
    html_url: String,
}

fn latest_release_tag(repo: &str) -> Option<GitHubReleaseResponse> {
    let url = format!("{GITHUB_API_BASE}/repos/{repo}/releases/latest");
    ureq::get(&url)
        .set(
            "User-Agent",
            &format!("mentisdbd/{}", env!("CARGO_PKG_VERSION")),
        )
        .timeout(std::time::Duration::from_secs(4))
        .call()
        .ok()?
        .into_json()
        .ok()
}

fn check_for_updates_noninteractive() {
    if !env_var_truthy("MENTISDB_UPDATE_CHECK", true) {
        return;
    }
    let repo = std::env::var("MENTISDB_UPDATE_REPO")
        .unwrap_or_else(|_| DEFAULT_UPDATE_REPO.to_string());
    let Some(resp) = latest_release_tag(&repo) else {
        log::debug!("update check: could not reach GitHub");
        return;
    };
    let current = env!("CARGO_PKG_VERSION");
    let latest = resp.tag_name.trim_start_matches('v');
    if latest == current {
        log::debug!("{UPDATE_CRATE_NAME} is up to date at {current}");
    } else {
        log::warn!(
            "{UPDATE_CRATE_NAME} update available: running {current}, latest release is {latest} ({})",
            resp.html_url
        );
    }
}

// ── Migrations (log each event) ─────────────────────────────────────────────

fn run_migrations(chain_dir: &std::path::Path, adapter: mentisdb::StorageAdapterKind) {
    let log_event = |event: MentisDbMigrationEvent| {
        log::info!("migration: {event:?}");
    };

    if let Err(e) = migrate_chain_hash_algorithm(chain_dir, log_event) {
        log::warn!("migrate_chain_hash_algorithm failed: {e}");
    }
    match migrate_registered_chains_with_adapter(chain_dir, adapter, log_event) {
        Ok(reports) => {
            for r in &reports {
                log::info!(
                    "migrated chain {} v{}→v{} ({} thoughts)",
                    r.chain_key,
                    r.from_version,
                    r.to_version,
                    r.thought_count
                );
            }
        }
        Err(e) => log::warn!("migrate_registered_chains_with_adapter failed: {e}"),
    }
    if let Err(e) = migrate_skill_registry(chain_dir) {
        log::warn!("migrate_skill_registry failed: {e}");
    }
    if let Err(e) = refresh_registered_chain_counts(chain_dir) {
        log::warn!("refresh_registered_chain_counts failed: {e}");
    }
}

// ── Stdio mode machinery ────────────────────────────────────────────────────

fn is_daemon_running(mcp_addr: &str) -> bool {
    let url = format!("http://{mcp_addr}/health");
    ureq::get(&url)
        .timeout(std::time::Duration::from_millis(500))
        .call()
        .map(|r| r.status() == 200)
        .unwrap_or(false)
}

/// Launch the daemon in the background, detached from the current process.
fn launch_daemon() -> Result<(), String> {
    let exe = std::env::current_exe()
        .map_err(|e| format!("could not resolve own executable path: {e}"))?;

    #[cfg(unix)]
    {
        let status = Command::new("nohup")
            .arg(&exe)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("failed to spawn daemon: {e}"))?;
        drop(status);
    }

    #[cfg(windows)]
    {
        let exe_str = exe.to_string_lossy();
        let status = Command::new("cmd")
            .args(["/C", "start", "/B", &exe_str])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("failed to spawn daemon: {e}"))?;
        drop(status);
    }

    Ok(())
}

fn wait_for_daemon(mcp_addr: &str, max_attempts: u32) -> bool {
    for _ in 0..max_attempts {
        if is_daemon_running(mcp_addr) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    false
}

/// Forward a single JSON-RPC request to the daemon's streamable HTTP MCP endpoint.
async fn proxy_jsonrpc_to_daemon(mcp_addr: &str, request: &str) -> Option<String> {
    let url = format!("http://{mcp_addr}/");

    let parsed: serde_json::Value = serde_json::from_str(request).ok()?;
    let method = parsed.get("method")?.as_str()?;
    if method == "resources/read" {
        let id = parsed.get("id").cloned()?;
        let params = parsed
            .get("params")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let uri = params.get("uri").and_then(|v| v.as_str()).unwrap_or("");
        return Some(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "contents": [{
                        "uri": uri,
                        "text": MENTISDB_SKILL_MD,
                        "mimeType": "text/markdown"
                    }]
                }
            })
            .to_string(),
        );
    }

    let request_bytes = request.as_bytes().to_vec();
    #[allow(clippy::result_large_err)]
    let resp = tokio::task::spawn_blocking(move || {
        ureq::post(&url)
            .set("Content-Type", "application/json")
            .set("Accept", "application/json, text/event-stream")
            .send(&*request_bytes)
    })
    .await
    .ok()?;

    let resp = resp.ok()?;
    let resp_body: String = resp.into_string().ok()?;

    if resp_body.starts_with("event:") || resp_body.contains("event:") {
        for line in resp_body.lines() {
            let line = line.trim();
            if let Some(data) = line.strip_prefix("data: ") {
                if data.starts_with('{') {
                    return Some(data.to_string());
                }
            }
        }
        return None;
    }

    Some(resp_body)
}

async fn run_stdio_mode(
    config: MentisDbServerConfig,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    init_logger();
    let mcp_addr = config.mcp_addr.to_string();

    if is_daemon_running(&mcp_addr) {
        log::info!("stdio: daemon detected at {mcp_addr}, proxying to live instance");
    } else {
        log::info!("stdio: no daemon running, launching background daemon");
        launch_daemon().map_err(|e| format!("failed to launch daemon: {e}"))?;
        if wait_for_daemon(&mcp_addr, 50) {
            log::info!("stdio: daemon started at {mcp_addr}, proxying");
        } else {
            log::warn!("stdio: daemon did not become responsive at {mcp_addr}, falling back to local mode");
            run_stdio_mode_local(config).await?;
            return Ok(());
        }
    }

    run_stdio_mode_proxy(&mcp_addr).await
}

async fn run_stdio_mode_local(
    config: MentisDbServerConfig,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let service = Arc::new(MentisDbService::new(config.service.clone()));
    let protocol = MentisDbMcpProtocol::new(service);

    let stdin = BufReader::new(tokio::io::stdin());
    let mut stdout = tokio::io::stdout();

    let mut lines = stdin.lines();
    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        if let Some(resp_json) = handle_stdio_jsonrpc(line, &protocol).await {
            stdout.write_all(resp_json.as_bytes()).await?;
            stdout.write_all(b"\n").await?;
            stdout.flush().await?;
        }
    }

    Ok(())
}

async fn run_stdio_mode_proxy(
    mcp_addr: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mcp_addr = mcp_addr.to_string();
    let stdin = BufReader::new(tokio::io::stdin());
    let mut stdout = tokio::io::stdout();

    let mut lines = stdin.lines();
    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        if let Some(resp_json) = proxy_jsonrpc_to_daemon(&mcp_addr, line).await {
            stdout.write_all(resp_json.as_bytes()).await?;
            stdout.write_all(b"\n").await?;
            stdout.flush().await?;
        }
    }

    Ok(())
}

async fn handle_stdio_jsonrpc(line: &str, protocol: &MentisDbMcpProtocol) -> Option<String> {
    let request: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return None,
    };

    let jsonrpc = request.get("jsonrpc")?.as_str()?;
    if jsonrpc != "2.0" {
        return None;
    }

    let id = request.get("id").cloned()?;
    let method = request.get("method")?.as_str()?;
    let params = request
        .get("params")
        .cloned()
        .unwrap_or(serde_json::Value::Null);

    let result: Result<serde_json::Value, String> = match method {
        "initialize" => Ok(serde_json::json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {
                "tools": {"listChanged": false},
                "resources": {"subscribe": false, "listChanged": false}
            },
            "serverInfo": {
                "name": "mentisdb",
                "version": env!("CARGO_PKG_VERSION")
            },
            "instructions": "MentisDB is an append-only semantic memory server. READ THIS FIRST: call `resources/read` for `mentisdb://skill/core` immediately after initialize to load the embedded MentisDB operating skill."
        })),
        "ping" => Ok(serde_json::json!({})),
        "tools/list" => match protocol.list_tools().await {
            Ok(tools) => Ok(serde_json::json!({
                "tools": tools.into_iter().map(|t| {
                    let def = t.to_tool_definition();
                    serde_json::json!({
                        "name": def.name,
                        "description": def.description,
                        "inputSchema": def.parameters_schema
                    })
                }).collect::<Vec<_>>()
            })),
            Err(e) => Err(e.to_string()),
        },
        "tools/call" => {
            let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or(serde_json::json!({}));
            let canonical = canonical_tool_name(name);
            match protocol.execute(&canonical, arguments).await {
                Ok(result) => Ok(serde_json::json!({
                    "content": [{
                        "type": "text",
                        "text": if result.output.is_string() {
                            result.output.as_str().unwrap_or_default().to_string()
                        } else {
                            serde_json::to_string(&result.output).unwrap_or_default()
                        }
                    }],
                    "isError": !result.success
                })),
                Err(e) => Err(e.to_string()),
            }
        }
        "resources/list" => match protocol.list_resources().await {
            Ok(resources) => Ok(serde_json::json!({"resources": resources})),
            Err(e) => Err(e.to_string()),
        },
        "resources/read" => {
            let uri = params.get("uri").and_then(|v| v.as_str()).unwrap_or("");
            match protocol.read_resource(uri).await {
                Ok(content) => Ok(serde_json::json!({"contents": [{"uri": uri, "text": content}]})),
                Err(e) => Err(e.to_string()),
            }
        }
        _ => Err(format!("Method not found: {method}")),
    };

    Some(match result {
        Ok(r) => serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": r
        })
        .to_string(),
        Err(e) => serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32603,
                "message": e
            }
        })
        .to_string(),
    })
}

fn canonical_tool_name(name: &str) -> String {
    let name = name.trim();
    if name.starts_with("mentisdb_") {
        return name.to_string();
    }
    format!("mentisdb_{}", name)
}

// ── HTTP mode (default) ─────────────────────────────────────────────────────

pub async fn run() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // When the server feature is enabled, axum-server (tls-rustls) and reqwest
    // both pull in rustls. Depending on the feature combination, both aws-lc-rs
    // and ring may be compiled in. Rustls refuses to auto-select a provider,
    // so pin `ring` explicitly.
    #[cfg(feature = "server")]
    let _ = rustls::crypto::ring::default_provider().install_default();

    init_logger();
    raise_fd_limit();

    log::info!(
        "mentisdbd v{} starting (headless)",
        env!("CARGO_PKG_VERSION")
    );

    // Adopt legacy ~/.cloudllm/thoughtchain directory layout if present.
    let storage_root_migration = if std::env::var_os("MENTISDB_DIR").is_none() {
        match adopt_legacy_default_mentisdb_dir() {
            Ok(Some(report)) => {
                if report.renamed_root_dir {
                    log::info!(
                        "adopted legacy storage: renamed {} → {}",
                        report.source_dir.display(),
                        report.target_dir.display()
                    );
                } else if report.merged_entries > 0 {
                    log::info!(
                        "adopted legacy storage: merged {} entries from {} → {}",
                        report.merged_entries,
                        report.source_dir.display(),
                        report.target_dir.display()
                    );
                }
                Some(report)
            }
            Ok(None) => None,
            Err(e) => {
                log::warn!("legacy storage adoption failed: {e}");
                None
            }
        }
    } else {
        None
    };
    let _ = storage_root_migration;

    let config = MentisDbServerConfig::from_env();
    run_migrations(&config.service.chain_dir, config.service.default_storage_adapter);

    // Non-blocking update check (log warning if a newer release exists).
    check_for_updates_noninteractive();

    let handles = start_servers(config).await?;

    log::info!("MCP  listening on http://{}", handles.mcp.local_addr());
    log::info!("REST listening on http://{}", handles.rest.local_addr());
    if let Some(h) = &handles.https_mcp {
        log::info!("HTTPS MCP  listening on https://{}", h.local_addr());
    }
    if let Some(h) = &handles.https_rest {
        log::info!("HTTPS REST listening on https://{}", h.local_addr());
    }
    if let Some(h) = &handles.dashboard {
        log::info!("dashboard  listening on https://{}", h.local_addr());
    }

    tokio::signal::ctrl_c().await.ok();
    log::info!("shutdown signal received");
    Ok(())
}

// ── Arg parsing ─────────────────────────────────────────────────────────────

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DaemonArgMode {
    Help,
    Run,
    Stdio,
    Both,
    CliSubcommand(Vec<OsString>),
}

pub(crate) fn parse_daemon_args<I, T>(args: I) -> Result<DaemonArgMode, String>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
{
    let args = args.into_iter().map(Into::into).collect::<Vec<OsString>>();

    if args.is_empty() {
        return Ok(DaemonArgMode::Run);
    }

    if args.len() == 1 && matches!(args[0].to_string_lossy().as_ref(), "--help" | "-h" | "help") {
        return Ok(DaemonArgMode::Help);
    }

    let first = args[0].to_string_lossy();
    if matches!(
        first.as_ref(),
        "setup" | "wizard" | "add" | "search" | "agents" | "backup" | "restore"
    ) {
        let mut command = vec![OsString::from("mentisdbd")];
        command.extend(args);
        return Ok(DaemonArgMode::CliSubcommand(command));
    }

    if let Some(mode_idx) = args
        .iter()
        .position(|arg| arg.to_string_lossy() == "--mode")
    {
        if mode_idx + 1 >= args.len() {
            return Err("--mode requires a value (stdio, http, or both)".to_string());
        }
        let mode = args[mode_idx + 1].to_string_lossy();
        match mode.as_ref() {
            "stdio" => return Ok(DaemonArgMode::Stdio),
            "http" => return Ok(DaemonArgMode::Run),
            "both" => return Ok(DaemonArgMode::Both),
            _ => {
                return Err(format!(
                    "Invalid --mode value '{}'. Valid values: stdio, http, both",
                    mode
                ))
            }
        }
    }

    if args
        .iter()
        .any(|arg| arg.to_string_lossy() == "--stdio-mcp")
    {
        return Ok(DaemonArgMode::Stdio);
    }

    Err(format!(
        "Unexpected arguments for `mentisdbd`: {}",
        args.iter()
            .map(|arg| arg.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ")
    ))
}

pub(crate) fn daemon_help_text() -> &'static str {
    "\
mentisdbd — headless MentisDB daemon

Usage:
  mentisdbd                    Start HTTP MCP + REST servers (default)
  mentisdbd --mode stdio       Start MCP server over stdio
  mentisdbd --mode http        HTTP servers only (same as default)
  mentisdbd --mode both        Run both stdio and HTTP
  mentisdbd --stdio-mcp        Alias for --mode stdio
  mentisdbd --help             Show this help

CLI subcommands (delegated — see `mentisdbd <subcommand> --help` for details):
  setup, wizard, add, search, agents, backup, restore

Configuration is read from MENTISDB_* environment variables.
"
}

fn run_cli_subcommand(args: Vec<OsString>) -> ExitCode {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let stderr = io::stderr();
    let mut input = stdin.lock();
    let mut output = stdout.lock();
    let mut errors = stderr.lock();

    run_cli_subcommand_with_io(args, &mut input, &mut output, &mut errors)
}

pub(crate) fn run_cli_subcommand_with_io(
    args: Vec<OsString>,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    mentisdb::cli::run_with_io(args, input, out, err)
}

// ── Entry point ─────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> ExitCode {
    match parse_daemon_args(std::env::args_os().skip(1)) {
        Ok(DaemonArgMode::Help) => {
            println!("{}", daemon_help_text());
            ExitCode::SUCCESS
        }
        Ok(DaemonArgMode::Run) => match run().await {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("{error}");
                ExitCode::from(1)
            }
        },
        Ok(DaemonArgMode::Stdio) => {
            init_logger();
            let config = MentisDbServerConfig::from_env();
            match run_stdio_mode(config).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("{error}");
                    ExitCode::from(1)
                }
            }
        }
        Ok(DaemonArgMode::Both) => {
            init_logger();
            let config = MentisDbServerConfig::from_env();
            let stdio_handle = tokio::spawn(async move { run_stdio_mode(config.clone()).await });
            let http_handle = tokio::spawn(async move { run().await });
            let (stdio_result, http_result) = tokio::join!(stdio_handle, http_handle);
            if let Err(e) = stdio_result {
                eprintln!("Stdio server error: {e}");
            }
            match http_result {
                Ok(Ok(())) => ExitCode::SUCCESS,
                Ok(Err(e)) => {
                    eprintln!("HTTP server error: {e}");
                    ExitCode::from(1)
                }
                Err(e) => {
                    eprintln!("HTTP server join error: {e}");
                    ExitCode::from(1)
                }
            }
        }
        Ok(DaemonArgMode::CliSubcommand(args)) => run_cli_subcommand(args),
        Err(message) => {
            eprintln!("{message}");
            eprintln!();
            eprintln!("{}", daemon_help_text());
            ExitCode::from(2)
        }
    }
}
