use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;

use isso_rs::{config::Config, server};

#[derive(Debug, Parser)]
#[command(name = "isso-rs", about = "Rust port of the Isso comment server")]
struct Args {
    /// Path to isso.cfg
    #[arg(short = 'c', long)]
    config: Option<PathBuf>,
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
    let config = match args.config {
        Some(path) => Config::from_file(&path)?,
        None => Config::default(),
    };

    let listen = config.server.listen.clone();
    let addr: SocketAddr = parse_listen(&listen)?;

    let app = server::build_app(config).await?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("listening on {addr}");
    axum::serve(listener, app).await?;
    Ok(())
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
