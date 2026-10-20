pub mod api;
pub mod db;
pub mod import;
pub mod lua;
pub mod models;
pub mod plan;
pub mod tree;
pub mod wiki;

use axum::Router;
use sqlx::PgPool;
use tower_http::services::{ServeDir, ServeFile};

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
}

pub fn app(state: AppState, web_dir: &str) -> Router {
    let spa = ServeDir::new(web_dir).fallback(ServeFile::new(format!("{web_dir}/index.html")));
    Router::new()
        .nest("/api", api::router())
        .fallback_service(spa)
        .with_state(state)
}
