mod accounts;
mod config;
mod formats;
mod ir;
mod mgmt;
mod oauth;
mod proxy;
mod server;
mod sse;
mod state;
mod upstream;
mod ws;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::accounts::Provider;
use crate::config::Config;
use crate::state::App;

#[derive(Parser)]
#[command(
    name = "subpool",
    version,
    about = "OpenAI / Claude / Gemini compatible proxy for your Claude Code, Codex and Gemini accounts"
)]
struct Cli {
    /// Path to the config file (created with defaults if missing).
    #[arg(short, long, global = true, env = "SUBPOOL_CONFIG", default_value = "config.yaml")]
    config: PathBuf,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the proxy server (default).
    Serve,
    /// Sign in to an account: claude or codex.
    Login {
        provider: String,
        /// Print the URL instead of opening a browser.
        #[arg(long)]
        no_browser: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = Config::load(&cli.config)?;
    let filter = std::env::var("RUST_LOG")
        .unwrap_or_else(|_| if cfg.debug { "subpool=debug".into() } else { "subpool=info".into() });
    tracing_subscriber::fmt().with_env_filter(filter).with_target(false).compact().init();
    std::fs::create_dir_all(cfg.auth_dir()).ok();

    let app = App::new(cfg, cli.config.clone());
    match cli.cmd {
        Some(Cmd::Login { provider, no_browser }) => login(app, &provider, no_browser).await,
        _ => serve(app).await,
    }
}

async fn serve(app: Arc<App>) -> Result<()> {
    let cfg = app.cfg();
    if !cfg.is_loopback() && cfg.api_keys.is_empty() {
        tracing::warn!(
            "listening on {} without api-keys: anyone who can reach this port can use your accounts",
            cfg.host
        );
    }
    let addr: SocketAddr = format!("{}:{}", if cfg.host.is_empty() { "0.0.0.0" } else { &cfg.host }, cfg.port)
        .parse()
        .or_else(|_| format!("[{}]:{}", cfg.host, cfg.port).parse())
        .context("invalid host/port")?;
    let listener = tokio::net::TcpListener::bind(addr).await.with_context(|| format!("binding {addr}"))?;

    tokio::spawn(oauth::refresher(app.clone()));
    tokio::spawn(watch(app.clone()));

    let shown =
        if addr.ip().is_unspecified() { format!("http://127.0.0.1:{}", addr.port()) } else { format!("http://{addr}") };
    let accounts = app.pool.all();
    println!();
    println!("  \x1b[1mSubpool\x1b[0m {}", env!("CARGO_PKG_VERSION"));
    println!("  dashboard  {shown}");
    println!("  openai     {shown}/v1");
    println!("  anthropic  {shown}");
    println!("  gemini     {shown}/v1beta");
    println!("  accounts   {} loaded from {}", accounts.len(), cfg.auth_dir().display());
    if accounts.is_empty() {
        println!(
            "\n  No accounts yet. Run `subpool login claude` / `subpool login codex` or open the dashboard."
        );
    }
    println!();

    let router = server::router(app);
    axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

fn mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

fn auth_signature(dir: &Path) -> Vec<(String, Option<SystemTime>)> {
    let mut v: Vec<_> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
                .map(|e| (e.file_name().to_string_lossy().to_string(), mtime(&e.path())))
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

/// Hot-reloads the config file and the auth directory.
async fn watch(app: Arc<App>) {
    let mut cfg_time = mtime(&app.cfg_path);
    let mut auth = auth_signature(&app.cfg().auth_dir());
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let t = mtime(&app.cfg_path);
        if t != cfg_time {
            cfg_time = t;
            match std::fs::read_to_string(&app.cfg_path).map_err(anyhow::Error::from).and_then(|s| Config::parse(&s)) {
                Ok(cfg) => {
                    tracing::info!("config reloaded");
                    app.set_config(cfg);
                }
                Err(e) => tracing::error!("config not reloaded: {e:#}"),
            }
        }
        let sig = auth_signature(&app.cfg().auth_dir());
        if sig != auth {
            auth = sig;
            if !app.reload_suppressed() {
                tracing::info!("auth directory changed, reloading accounts");
                app.reload_accounts();
            }
        }
    }
}

async fn login(app: Arc<App>, provider: &str, no_browser: bool) -> Result<()> {
    let provider = match provider {
        "claude" | "anthropic" => Provider::Claude,
        "codex" | "openai" | "chatgpt" => Provider::Codex,
        other => anyhow::bail!("unknown provider `{other}` (use claude or codex)"),
    };
    let (state, login) = mgmt::start_login(&app, provider).await?;
    println!("\nOpen this URL to sign in:\n\n  {}\n", login.url);
    if !no_browser && open::that(&login.url).is_err() {
        println!("(could not open a browser automatically)");
    }
    if login.callback {
        println!("Waiting for the browser to finish… (or paste the redirect URL here)");
    } else {
        println!("Paste the URL your browser was redirected to:");
    }

    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(1);
    std::thread::spawn(move || {
        let mut line = String::new();
        while std::io::stdin().read_line(&mut line).is_ok_and(|n| n > 0) {
            if !line.trim().is_empty() && tx.blocking_send(line.trim().to_string()).is_err() {
                break;
            }
            line.clear();
        }
    });
    loop {
        tokio::select! {
            Some(input) = rx.recv() => {
                let (code, _) = mgmt::parse_pasted(&input);
                match mgmt::complete_login(&app, &state, &code).await {
                    Ok(label) => { println!("\n✓ Signed in as {label}"); return Ok(()); }
                    Err(e) => { println!("\n✗ {e:#}"); return Err(e); }
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(400)) => {
                let status = app.logins.lock().get(&state).map(|l| (l.status, l.message.clone()));
                match status {
                    Some(("done", m)) => { println!("\n✓ Signed in as {}", m.unwrap_or_default()); return Ok(()); }
                    Some(("error", m)) => anyhow::bail!(m.unwrap_or_default()),
                    _ => {}
                }
            }
        }
    }
}
