use axum::{routing::get, Router};
use std::path::PathBuf;
use tokio::net::TcpListener;

mod config;
mod hub;
mod io_task;
mod metrics;
mod reaper;
mod region;
mod sim;
mod snapshot;
mod ws;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")))
        .init();

    let config_path = std::env::var("LAZOS_CONFIG")
        .unwrap_or_else(|_| "config/server.toml".to_string());
    let cfg = config::Config::load(&PathBuf::from(&config_path));
    tracing::info!(path = %config_path, "loaded config");

    let mut world = match snapshot::load(&cfg.snapshot_path) {
        Ok(w) => {
            tracing::info!(tick = w.tick_number(), chunks = w.len(), "loaded snapshot");
            w
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!("no snapshot found; starting fresh world");
            simulation::World::new()
        }
        Err(e) => panic!("snapshot load failed: {e}"),
    };
    let chunk_size_u8 = u8::try_from(simulation::CHUNK_SIZE).expect("CHUNK_SIZE > u8::MAX");
    match io_task::replay_into(&mut world, &cfg.snapshot_path, chunk_size_u8) {
        Ok(0) => {}
        Ok(n) => tracing::info!(records = n, tick = world.tick_number(), "wal replayed"),
        Err(e) => panic!("wal replay failed: {e}"),
    }
    let regions: std::sync::Arc<[protocol::Region]> =
        region::load(&mut world, &cfg.regions_path).into();

    let metrics = metrics::Metrics::new();
    let io_handles = io_task::spawn(cfg.snapshot_path.clone(), chunk_size_u8);
    let sim_handles = sim::spawn(cfg.clone(), world, io_handles.tx);
    let hub_tx = hub::spawn(cfg.clone(), sim_handles.cmd_tx, sim_handles.event_rx, regions, metrics.clone());

    // Metrics server.
    let metrics_server = {
        let metrics = metrics.clone();
        let bind = cfg.metrics_bind.clone();
        async move {
            let app = Router::new().route(
                "/metrics",
                get(move || {
                    let m = metrics.clone();
                    async move { m.render() }
                }),
            );
            let listener = TcpListener::bind(&bind).await.expect("bind metrics");
            tracing::info!(%bind, "metrics serving");
            axum::serve(listener, app).await.expect("metrics serve");
        }
    };

    // Game server.
    let game_server = {
        let bind = cfg.bind.clone();
        let app = Router::new()
            .route("/ws", get(ws::upgrade))
            .with_state(ws::WsState { hub: hub_tx.clone() });
        async move {
            let listener = TcpListener::bind(&bind).await.expect("bind game");
            tracing::info!(%bind, "game serving");
            axum::serve(listener, app).await.expect("game serve");
        }
    };

    tokio::select! {
        _ = metrics_server => {}
        _ = game_server => {}
        _ = tokio::signal::ctrl_c() => tracing::info!("shutdown signal"),
    }
}
