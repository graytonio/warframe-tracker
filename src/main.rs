use anyhow::Context;
use warframe_tracker::{AppState, app, db, import, lua, tokens, wiki};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cmd = std::env::args().nth(1).unwrap_or_else(|| "serve".into());
    if cmd == "dump" {
        let dir = std::env::args()
            .nth(2)
            .unwrap_or_else(|| "wiki-json".into());
        return dump(&dir).await;
    }
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
        "token" => {
            let args: Vec<String> = std::env::args().skip(2).collect();
            let flag = |f: &str| {
                args.iter()
                    .position(|a| a == f)
                    .and_then(|i| args.get(i + 1))
                    .cloned()
            };
            let name = flag("--name").context("--name is required")?;
            let pool = db::connect_with_retry(&url).await;
            db::migrate(&pool).await?;
            match args.first().map(String::as_str) {
                Some("create") => {
                    let user = flag("--user").map_or(Ok(1), |u| u.parse())?;
                    println!("{}", tokens::create(&pool, user, &name).await?);
                }
                Some("revoke") => {
                    if !tokens::revoke(&pool, &name).await? {
                        eprintln!("no live token named {name}");
                        std::process::exit(1);
                    }
                }
                _ => anyhow::bail!("expected token create|revoke --name N [--user ID]"),
            }
        }
        other => anyhow::bail!(
            "unknown command {other}; expected serve | import | token create|revoke | dump <dir>"
        ),
    }
    Ok(())
}

/// Dev tool: fetch every module and write its evaluated JSON, for fixtures and inspection.
async fn dump(dir: &str) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    let client = wiki::WikiClient::new("https://wiki.warframe.com");
    let mut sources = std::collections::HashMap::new();
    for t in wiki::MODULES {
        sources.insert(t.to_string(), client.raw(t).await?);
    }
    for t in wiki::MODULES.iter().filter(|t| **t != "Module:Table") {
        let v = lua::eval_module(t, &sources)?;
        let file = format!("{dir}/{}.json", t.replace([':', '/'], "_"));
        std::fs::write(&file, serde_json::to_vec_pretty(&v)?)?;
        println!("wrote {file}");
    }
    Ok(())
}
