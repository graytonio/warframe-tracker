use anyhow::{Context, bail};
use reqwest::Url;

/// Settings for `serve`, read once from the environment.
#[derive(Clone, Debug)]
pub struct Config {
    /// No trailing slash.
    pub public_url: String,
    pub discord_client_id: Option<String>,
    pub discord_client_secret: Option<String>,
    pub discord_api_base: String,
    pub owner_discord_id: Option<String>,
    /// `DEV_LOGIN=1` and `public_url` is localhost.
    pub dev_login: bool,
}

/// Plain-HTTP localhost: dev login allowed, cookies not `Secure`.
fn is_localhost(url: &str) -> bool {
    Url::parse(url).is_ok_and(|u| {
        u.scheme() == "http" && matches!(u.host_str(), Some("localhost" | "127.0.0.1"))
    })
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        Self::from_env_with(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
    }

    pub fn from_env_with(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let public_url = get("PUBLIC_URL").context("PUBLIC_URL must be set")?;
        Url::parse(&public_url).context("PUBLIC_URL is not a URL")?;
        let public_url = public_url.trim_end_matches('/').to_string();
        let dev_login = get("DEV_LOGIN").as_deref() == Some("1");
        if dev_login && !is_localhost(&public_url) {
            bail!("dev login requires a localhost PUBLIC_URL");
        }
        let c = Config {
            discord_client_id: get("DISCORD_CLIENT_ID"),
            discord_client_secret: get("DISCORD_CLIENT_SECRET"),
            discord_api_base: get("DISCORD_API_BASE")
                .unwrap_or_else(|| "https://discord.com/api".into()),
            owner_discord_id: get("OWNER_DISCORD_ID"),
            public_url,
            dev_login,
        };
        if !dev_login && (c.discord_client_id.is_none() || c.discord_client_secret.is_none()) {
            bail!("DISCORD_CLIENT_ID and DISCORD_CLIENT_SECRET must be set (or DEV_LOGIN=1)");
        }
        Ok(c)
    }

    pub fn for_tests() -> Self {
        Config {
            public_url: "http://localhost:3000".into(),
            discord_client_id: None,
            discord_client_secret: None,
            discord_api_base: "https://discord.com/api".into(),
            owner_discord_id: None,
            dev_login: true,
        }
    }

    pub fn secure_cookies(&self) -> bool {
        !is_localhost(&self.public_url)
    }

    /// `scheme://host[:port]`, what browsers send as `Origin`.
    pub fn origin(&self) -> &str {
        // public_url is validated and has no trailing slash; strip any path.
        let after_scheme = self.public_url.find("://").map_or(0, |i| i + 3);
        match self.public_url[after_scheme..].find('/') {
            Some(i) => &self.public_url[..after_scheme + i],
            None => &self.public_url,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Config;
    use std::collections::HashMap;

    fn cfg(vars: &[(&str, &str)]) -> anyhow::Result<Config> {
        let m: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Config::from_env_with(|k| m.get(k).cloned())
    }

    const DISCORD: [(&str, &str); 2] =
        [("DISCORD_CLIENT_ID", "id"), ("DISCORD_CLIENT_SECRET", "s")];

    #[test]
    fn startup_rules() {
        assert!(cfg(&DISCORD).is_err(), "missing PUBLIC_URL");
        assert!(
            cfg(&[("PUBLIC_URL", "https://x.com")]).is_err(),
            "no discord"
        );
        assert!(cfg(&[("PUBLIC_URL", "https://x.com"), DISCORD[0]]).is_err());
        let e = cfg(&[("PUBLIC_URL", "https://x.com"), ("DEV_LOGIN", "1")]).unwrap_err();
        assert_eq!(e.to_string(), "dev login requires a localhost PUBLIC_URL");
        assert!(
            cfg(&[
                ("PUBLIC_URL", "http://localhost.evil.com"),
                ("DEV_LOGIN", "1")
            ])
            .is_err()
        );
        let c = cfg(&[("PUBLIC_URL", "http://localhost:3100"), ("DEV_LOGIN", "1")]).unwrap();
        assert!(c.dev_login && !c.secure_cookies());
        let c = cfg(&[("PUBLIC_URL", "https://x.com/"), DISCORD[0], DISCORD[1]]).unwrap();
        assert!(!c.dev_login && c.secure_cookies());
        assert_eq!(c.public_url, "https://x.com");
        assert_eq!(c.discord_api_base, "https://discord.com/api");
    }

    #[test]
    fn origin_strips_path_and_slash() {
        let c = cfg(&[
            ("PUBLIC_URL", "https://warframe.graytonward.com/"),
            DISCORD[0],
            DISCORD[1],
        ])
        .unwrap();
        assert_eq!(c.origin(), "https://warframe.graytonward.com");
        let mut c = Config::for_tests();
        assert_eq!(c.origin(), "http://localhost:3000");
        c.public_url = "https://x.com:8443/app".into();
        assert_eq!(c.origin(), "https://x.com:8443");
    }
}
