//! Pushes WFHelper's `inventory.json` to the tracker whenever it changes.
//! Never prints the token: it only ever goes into the `Authorization` header.

mod inventory;

use anyhow::{Context, bail};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

const USAGE: &str = "usage: wft-sync --url URL [--token T] [--file PATH] [--interval SECS] [--once]
each flag falls back to WFT_URL, WFT_TOKEN, WFT_FILE, WFT_INTERVAL (prefer WFT_TOKEN)";

struct Config {
    url: String,
    token: String,
    file: PathBuf,
    interval: Duration,
    once: bool,
}

/// What was last pushed, so unchanged files are skipped.
#[derive(Default)]
struct Last {
    mtime: Option<SystemTime>,
    oid: Option<String>,
}

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.is_empty())
}

/// WFHelper writes `<userData>/api-helper/inventory.json`; `userData` is Electron's appData
/// + "WFHelper" unless `WFHELPER_USER_DATA` overrides it (WFHelper's appIdentity.ts).
fn default_file(os: &str, env: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let base = match env("WFHELPER_USER_DATA") {
        Some(d) => PathBuf::from(d),
        None if os == "windows" => PathBuf::from(env("APPDATA")?).join("WFHelper"),
        None => env("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| Some(PathBuf::from(env("HOME")?).join(".config")))?
            .join("WFHelper"),
    };
    Some(base.join("api-helper").join("inventory.json"))
}

fn config() -> anyhow::Result<Config> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        std::process::exit(0);
    }
    let flag = |f: &str, var: &str| {
        args.iter()
            .position(|a| a == f)
            .and_then(|i| args.get(i + 1))
            .cloned()
            .or_else(|| env(var))
    };
    let file = match flag("--file", "WFT_FILE") {
        Some(f) => PathBuf::from(f),
        None => default_file(std::env::consts::OS, env)
            .context("can't find WFHelper's data folder; pass --file")?,
    };
    let interval = match flag("--interval", "WFT_INTERVAL") {
        Some(s) => s.parse().context("--interval must be whole seconds")?,
        None => 60,
    };
    Ok(Config {
        url: flag("--url", "WFT_URL").context("--url or WFT_URL is required")?,
        token: flag("--token", "WFT_TOKEN").context("--token or WFT_TOKEN is required")?,
        file,
        interval: Duration::from_secs(interval),
        once: args.iter().any(|a| a == "--once"),
    })
}

enum Push {
    Done,
    Unauthorized,
}

/// One tick: stat, read, and push if the inventory changed since the last push.
fn tick(c: &Config, http: &reqwest::blocking::Client, last: &mut Last) -> anyhow::Result<Push> {
    let mtime = std::fs::metadata(&c.file)
        .and_then(|m| m.modified())
        .with_context(|| format!("no inventory at {}; is WFHelper running?", c.file.display()))?;
    if last.mtime == Some(mtime) {
        return Ok(Push::Done);
    }
    let v = inventory::read(&c.file)?;
    let oid = inventory::sync_id(&v);
    if oid.is_some() && oid == last.oid {
        last.mtime = Some(mtime);
        return Ok(Push::Done);
    }
    let items = inventory::flatten(&v);
    let res = http
        .post(format!(
            "{}/api/sync/inventory",
            c.url.trim_end_matches('/')
        ))
        .bearer_auth(&c.token)
        .json(&serde_json::json!({ "items": items }))
        .send()
        .context("posting inventory")?;
    let status = res.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Ok(Push::Unauthorized);
    }
    if !status.is_success() {
        bail!(
            "server returned {status}: {}",
            res.text().unwrap_or_default()
        );
    }
    let r: serde_json::Value = res.json().context("reading server response")?;
    let matched = r["matched"].as_u64().unwrap_or(0);
    println!(
        "synced {} paths: matched={matched} unmatched={} updated={}",
        items.len(),
        items.len() as u64 - matched.min(items.len() as u64),
        r["updated"]
    );
    *last = Last {
        mtime: Some(mtime),
        oid,
    };
    Ok(Push::Done)
}

fn main() -> ExitCode {
    let c = match config() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e:#}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let http = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("http client");
    println!("watching {} every {:?}", c.file.display(), c.interval);
    let mut last = Last::default();
    loop {
        match tick(&c, &http, &mut last) {
            Ok(Push::Done) => {}
            Ok(Push::Unauthorized) => {
                eprintln!(
                    "token rejected (401); create a new one with `warframe-tracker token create`"
                );
                return ExitCode::from(2);
            }
            // Mid-write file, network blip, server restart: try again next tick.
            Err(e) if !c.once => eprintln!("warning: {e:#}"),
            Err(e) => {
                eprintln!("error: {e:#}");
                return ExitCode::FAILURE;
            }
        }
        if c.once {
            return ExitCode::SUCCESS;
        }
        std::thread::sleep(c.interval);
    }
}

#[cfg(test)]
mod tests {
    use super::default_file;
    use std::path::PathBuf;

    fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            vars.iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    fn tail(base: PathBuf) -> PathBuf {
        base.join("api-helper").join("inventory.json")
    }

    #[test]
    fn default_file_per_os() {
        let appdata = r"C:\Users\me\AppData\Roaming";
        assert_eq!(
            default_file("windows", env(&[("APPDATA", appdata)])),
            Some(tail(PathBuf::from(appdata).join("WFHelper")))
        );
        assert_eq!(
            default_file("linux", env(&[("HOME", "/home/me")])),
            Some(tail(PathBuf::from("/home/me/.config/WFHelper")))
        );
        assert_eq!(
            default_file(
                "linux",
                env(&[("HOME", "/home/me"), ("XDG_CONFIG_HOME", "/xdg")])
            ),
            Some(tail(PathBuf::from("/xdg/WFHelper")))
        );
        assert_eq!(
            default_file(
                "windows",
                env(&[("APPDATA", appdata), ("WFHELPER_USER_DATA", "/custom")])
            ),
            Some(tail(PathBuf::from("/custom")))
        );
        assert_eq!(default_file("linux", env(&[])), None);
    }
}
