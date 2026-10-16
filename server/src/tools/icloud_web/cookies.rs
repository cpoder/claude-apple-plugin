//! A small persistent cookie jar (reqwest `CookieStore`) with JSON serialization: Apple's web
//! session is a handful of cookies on `idmsa.apple.com` and `*.icloud.com`.

use std::sync::Mutex;

use reqwest::header::HeaderValue;
use serde::{Deserialize, Serialize};
use url::Url;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    /// Domain without a leading dot; `host_only` cookies match that exact host only.
    pub domain: String,
    pub host_only: bool,
    pub path: String,
    pub secure: bool,
    /// Unix timestamp; `None` = session cookie (still persisted: Apple's session relies on it).
    pub expires: Option<i64>,
}

#[derive(Default)]
pub struct Jar {
    inner: Mutex<Vec<Cookie>>,
}

fn default_path(url: &Url) -> String {
    let p = url.path();
    match p.rfind('/') {
        Some(0) | None => "/".into(),
        Some(i) => p[..i].to_string(),
    }
}

fn parse_expires(s: &str) -> Option<i64> {
    let s = s.trim();
    chrono::DateTime::parse_from_rfc2822(s)
        .ok()
        .map(|d| d.timestamp())
        .or_else(|| chrono::NaiveDateTime::parse_from_str(s, "%a, %d %b %Y %H:%M:%S GMT").ok().map(|d| d.and_utc().timestamp()))
        .or_else(|| chrono::NaiveDateTime::parse_from_str(s, "%a, %d-%b-%Y %H:%M:%S GMT").ok().map(|d| d.and_utc().timestamp()))
}

/// Parse one `Set-Cookie` header for a response from `url`.
pub fn parse_set_cookie(raw: &str, url: &Url) -> Option<Cookie> {
    let host = url.host_str()?.to_lowercase();
    let mut parts = raw.split(';');
    let (name, value) = parts.next()?.split_once('=')?;
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    let mut c = Cookie {
        name: name.to_string(),
        value: value.trim().to_string(),
        domain: host.clone(),
        host_only: true,
        path: default_path(url),
        secure: false,
        expires: None,
    };
    let now = chrono::Utc::now().timestamp();
    for attr in parts {
        let (k, v) = match attr.split_once('=') {
            Some((k, v)) => (k.trim().to_ascii_lowercase(), v.trim()),
            None => (attr.trim().to_ascii_lowercase(), ""),
        };
        match k.as_str() {
            "domain" => {
                let d = v.trim_start_matches('.').to_lowercase();
                if !d.is_empty() && (host == d || host.ends_with(&format!(".{d}"))) {
                    c.domain = d;
                    c.host_only = false;
                }
            }
            "path" if v.starts_with('/') => c.path = v.to_string(),
            "secure" => c.secure = true,
            "max-age" => {
                if let Ok(secs) = v.parse::<i64>() {
                    c.expires = Some(now + secs);
                }
            }
            "expires" => {
                if c.expires.is_none() {
                    c.expires = parse_expires(v);
                }
            }
            _ => {}
        }
    }
    Some(c)
}

impl Jar {
    pub fn new(cookies: Vec<Cookie>) -> Self {
        Self { inner: Mutex::new(cookies) }
    }

    pub fn snapshot(&self) -> Vec<Cookie> {
        let now = chrono::Utc::now().timestamp();
        self.inner.lock().expect("jar").iter().filter(|c| c.expires.is_none_or(|e| e > now)).cloned().collect()
    }

    pub fn get(&self, name: &str) -> Option<String> {
        self.snapshot().into_iter().find(|c| c.name == name).map(|c| c.value)
    }

    pub fn clear(&self) {
        self.inner.lock().expect("jar").clear();
    }

    fn matches(c: &Cookie, url: &Url) -> bool {
        let Some(host) = url.host_str().map(|h| h.to_lowercase()) else { return false };
        let domain_ok = if c.host_only { host == c.domain } else { host == c.domain || host.ends_with(&format!(".{}", c.domain)) };
        let path = url.path();
        let path_ok = path == c.path || (path.starts_with(&c.path) && (c.path.ends_with('/') || path[c.path.len()..].starts_with('/')));
        domain_ok && path_ok && (!c.secure || url.scheme() == "https")
    }

    pub fn store(&self, c: Cookie) {
        let mut inner = self.inner.lock().expect("jar");
        inner.retain(|x| !(x.name == c.name && x.domain == c.domain && x.path == c.path));
        let now = chrono::Utc::now().timestamp();
        if c.expires.is_none_or(|e| e > now) {
            inner.push(c);
        }
    }
}

impl reqwest::cookie::CookieStore for Jar {
    fn set_cookies(&self, cookie_headers: &mut dyn Iterator<Item = &HeaderValue>, url: &Url) {
        for h in cookie_headers {
            if let Ok(s) = h.to_str()
                && let Some(c) = parse_set_cookie(s, url)
            {
                self.store(c);
            }
        }
    }

    fn cookies(&self, url: &Url) -> Option<HeaderValue> {
        let now = chrono::Utc::now().timestamp();
        let inner = self.inner.lock().expect("jar");
        let mut pairs: Vec<String> = inner
            .iter()
            .filter(|c| c.expires.is_none_or(|e| e > now) && Self::matches(c, url))
            .map(|c| format!("{}={}", c.name, c.value))
            .collect();
        if pairs.is_empty() {
            return None;
        }
        pairs.sort();
        HeaderValue::from_str(&pairs.join("; ")).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::cookie::CookieStore;

    #[test]
    fn parses_and_matches() {
        let url = Url::parse("https://setup.icloud.com/setup/ws/1/accountLogin").unwrap();
        let c = parse_set_cookie("X-APPLE-WEBAUTH-TOKEN=\"v=2:t=abc\"; Domain=.icloud.com; Path=/; Secure; HttpOnly", &url).unwrap();
        assert_eq!(c.domain, "icloud.com");
        assert!(!c.host_only);
        assert_eq!(c.path, "/");
        let jar = Jar::default();
        jar.store(c);
        let v = jar.cookies(&Url::parse("https://p123-ckdatabasews.icloud.com/database/1/x").unwrap()).unwrap();
        assert_eq!(v.to_str().unwrap(), "X-APPLE-WEBAUTH-TOKEN=\"v=2:t=abc\"");
        assert!(jar.cookies(&Url::parse("https://idmsa.apple.com/appleauth/auth").unwrap()).is_none());
        assert!(jar.cookies(&Url::parse("http://www.icloud.com/").unwrap()).is_none(), "secure cookie not sent over http");
    }

    #[test]
    fn host_only_and_expiry() {
        let url = Url::parse("https://idmsa.apple.com/appleauth/auth/signin").unwrap();
        let c = parse_set_cookie("aasp=XYZ; Path=/; Max-Age=0", &url).unwrap();
        assert!(c.host_only);
        assert_eq!(c.path, "/");
        let jar = Jar::default();
        jar.store(c);
        assert!(jar.snapshot().is_empty(), "expired cookie dropped");
        let c = parse_set_cookie("dslang=FR-FR; Domain=apple.com; Expires=Wed, 21 Oct 2037 07:28:00 GMT", &url).unwrap();
        assert!(c.expires.is_some());
        jar.store(c);
        assert_eq!(jar.get("dslang").as_deref(), Some("FR-FR"));
        assert!(jar.cookies(&Url::parse("https://www.apple.com/appleauth/auth/x").unwrap()).is_some());
        assert!(jar.cookies(&Url::parse("https://www.apple.com/").unwrap()).is_none(), "path scoped to /appleauth/auth");
    }
}
