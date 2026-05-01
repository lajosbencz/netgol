use axum::{routing::get, Router};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::TcpListener;

mod auth;
mod claim_store;
mod claims;
mod config;
mod hub;
mod io_task;
mod metrics;
mod reaper;
mod region;
mod sim;
mod snapshot;
mod user_store;
mod ws;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")))
        .init();

    let config_path = std::env::var("NETGOL_CONFIG")
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
    let static_regions: Arc<[protocol::Region]> =
        region::load(&mut world, &cfg.regions_path).into();

    // Auth + claim stores.
    let user_store = Arc::new(user_store::UserStore::new(cfg.users_dir.clone()).await);
    let claim_store = Arc::new(claim_store::ClaimStore::new(cfg.claims_dir.clone()).await);
    let claim_mgr = claims::ClaimManager::new(
        Arc::clone(&claim_store),
        cfg.claim_w_chunks,
        cfg.claim_h_chunks,
    ).await;

    // Seed world owned chunks: static locked regions + persisted user claims.
    let mut initial_owned = region::locked_chunks(&static_regions);
    initial_owned.extend(claim_mgr.owned_coord_map());
    world.set_owned_chunks(initial_owned);

    let auth_state = Arc::new(auth::AuthState::new(cfg.clone(), Arc::clone(&user_store)).await);

    let metrics = metrics::Metrics::new();
    let io_handles = io_task::spawn(cfg.snapshot_path.clone(), chunk_size_u8, metrics.clone());
    let sim_handles = sim::spawn(cfg.clone(), world, io_handles.tx, metrics.clone());
    let hub_tx = hub::spawn(
        cfg.clone(),
        sim_handles.cmd_tx,
        sim_handles.event_rx,
        static_regions,
        claim_mgr,
        metrics.clone(),
    );

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
        let ws_state = ws::WsState {
            hub: hub_tx.clone(),
            auth: Arc::clone(&auth_state),
            claim_store: Arc::clone(&claim_store),
        };
        let app = Router::new()
            .route("/ws", get(ws::upgrade))
            .route("/auth/providers", get(auth::providers_list))
            .route("/auth/{provider}", get(auth::start))
            .route("/auth/{provider}/callback", get(auth::callback))
            .with_state(ws_state);
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
