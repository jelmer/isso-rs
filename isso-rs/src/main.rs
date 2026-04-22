use std::net::SocketAddr;
use std::path::PathBuf;

use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Router;
use clap::Parser;

use isso_rs::{config::Config, server};

#[derive(Debug, Parser)]
#[command(name = "isso-rs", about = "Rust port of the Isso comment server")]
struct Args {
    /// Path to isso.cfg. Pass multiple times for multi-site deployments —
    /// each config needs `[general] name = <slug>` and its routes will be
    /// mounted at `/<slug>/`.
    #[arg(short = 'c', long)]
    config: Vec<PathBuf>,
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

    let args = Args::parse();

    // No config paths → run a single default instance bound to localhost.
    // One path → single-site (routes at `/`).
    // Many paths → multi-site: each config's `[general] name` becomes the
    // mount point.
    let configs: Vec<Config> = if args.config.is_empty() {
        vec![Config::default()]
    } else {
        args.config
            .iter()
            .map(|p| Config::from_file(p))
            .collect::<anyhow::Result<_>>()?
    };

    // The server address comes from the FIRST config's [server] listen.
    // Multi-site deployments share one port.
    let listen = configs[0].server.listen.clone();
    let addr: SocketAddr = parse_listen(&listen)?;

    let app = if configs.len() == 1 {
        server::build_app(configs.into_iter().next().unwrap()).await?
    } else {
        build_multisite(configs).await?
    };

    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("listening on {addr}");
    axum::serve(listener, app).await?;
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
    // Unmatched paths get a plain-text listing of valid mounts.
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
