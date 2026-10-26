use std::future::IntoFuture;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use axum::Router;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::{build_router, AppState};

pub async fn serve(state: AppState) -> Result<()> {
    let listen = state.config.router.listen.clone();
    let addr: SocketAddr = listen
        .parse()
        .with_context(|| format!("invalid listen address {}", listen))?;
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind {}", addr))?;
    info!("thickapi listening on {}", addr);
    serve_on(listener, state, async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
}

/// Serve on an already-bound listener until `shutdown` fires, then drain for at most
/// `drain_seconds` (SPEC §1.7 Drain). Tests inject a oneshot instead of ctrl-c.
pub async fn serve_on(
    listener: TcpListener,
    state: AppState,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let drain_seconds = state.config.router.drain_seconds;
    let registry = state.registry.clone();
    let pools = state.pools.clone();
    let drain_token = state.drain.clone();
    let drain_watch = drain_token.clone();
    let thin_in_flight = state.thin_in_flight.clone();

    let swept = state.thin.sweep();
    if swept > 0 {
        info!(swept, "removed dead private request directories");
    }

    // Background direct-path probe (SPEC §19.15 R2): a user request never learns about a block
    // that this task could have found first.
    let claude = state.thin.claude().clone();
    if claude.probe_active() {
        info!(
            model = %claude.probe_config().model,
            on_start = claude.probe_config().on_start,
            "claude oauth probe task started"
        );
        tokio::spawn(claude.probe_loop());
    }

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        shutdown.await;
        drain_token.cancel();
        pools.set_draining(true);
        info!("shutdown requested; draining for up to {}s", drain_seconds);
        let _ = stop_tx.send(());
    });

    let app: Router = build_router(state);
    let serve = axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = stop_rx.await;
        })
        .into_future();
    tokio::pin!(serve);

    tokio::select! {
        r = &mut serve => {
            r.context("serve HTTP")?;
        }
        _ = async {
            drain_watch.cancelled().await;
            let deadline = Instant::now() + Duration::from_secs(drain_seconds);
            loop {
                let thick = registry.active_count();
                let thin = thin_in_flight.load(Ordering::SeqCst);
                if Instant::now() >= deadline {
                    if thick > 0 {
                        registry.cancel_all_active();
                    }
                    break;
                }
                if thick == 0 && thin == 0 {
                    std::future::pending::<()>().await;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        } => {
            info!("drain timeout ({}s); exiting", drain_seconds);
        }
    }
    Ok(())
}

pub fn make_state(config: thickapi_core::ConfigBundle, store: thickapi_core::Store) -> AppState {
    let store = Arc::new(store);
    let _ = store.reconcile_orphan_processes();
    make_state_with_store(config, store)
}

/// Build the shared state around an already-open store (tests use an in-memory one).
pub fn make_state_with_store(
    config: thickapi_core::ConfigBundle,
    store: Arc<thickapi_core::Store>,
) -> AppState {
    let registry = Arc::new(thickapi_core::TurnRegistry::new());
    let pools = Arc::new(thickapi_core::PoolManager::new(
        &config.accounts,
        config.router.queue_timeout_seconds,
        config.router.max_queue_depth,
    ));
    let engine = Arc::new(
        thickapi_core::TurnEngine::new(config.clone(), store.clone())
            .with_registry(registry.clone())
            .with_pools(pools.clone()),
    );
    let thin = Arc::new(thickapi_core::thin::runtime::ThinRuntime::new(&config));
    AppState {
        config,
        store,
        engine,
        registry,
        pools,
        started_at: std::time::Instant::now(),
        started_unix: thickapi_core::thin::openai::now_unix(),
        thin,
        thin_in_flight: Arc::new(AtomicU32::new(0)),
        drain: CancellationToken::new(),
        dump_tools: Arc::new(std::sync::Mutex::new(None)),
    }
}
