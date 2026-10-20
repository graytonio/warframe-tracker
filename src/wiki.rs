use anyhow::Context;
use std::collections::HashMap;

/// Every wiki module the importer reads. `Module:Table` is only there to satisfy `require`.
pub const MODULES: &[&str] = &[
    "Module:Warframes/data",
    "Module:Weapons/data/primary",
    "Module:Weapons/data/secondary",
    "Module:Weapons/data/melee",
    "Module:Mods/data",
    "Module:Arcane/data",
    "Module:Blueprints/data",
    "Module:Resources/data",
    "Module:Void/data",
    "Module:DropTables/data",
    "Module:Missions/data",
    "Module:Table",
];

pub struct WikiClient {
    base: String,
    http: reqwest::Client,
}

impl WikiClient {
    pub fn new(base: &str) -> Self {
        let ua = format!(
            "warframe-tracker/{} (+https://github.com/graytonio/warframe-tracker)",
            env!("CARGO_PKG_VERSION")
        );
        let http = reqwest::Client::builder()
            .user_agent(ua)
            .build()
            .expect("reqwest client");
        Self {
            base: base.trim_end_matches('/').to_string(),
            http,
        }
    }

    /// Latest revision id per title, in one request (API limit is 50 titles).
    /// Missing pages are left out of the map.
    pub async fn revisions(&self, titles: &[&str]) -> anyhow::Result<HashMap<String, i64>> {
        let v: serde_json::Value = self
            .http
            .get(format!("{}/api.php", self.base))
            .query(&[
                ("action", "query"),
                ("format", "json"),
                ("prop", "revisions"),
                ("rvprop", "ids"),
                ("titles", &titles.join("|")),
            ])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let pages = v["query"]["pages"]
            .as_object()
            .context("no query.pages in response")?;
        Ok(pages
            .values()
            .filter_map(|p| {
                Some((
                    p["title"].as_str()?.to_string(),
                    p["revisions"][0]["revid"].as_i64()?,
                ))
            })
            .collect())
    }

    pub async fn raw(&self, title: &str) -> anyhow::Result<String> {
        Ok(self
            .http
            .get(format!("{}/index.php", self.base))
            .query(&[("title", title), ("action", "raw")])
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::WikiClient;
    use axum::{Router, extract::Query, routing::get};
    use std::collections::HashMap;

    async fn stub() -> String {
        let app = Router::new()
            .route("/api.php", get(|| async {
                r#"{"query":{"pages":{"1":{"title":"Module:A","revisions":[{"revid":42}]},"-1":{"title":"Module:Gone","missing":""}}}}"#
            }))
            .route(
                "/index.php",
                get(|Query(q): Query<HashMap<String, String>>| async move {
                    format!("return '{}'", q["title"])
                }),
            );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn parses_revisions_and_skips_missing() {
        let c = WikiClient::new(&stub().await);
        let revs = c.revisions(&["Module:A", "Module:Gone"]).await.unwrap();
        assert_eq!(revs.get("Module:A"), Some(&42));
        assert!(!revs.contains_key("Module:Gone"));
    }

    #[tokio::test]
    async fn fetches_raw_source() {
        let c = WikiClient::new(&stub().await);
        assert_eq!(c.raw("Module:A").await.unwrap(), "return 'Module:A'");
    }
}
