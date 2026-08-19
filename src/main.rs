use std::{net::SocketAddr, sync::Arc};

use anyhow::Context;
use axum::{
    Json, Router,
    extract::State,
    routing::{get, post},
};
use axum_server::tls_rustls::RustlsConfig;
use k8s_openapi::api::core::v1::Pod;
use kube::{
    Api, Client,
    core::admission::{AdmissionResponse, AdmissionReview},
};
use tokio::sync::RwLock;
use tower_http::trace::TraceLayer;
use tracing::{error, info};

use debug_operator::{
    api::{DebugProfile, ProfileStore},
    controller::{run_bootstrap_controller, run_profile_cache},
    mutation::mutate_pod,
};

#[derive(Clone)]
struct AppState {
    client: Client,
    profiles: Arc<RwLock<ProfileStore>>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("install ring as the rustls crypto provider"))?;

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let client = Client::try_default()
        .await
        .context("create Kubernetes client")?;
    let profiles = Arc::new(RwLock::new(ProfileStore::default()));

    tokio::spawn(run_profile_cache(client.clone(), profiles.clone()));
    tokio::spawn(run_bootstrap_controller(client.clone()));

    let state = AppState { client, profiles };
    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/mutate/pods", post(admit_pod))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let addr: SocketAddr = std::env::var("LISTEN_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:8443".to_string())
        .parse()
        .context("parse LISTEN_ADDR")?;

    info!(%addr, "starting debug-operator webhook");
    match (
        std::env::var("TLS_CERT_FILE"),
        std::env::var("TLS_KEY_FILE"),
    ) {
        (Ok(cert), Ok(key)) => {
            let config = RustlsConfig::from_pem_file(cert, key)
                .await
                .context("load TLS certificate and key")?;
            axum_server::bind_rustls(addr, config)
                .serve(app.into_make_service())
                .await?;
        }
        _ => {
            let listener = tokio::net::TcpListener::bind(addr).await?;
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown_signal())
                .await?;
        }
    }
    Ok(())
}

async fn admit_pod(
    State(state): State<AppState>,
    Json(review): Json<AdmissionReview<Pod>>,
) -> Json<AdmissionReview<kube::core::DynamicObject>> {
    let req = match review.try_into() {
        Ok(req) => req,
        Err(err) => {
            error!(?err, "invalid admission review");
            return Json(AdmissionResponse::invalid("invalid AdmissionReview").into_review());
        }
    };

    let profiles = state.profiles.read().await;
    let namespaces: Api<k8s_openapi::api::core::v1::Namespace> = Api::all(state.client.clone());
    let response = match mutate_pod(&req, &profiles, &namespaces).await {
        Ok(response) => response,
        Err(err) => {
            error!(?err, uid = %req.uid, "failed to mutate pod");
            AdmissionResponse::from(&req).deny(err.to_string())
        }
    };

    Json(response.into_review())
}

async fn shutdown_signal() {
    if let Err(err) = tokio::signal::ctrl_c().await {
        error!(?err, "failed to install ctrl-c handler");
    }
}

#[allow(dead_code)]
fn _assert_profile_api(_: Api<DebugProfile>) {}
