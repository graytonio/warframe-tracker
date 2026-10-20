use warframe_tracker::{AppState, app, db, import};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cmd = std::env::args().nth(1).unwrap_or_else(|| "serve".into());
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    match cmd.as_str() {
        "serve" => {
            let pool = db::connect_with_retry(&url).await;
            db::migrate(&pool).await?;
            let web_dir = std::env::var("WEB_DIR").unwrap_or_else(|_| "web/dist".into());
            let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await?;
            println!("listening on :3000");
            axum::serve(listener, app(AppState { pool }, &web_dir)).await?;
        }
        "import" => {
            let pool = db::connect_with_retry(&url).await;
            db::migrate(&pool).await?;
            import::run_import(&pool).await?;
        }
        other => anyhow::bail!("unknown command {other}; expected serve | import | dump <dir>"),
    }
    Ok(())
}
