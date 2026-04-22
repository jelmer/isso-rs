use std::net::SocketAddr;
use std::path::PathBuf;

use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Router;
use clap::{Parser, Subcommand};

use isso_rs::{config::Config, db, migrate, server};

#[derive(Debug, Parser)]
#[command(name = "isso", about = "A lightweight, self-hosted commenting service")]
struct Cli {
    /// Path to isso.cfg. Pass multiple times for multi-site deployments —
    /// each config needs `[general] name = <slug>` and its routes will be
    /// mounted at `/<slug>/`. Applies to all subcommands (and the default
    /// "serve" behaviour when no subcommand is given).
    #[arg(short = 'c', long, global = true)]
    config: Vec<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Start the comment server (default when no subcommand is given).
    Serve,
    /// Import a comment dump into the configured SQLite database.
    /// Supports Disqus XML, WordPress WXR, and the Isso "generic" JSON format.
    Import {
        /// Import format. Use "auto" to detect from the file contents.
        #[arg(short = 't', long, default_value = "auto")]
        r#type: String,
        /// Path to the dump file.
        dump: PathBuf,
        /// For Disqus: keep threads with empty `<id/>` elements. Matches
        /// isso's --empty-id CLI flag.
        #[arg(long)]
        empty_id: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new("isso_rs=info,tower_http=info")
            }),
        )
        .init();

    let cli = Cli::parse();

    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => run_serve(cli.config).await,
        Command::Import {
            r#type,
            dump,
            empty_id,
        } => run_import(cli.config, &r#type, &dump, empty_id).await,
    }
}

async fn run_serve(config_paths: Vec<PathBuf>) -> anyhow::Result<()> {
    let configs: Vec<Config> = if config_paths.is_empty() {
        vec![Config::default()]
    } else {
        config_paths
            .iter()
            .map(|p| Config::from_file(p))
            .collect::<anyhow::Result<_>>()?
    };

    let listen = configs[0].server.listen.clone();

    let app = if configs.len() == 1 {
        server::build_app(configs.into_iter().next().unwrap()).await?
    } else {
        build_multisite(configs).await?
    };

    // `[server] listen` accepts either `http://host:port` / bare `host:port`
    // or `unix:///path/to/socket`. Python Isso supports both; we mirror that.
    #[cfg(unix)]
    if let Some(socket_path) = listen.strip_prefix("unix://") {
        return serve_unix(socket_path, app).await;
    }
    #[cfg(not(unix))]
    if listen.starts_with("unix://") {
        anyhow::bail!("unix:// listener is not supported on this platform");
    }

    let addr: SocketAddr = parse_listen(&listen)?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("listening on {addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(unix)]
async fn serve_unix(socket_path: &str, app: Router) -> anyhow::Result<()> {
    use axum::extract::Request;
    use hyper::body::Incoming;
    use hyper_util::rt::{TokioExecutor, TokioIo};
    use hyper_util::server::conn::auto::Builder;
    use tower::Service;

    // Remove any stale socket from a previous crash.
    match std::fs::remove_file(socket_path) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(anyhow::anyhow!("removing stale socket: {e}")),
    }
    let listener = tokio::net::UnixListener::bind(socket_path)?;
    tracing::info!("listening on unix://{socket_path}");

    // axum::Router → tower Service. hyper-util's auto Builder handles
    // HTTP/1.1 + HTTP/2 connection upgrades per-connection.
    let make_service = app.into_make_service();
    let mut make_service = make_service;
    loop {
        let (socket, _peer) = listener.accept().await?;
        let tower_service = make_service.call(&socket).await.expect("Infallible");
        let io = TokioIo::new(socket);
        // axum's Service<Request> wants &mut self; clone into the closure
        // per-invocation so the outer reference-counted router isn't borrowed
        // mutably across awaits.
        let hyper_service = hyper::service::service_fn(move |req: Request<Incoming>| {
            let mut svc = tower_service.clone();
            async move { svc.call(req).await }
        });
        tokio::spawn(async move {
            if let Err(e) = Builder::new(TokioExecutor::new())
                .serve_connection(io, hyper_service)
                .await
            {
                tracing::warn!("unix connection error: {e}");
            }
        });
    }
}

async fn run_import(
    config_paths: Vec<PathBuf>,
    kind: &str,
    dump: &std::path::Path,
    empty_id: bool,
) -> anyhow::Result<()> {
    // A single config is required — the importer writes to exactly one DB.
    let config = match config_paths.len() {
        0 => Config::default(),
        1 => Config::from_file(&config_paths[0])?,
        _ => anyhow::bail!("import accepts at most one -c config"),
    };
    let pool = db::open(&config.general.dbpath).await?;

    // Mirror Python's safety check: prompt before clobbering a non-empty DB.
    let existing: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM comments")
        .fetch_one(&pool)
        .await?;
    if existing > 0 {
        eprintln!(
            "Isso DB at {} already contains {existing} comments. Continue? [y/N]",
            config.general.dbpath
        );
        let mut buf = String::new();
        std::io::stdin().read_line(&mut buf)?;
        if !matches!(buf.trim(), "y" | "Y") {
            anyhow::bail!("Abort.");
        }
    }

    let report = migrate::dispatch(kind, dump, &pool, empty_id).await?;
    eprintln!(
        "Imported {} threads and {} comments (orphans: {})",
        report.threads_inserted, report.comments_inserted, report.orphan_count
    );
    Ok(())
}

/// Mount each site's router at `/<name>/` based on its `[general] name`.
/// Requests that don't match any site fall through to a 404 page listing
/// the registered mount points — mirrors isso/dispatch.py::Dispatcher.default.
async fn build_multisite(configs: Vec<Config>) -> anyhow::Result<Router> {
    let mut router = Router::new();
    let mut mounts: Vec<String> = Vec::new();
    for cfg in configs {
        let name = cfg.general.name.clone();
        if name.is_empty() {
            tracing::warn!("skipping config: [general] name is empty");
            continue;
        }
        let mount = format!("/{name}");
        mounts.push(mount.clone());
        let inner = server::build_app(cfg).await?;
        router = router.nest(&mount, inner);
    }
    if mounts.is_empty() {
        anyhow::bail!("no sites configured — every config needs [general] name");
    }
    let listing = mounts.join("\n");
    router = router.fallback(move || {
        let body = listing.clone();
        async move { (StatusCode::NOT_FOUND, body).into_response() }
    });
    Ok(router)
}

fn parse_listen(listen: &str) -> anyhow::Result<SocketAddr> {
    let url = if listen.starts_with("http://") || listen.starts_with("https://") {
        url::Url::parse(listen)?
    } else {
        url::Url::parse(&format!("http://{listen}"))?
    };
    let host = url.host_str().unwrap_or("127.0.0.1");
    let port = url.port().unwrap_or(8080);
    Ok(format!("{host}:{port}").parse()?)
}
