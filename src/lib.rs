pub mod api;
pub mod config;
pub mod db;
pub mod import;
pub mod lua;
pub mod models;
pub mod plan;
pub mod sessions;
pub mod tokens;
pub mod tree;
pub mod wiki;

use axum::Router;
use config::Config;
use sqlx::PgPool;
use std::sync::Arc;
use tower_http::services::{ServeDir, ServeFile};

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub config: Arc<Config>,
}

impl AppState {
    pub fn new(pool: PgPool, config: Config) -> Self {
        AppState {
            pool,
            config: Arc::new(config),
        }
    }
}

pub fn app(state: AppState, web_dir: &str) -> Router {
    let spa = ServeDir::new(web_dir).fallback(ServeFile::new(format!("{web_dir}/index.html")));
    Router::new()
        .nest("/api", api::router())
        .fallback_service(spa)
        .with_state(state)
}
