//! Chaturbate API and CDN client.
//!
//! Three gates sit in front of a recording:
//! 1. the JSON API wants cookies (a cookieless request gets the login page),
//! 2. the age gate is decided by the *exit country*, so a proxy can move it,
//! 3. the master playlist URL is single-use and its chunklist `session` token
//!    expires after a minute or two, so recording has to re-mint.
//!
//! Only the API leg uses the proxy. The mmcdn CDN is not gated.

use crate::config::Config;
use crate::error::{Error, Result};
use crate::hls::{self, Variant};
use serde::Deserialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

pub const API_BASE: &str = "https://chaturbate.com";
pub const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/// Media lives on `*.mmcdn.com` only.
pub fn is_allowed_media_host(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let host = rest.split(['/', '?']).next().unwrap_or("");
    let host = host.split(':').next().unwrap_or("");
    host == "mmcdn.com" || host.ends_with(".mmcdn.com")
}

pub struct Http {
    proxy: String,
    api: Mutex<ureq::Agent>,
    direct: ureq::Agent,
    warmed: AtomicBool,
}

fn new_agent(proxy: &str) -> Result<ureq::Agent> {
    let mut builder = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent(USER_AGENT);
    let configured = proxy.trim();
    if !configured.is_empty() {
        // A fresh SOCKS username asks Tor for a fresh circuit; that is how a
        // Cloudflare-gated exit gets replaced instead of retried forever.
        let rotated = rotate_circuit(configured);
        let proxy = ureq::Proxy::new(&rotated)
            .map_err(|e| Error::Other(format!("bad proxy {configured:?}: {e}")))?;
        builder = builder.proxy(proxy);
    }
    Ok(builder.build())
}

/// `socks5h://user:pass@host:port` -> same with new random credentials.
fn rotate_circuit(proxy: &str) -> String {
    let normalized = proxy
        .strip_prefix("socks5h://")
        .map(|rest| format!("socks5://{rest}"))
        .unwrap_or_else(|| proxy.to_string());
    let Some(rest) = normalized.strip_prefix("socks5://") else {
        return normalized;
    };
    let (creds, host) = match rest.rsplit_once('@') {
        Some((creds, host)) => (creds, host),
        None => ("", rest),
    };
    let _ = creds;
    let tag = format!(
        "cb{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    );
    format!("socks5://{tag}:{tag}@{host}")
}

impl Http {
    pub fn new(config: &Config) -> Result<Self> {
        Ok(Self {
            proxy: config.proxy.clone(),
            api: Mutex::new(new_agent(&config.proxy)?),
            direct: ureq::AgentBuilder::new()
                .timeout(std::time::Duration::from_secs(30))
                .user_agent(USER_AGENT)
                .build(),
            warmed: AtomicBool::new(false),
        })
    }

    /// Replace the API agent: new Tor circuit, empty cookie jar.
    pub fn rebuild(&self) -> Result<()> {
        let agent = new_agent(&self.proxy)?;
        *self.api.lock().expect("api agent lock") = agent;
        self.warmed.store(false, Ordering::SeqCst);
        Ok(())
    }

    /// One homepage fetch harvests the cookies the JSON APIs require.
    fn warm(&self) -> Result<()> {
        if self.warmed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let agent = self.api.lock().expect("api agent lock").clone();
        let result = agent
            .get(API_BASE)
            .set("Accept", "text/html,application/xhtml+xml")
            .call();
        match result {
            Ok(_) => Ok(()),
            Err(e) => {
                self.warmed.store(false, Ordering::SeqCst);
                Err(Error::Http(format!("chaturbate.com: {e}")))
            }
        }
    }

    fn api_get(&self, url: &str, referer: &str) -> Result<String> {
        self.warm()?;
        let agent = self.api.lock().expect("api agent lock").clone();
        let resp = agent
            .get(url)
            .set("Referer", referer)
            .set("Accept", "application/json")
            .set("X-Requested-With", "XMLHttpRequest")
            .call()
            .map_err(Error::from)?;
        resp.into_string().map_err(|e| Error::Http(e.to_string()))
    }

    /// A media body as bytes, over the shared direct agent (one TLS session
    /// reused for every segment, which is the difference between keeping up
    /// with a 1080p stream and falling permanently behind it).
    pub fn media_bytes(&self, url: &str) -> Result<Vec<u8>> {
        if !is_allowed_media_host(url) {
            return Err(Error::Other(format!(
                "refusing to fetch a non-CDN URL: {}",
                &url[..url.len().min(80)]
            )));
        }
        let resp = self
            .direct
            .get(url)
            .set("Referer", &format!("{API_BASE}/"))
            .call()?;
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut resp.into_reader(), &mut buf)
            .map_err(|e| Error::Http(e.to_string()))?;
        Ok(buf)
    }

    pub fn media_get(&self, url: &str) -> Result<String> {
        if !is_allowed_media_host(url) {
            return Err(Error::Other(format!(
                "refusing to fetch a non-CDN URL: {}",
                &url[..url.len().min(80)]
            )));
        }
        self.direct
            .get(url)
            .set("Referer", &format!("{API_BASE}/"))
            .call()?
            .into_string()
            .map_err(|e| Error::Http(e.to_string()))
    }

    pub fn api_reachable(&self) -> bool {
        self.warm().is_ok()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Context {
    #[serde(default)]
    pub broadcast_username: Option<String>,
    #[serde(default)]
    pub room_status: Option<String>,
    #[serde(default)]
    pub room_title: Option<String>,
    #[serde(default)]
    pub num_viewers: Option<u64>,
    #[serde(default)]
    pub hls_source: Option<String>,
    #[serde(default)]
    pub code: Option<String>,
}

impl Context {
    pub fn is_public_live(&self) -> bool {
        self.room_status.as_deref() == Some("public") && self.hls_source.is_some()
    }
}

/// Room context. Retries on the gate: a fresh circuit and cookies each time.
pub fn fetch_context(http: &Http, username: &str) -> Result<Context> {
    let url = format!("{API_BASE}/api/chatvideocontext/{username}/");
    let referer = format!("{API_BASE}/{username}/");
    let mut last = Error::Other("chaturbate did not answer".into());
    for attempt in 0..3 {
        match http.api_get(&url, &referer) {
            Ok(body) => {
                if let Ok(ctx) = serde_json::from_str::<Context>(&body) {
                    if ctx.code.as_deref() == Some("age-gate-required") {
                        return Err(Error::Offline(
                            "Chaturbate age-gates this region. Set a proxy in Settings \
                             (a local Tor works) so the lookup leaves from elsewhere."
                                .into(),
                        ));
                    }
                    return Ok(ctx);
                }
                last = Error::Offline(format!("{username}: the site served HTML, not JSON"));
            }
            Err(e) => last = e,
        }
        if attempt < 2 {
            let _ = http.rebuild();
        }
    }
    Err(last)
}

#[derive(Debug, Clone)]
pub struct Rendition {
    pub url: String,
}

#[derive(Debug, Clone)]
pub struct Master {
    pub variants: Vec<Variant>,
    /// Audio renditions, best first.
    pub audio: Vec<Rendition>,
    pub base: String,
}

fn absolutize(base: &str, path: &str) -> String {
    if path.starts_with("https://") {
        return path.to_string();
    }
    let scheme_host = base.split('/').take(3).collect::<Vec<_>>().join("/");
    if path.starts_with('/') {
        format!("{scheme_host}{path}")
    } else {
        format!("{scheme_host}/{path}")
    }
}

/// Parse the master playlist. Audio is a separate rendition group, so both
/// tracks are needed for a complete recording.
pub fn parse_master(master_url: &str, text: &str) -> Master {
    let variants = hls::parse_master_playlist(master_url, text);
    let mut audio = Vec::new();
    for line in text.lines().map(str::trim) {
        if !line.starts_with("#EXT-X-MEDIA:TYPE=AUDIO") {
            continue;
        }
        if let Some(uri) = line
            .split("URI=\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
        {
            audio.push(Rendition {
                url: absolutize(master_url, uri),
            });
        }
    }
    Master {
        variants,
        audio,
        base: master_url.to_string(),
    }
}

/// Full session: context, one-shot master, variant and audio choice.
pub struct Session {
    pub username: String,
    pub master_url: String,
    pub video_url: String,
    pub audio_url: Option<String>,
    pub quality: String,
    pub resolution: String,
}

pub fn open(http: &Http, username: &str, quality: &str) -> Result<Session> {
    let ctx = fetch_context(http, username)?;
    if !ctx.is_public_live() {
        return Err(Error::Offline(format!(
            "{username} is {}",
            ctx.room_status.as_deref().unwrap_or("offline")
        )));
    }
    let master_url = ctx
        .hls_source
        .clone()
        .ok_or_else(|| Error::Offline("no hls_source in the room context".into()))?;
    let text = http.media_get(&master_url)?;
    let master = parse_master(&master_url, &text);
    if master.variants.is_empty() {
        return Err(Error::Other("the master playlist listed no variants".into()));
    }
    let variant = if quality.is_empty() || quality == "source" || quality == "auto" {
        hls::best_variant(&master.variants)
    } else {
        master
            .variants
            .iter()
            .find(|v| v.name.as_deref() == Some(quality))
            .or_else(|| hls::best_variant(&master.variants))
    }
    .cloned()
    .ok_or_else(|| Error::Other("no playable variant".into()))?;

    Ok(Session {
        username: username.to_string(),
        master_url,
        video_url: variant.uri.clone(),
        audio_url: master.audio.first().map(|r| r.url.clone()),
        quality: variant.name.clone().unwrap_or_else(|| "auto".into()),
        resolution: variant
            .resolution
            .map(|(w, h)| format!("{w}x{h}"))
            .unwrap_or_else(|| "source".into()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_mmcdn_is_allowed() {
        assert!(is_allowed_media_host("https://edge22-rtm.live.mmcdn.com/v1/x"));
        assert!(!is_allowed_media_host("https://mmcdn.com.evil.net/x"));
        assert!(!is_allowed_media_host("http://live.mmcdn.com/x"));
        assert!(!is_allowed_media_host("https://chaturbate.com/x"));
    }

    #[test]
    fn circuit_rotation_keeps_host_and_changes_creds() {
        let rotated = rotate_circuit("socks5h://127.0.0.1:9050");
        assert!(rotated.starts_with("socks5://cb"));
        assert!(rotated.ends_with("@127.0.0.1:9050"));
        let again = rotate_circuit(&rotated);
        assert!(again.ends_with("@127.0.0.1:9050"));
        assert_ne!(rotated, again);
    }

    #[test]
    fn master_parses_audio_renditions() {
        let text = "#EXTM3U\n\
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio_aac_128\",NAME=\"Audio_200_5\",DEFAULT=YES,URI=\"/v1/edge/x/chunklist_5_audio.m3u8?session=s\"\n\
#EXT-X-STREAM-INF:BANDWIDTH=1000,RESOLUTION=640x360\n\
/v1/edge/x/chunklist_0_video.m3u8?session=s\n";
        let master = parse_master(
            "https://edge22-rtm.live.mmcdn.com/v1/edge/streams/origin.u.s/master.m3u8",
            text,
        );
        assert_eq!(master.variants.len(), 1);
        assert_eq!(master.audio.len(), 1);
        assert!(master.audio[0]
            .url
            .starts_with("https://edge22-rtm.live.mmcdn.com/v1/edge/x/"));
    }
}
