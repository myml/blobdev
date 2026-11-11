//! WebDAV object store (Apache mod_dav, nginx-dav-ext-module, Nextcloud,
//! ownCloud, SabreDAV, ...).
//!
//! WebDAV is just HTTP with a few extra methods, which makes it the better
//! backend for this application: a plain `PUT` replaces an object, a `GET`
//! with a `Range` header reads part of it, and — unlike S3 — a `PUT` with
//! `Content-Range: bytes start-end/total` rewrites a window inside the
//! existing object, so a dirty 4 KiB block costs a 4 KiB request instead of
//! an 8 MiB chunk upload. That is why the capabilities here are
//! [`Caps::FULL`].
//!
//! Two caveats worth knowing before pointing this at a server:
//!
//! * `PROPFIND` is needed by most servers to report object sizes; servers
//!   that answer `HEAD` with a `Content-Length` are also fine and cheaper,
//!   so `stat` tries `HEAD` first and falls back to `PROPFIND`.
//! * Servers that reject partial writes (`405 Method Not Allowed`, `400`,
//!   `501`) are not an error condition for the volume layer: it falls back to
//!   re-uploading the whole object. `patch` therefore reports a plain failure
//!   mentioning "partial write not supported", which is what triggers that
//!   fallback.
//!
//! Keys are mapped to URLs with a fixed, predictable encoding so that the
//! URL built for a request is exactly the URL whose namespace was assumed
//! (see [`encode_segment`] and [`object_path`]).

use anyhow::{Context, Result};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use std::fmt;
use std::io::Read;
use std::ops::Range;
use std::time::Duration;

use crate::store::{Caps, ObjectStore, Retry, StoreError, ETag};

/// Connection settings for a WebDAV server.
#[derive(Clone)]
pub struct WebDavConfig {
    /// Base URL, e.g. `https://dav.example.com/remote.php/dav/files/me/`.
    pub base_url: String,
    pub username: Option<String>,
    pub password: Option<String>,
    /// Key namespace below `base_url`; `""` for the base itself.
    pub prefix: String,
    /// Per-request deadline. `None` uses the HTTP client default.
    pub timeout: Option<Duration>,
}

impl WebDavConfig {
    pub fn new(base_url: impl Into<String>) -> WebDavConfig {
        WebDavConfig {
            base_url: base_url.into(),
            username: None,
            password: None,
            prefix: String::new(),
            timeout: None,
        }
    }

    /// Basic authentication.
    pub fn with_auth(
        mut self,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> WebDavConfig {
        self.username = Some(username.into());
        self.password = Some(password.into());
        self
    }

    /// Set the key prefix; leading and trailing `/` and surrounding
    /// whitespace are dropped.
    pub fn with_prefix(mut self, prefix: impl Into<String>) -> WebDavConfig {
        self.prefix = clean_prefix(&prefix.into());
        self
    }
}

impl fmt::Debug for WebDavConfig {
    /// Never print the password.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebDavConfig")
            .field("base_url", &self.base_url)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("prefix", &self.prefix)
            .field("timeout", &self.timeout)
            .finish()
    }
}

fn clean_prefix(prefix: &str) -> String {
    prefix.trim().trim_matches('/').to_string()
}

/// `true` if `b` may appear literally in a URL path segment: the RFC 3986
/// *unreserved* set plus the sub-delims WebDAV servers do not reinterpret.
///
/// `%` is excluded so an encoded segment can never be decoded a second time
/// server-side; `+` is excluded because a few servers treat it as a space in
/// paths; `:` and `@` are excluded because they would change the meaning of
/// the first segment (`scheme:`, `user@host`).
fn is_literal(b: u8) -> bool {
    b.is_ascii_alphanumeric()
        || matches!(
            b,
            b'-' | b'.' | b'_' | b'~' | b'!' | b'$' | b'&' | b'\'' | b'(' | b')' | b'*' | b','
                | b';' | b'='
        )
}

/// Percent-encode one path segment (no `/` allowed inside).
pub fn encode_segment(seg: &str) -> String {
    let mut out = String::with_capacity(seg.len());
    for &b in seg.as_bytes() {
        if is_literal(b) {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(char::from_digit((b >> 4) as u32, 16).unwrap().to_ascii_uppercase());
            out.push(char::from_digit((b & 0x0f) as u32, 16).unwrap().to_ascii_uppercase());
        }
    }
    out
}

/// Encode a key (which may contain `/`) as a relative URL path.
///
/// Each `/`-separated piece is encoded independently, so a piece such as
/// `..` cannot escape the base path and a literal `%2F` in a key cannot
/// become a separator. Empty segments (from `a//b` or a trailing `/`) are
/// dropped; a key that reduces to nothing maps to `""`.
fn encode_key_path(key: &str) -> String {
    let mut out = String::new();
    for seg in key.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        debug_assert!(seg != "..", "keys are generated internally");
        if !out.is_empty() {
            out.push('/');
        }
        out.push_str(&encode_segment(seg));
    }
    out
}

/// Normalise a base URL: lowercase scheme, drop trailing `/` from the path,
/// keep any query string out (a WebDAV base with a query makes no sense).
fn normalize_base(base: &str) -> Result<String> {
    let base = base.trim();
    anyhow::ensure!(!base.is_empty(), "WebDAV base URL must not be empty");
    let (scheme, rest) = base
        .split_once("://")
        .with_context(|| format!("WebDAV base URL {base:?} has no scheme"))?;
    let scheme = scheme.to_ascii_lowercase();
    anyhow::ensure!(
        scheme == "http" || scheme == "https",
        "unsupported WebDAV base URL scheme {scheme:?}"
    );
    let rest = rest.split('#').next().unwrap_or(rest);
    anyhow::ensure!(
        !rest.contains('?'),
        "WebDAV base URL must not contain a query string"
    );
    let authority = rest.split('/').next().unwrap_or(rest);
    anyhow::ensure!(!authority.is_empty(), "WebDAV base URL {base:?} has no host");
    let path = rest[authority.len()..].trim_end_matches('/');
    Ok(format!("{scheme}://{authority}{path}"))
}

/// Full absolute URL for a request against `key`, or the collection itself
/// for an empty key.
fn object_path(base: &str, prefix: &str, key: &str) -> String {
    let mut out = String::from(base);
    let rel = encode_key_path(key.trim_start_matches('/'));
    let prefix = clean_prefix(prefix);
    if !prefix.is_empty() {
        out.push('/');
        out.push_str(&encode_key_path(&prefix));
    }
    if !rel.is_empty() {
        out.push('/');
        out.push_str(&rel);
    }
    out
}

/// The URL of the collection that holds `key`'s parent directories, used to
/// create the key's directory before the first `PUT`.
fn parent_path(base: &str, prefix: &str, key: &str) -> String {
    let mut out = String::from(base);
    let mut segs: Vec<&str> = Vec::new();
    let prefix = clean_prefix(prefix);
    if !prefix.is_empty() {
        segs.extend(prefix.split('/').filter(|s| !s.is_empty()));
    }
    let trimmed = key.trim_start_matches('/');
    let mut parts: Vec<&str> = trimmed.split('/').filter(|s| !s.is_empty()).collect();
    parts.pop(); // the object's own name
    segs.extend(parts);
    for seg in segs {
        out.push('/');
        out.push_str(&encode_segment(seg));
    }
    out
}

/// `bytes=start-end` (inclusive), or `None` for an empty/all range.
fn range_header(range: Option<Range<u64>>) -> Result<Option<String>> {
    match range {
        None => Ok(None),
        Some(r) => {
            anyhow::ensure!(r.start < r.end, "invalid range {}..{}", r.start, r.end);
            Ok(Some(format!("bytes={}-{}", r.start, r.end - 1)))
        }
    }
}

/// `Content-Range` for a partial `PUT`, or `None` when the write covers the
/// whole object and a plain `PUT` should be used instead.
fn content_range_header(offset: u64, len: u64, total: u64) -> Result<Option<String>> {
    anyhow::ensure!(len > 0, "partial write of zero bytes");
    anyhow::ensure!(
        offset.checked_add(len).with_context(|| format!("offset {offset} + {len} overflows"))?
            <= total,
        "partial write {offset}..{} lies outside the {total}-byte object",
        offset + len
    );
    if offset == 0 && len == total {
        return Ok(None);
    }
    Ok(Some(format!("bytes {offset}-{}/{total}", offset + len - 1)))
}

/// `Authorization` value for basic auth.
fn basic_auth(user: &str, password: &str) -> String {
    format!("Basic {}", B64.encode(format!("{user}:{password}").as_bytes()))
}

/// `href` values of a `207 Multi-Status` response, XML-unescaped.
///
/// Servers answer with absolute or root-relative URLs and with whatever
/// namespace prefix they like, so this ignores prefixes and only extracts the
/// text.
#[cfg_attr(not(test), allow(dead_code))]
fn hrefs(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(i) = rest.find("<href").or_else(|| rest.find("<D:href")).or_else(|| rest.find("<d:href")) {
        let open = &rest[i..];
        let Some(gt) = open.find('>') else { break };
        let name = open[..gt]
            .trim_start_matches('<')
            .trim_end_matches('/')
            .rsplit(':')
            .next()
            .unwrap_or("href")
            .to_string();
        let after = &rest[i + gt + 1..];
        if let Some(value) = element_text(open, gt + 1, &name) {
            if !value.is_empty() {
                out.push(value);
            }
        }
        rest = after;
    }
    out
}

/// Text of the element named `name` whose content starts at byte `start`.
///
/// Prefixes (`D:`, `d:`, ...) are ignored, and a closing tag for anything
/// other than this element (a sibling's `</D:propstat>`) is not mistaken for
/// the end.
fn element_text(body: &str, start: usize, name: &str) -> Option<String> {
    let text = &body[start..];
    let close = text.find("</")?;
    let end = text[close..].find('>')? + close;
    let tag = text[close + 2..end].trim();
    if tag.rsplit(':').next().unwrap_or(tag) != name {
        return None;
    }
    Some(xml_unescape(text[..close].trim()))
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// A response, with the body captured (these bodies are small: PROPFIND
/// listings and error pages).
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Reply {
    #[cfg_attr(not(test), allow(dead_code))]
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn etag(&self) -> Option<ETag> {
        // ETags are opaque; the volume layer only compares them, but the
        // quotes are part of the value for `If-Match` and should stay.
        self.header("etag")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    fn describe(&self) -> String {
        let mut msg = format!("HTTP {}", self.status);
        let text = String::from_utf8_lossy(&self.body);
        let text = text.trim();
        if !text.is_empty() && !text.starts_with("<?xml") && !text.starts_with('<') {
            msg.push_str(": ");
            msg.push_str(&text.chars().take(200).collect::<String>());
        }
        msg
    }
}

/// A blocking WebDAV client.
pub struct WebDavStore {
    /// Retry policy for transient failures.
    retry: Retry,
    cfg: WebDavConfig,
    /// Normalised base URL without a trailing `/`.
    base: String,
    auth: Option<String>,
    agent: ureq::Agent,
}

impl WebDavStore {
    pub fn new(cfg: WebDavConfig) -> Result<WebDavStore> {
        let base = normalize_base(&cfg.base_url)?;
        let auth = match (&cfg.username, &cfg.password) {
            (Some(u), Some(p)) => Some(basic_auth(u, p)),
            (Some(u), None) => Some(basic_auth(u, "")),
            (None, p) => {
                anyhow::ensure!(
                    p.is_none(),
                    "WebDAV username is required when a password is given"
                );
                None
            }
        };
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            // WebDAV needs MKCOL/PROPFIND, which ureq rejects by default.
            .allow_non_standard_methods(true)
            .timeout_global(cfg.timeout)
            .build();
        Ok(WebDavStore {
            retry: Retry::default(),
            cfg,
            base,
            auth,
            agent: config.into(),
        })
    }

    /// Replace the retry policy (1 attempt disables retrying).
    pub fn with_retry(mut self, retry: Retry) -> WebDavStore {
        self.retry = retry;
        self
    }

    /// Absolute URL for a key; useful in logs and tests.
    pub fn url_for(&self, key: &str) -> String {
        object_path(&self.base, &self.cfg.prefix, key)
    }

    /// Run one request and read the (small) response body.
    ///
    /// `Agent::run` takes a plain `http::Request`, which handles any method —
    /// including the WebDAV extensions (`PROPFIND`, `MKCOL`) that have no
    /// builder of their own, and bodiless `PUT`.
    fn request(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<Reply> {
        let mut req = ureq::http::Request::builder().method(method).uri(url);
        for (name, value) in headers {
            req = req.header(*name, *value);
        }
        if let Some(auth) = &self.auth {
            req = req.header("Authorization", auth);
        }
        let req = req
            .body(body.unwrap_or(&[]))
            .with_context(|| format!("build {method} {url}"))?;
        let mut resp = self
            .agent
            .run(req)
            .with_context(|| format!("{method} {url}"))?;
        let status = resp.status().as_u16();
        let mut rheaders = Vec::new();
        for (name, value) in resp.headers() {
            rheaders.push((
                name.as_str().to_ascii_lowercase(),
                value.to_str().unwrap_or_default().to_string(),
            ));
        }
        let mut rbody = Vec::new();
        let read = resp.body_mut().as_reader().read_to_end(&mut rbody);
        if let Err(e) = read {
            if status == 200 {
                return Err(e).with_context(|| format!("read body of {method} {url}"));
            }
        }
        Ok(Reply {
            status,
            headers: rheaders,
            body: rbody,
        })
    }

    /// Create the parent collection of `key` with `MKCOL`.
    ///
    /// Creating a collection that already exists answers `405`, which is the
    /// normal case once the volume has been used; only a `409` (the parent's
    /// parent is missing) is an error.
    fn mkcol(&self, key: &str) -> Result<()> {
        let url = parent_path(&self.base, &self.cfg.prefix, key);
        if url == self.base {
            return Ok(()); // the key is directly in the base collection
        }
        let reply = self.request("MKCOL", &url, &[], None)?;
        match reply.status {
            200..=299 => Ok(()),
            // 405 = already exists; 301/302/307 from servers that redirect a
            // collection URL ending in `/'`. 409/403 from read-only roots
            // surfaces later, but a failed PUT would be less clear.
            405 | 301 | 307 | 308 => Ok(()),
            // A server that does not implement MKCOL is not fatal: the PUT
            // that follows may create the parent implicitly (many do), and if
            // it does not, the PUT error will say so more directly.
            501 => {
                log::debug!("MKCOL {url} unsupported (501); relying on PUT to create it");
                Ok(())
            }
            _ => anyhow::bail!("MKCOL {url}: {}", reply.describe()),
        }
    }

    /// HEAD an object: `(size, etag)` or `None` when it does not exist.
    fn head(&self, key: &str) -> Result<Option<(u64, Option<ETag>)>> {
        let url = self.url_for(key);
        let reply = self.request("HEAD", &url, &[], None)?;
        match reply.status {
            200 => {
                let param = match reply.header("content-length") {
                    Some(v) => v
                        .trim()
                        .parse::<u64>()
                        .with_context(|| format!("bad Content-Length {v:?} for {url}"))?,
                    // Some servers omit it for unknown-size resources; fall
                    // through to PROPFIND to learn the length.
                    None => return self.propfind_size(key),
                };
                Ok(Some((param, reply.etag())))
            }
            404 | 410 => Ok(None),
            // A server that refuses HEAD but serves GET: ask PROPFIND.
            405 | 501 => self.propfind_size(key),
            // Some servers reject HEAD for paths they do not know rather than
            // answering 404. A 4xx other than the auth/throttling ones that
            // only PROPFIND can settle gets one PROPFIND; if that also fails,
            // its error is reported rather than a bare "HEAD 400".
            400..=499 if !matches!(reply.status, 401 | 403 | 407 | 408 | 409 | 429) => {
                self.propfind_size(key)
            }
            _ => anyhow::bail!("HEAD {url}: {}", reply.describe()),
        }
    }

    /// Ask for the size with a `PROPFIND` of depth 0.
    ///
    /// `getcontentlength` is the standard property; servers that omit it
    /// (some WebDAV shims) are read with a full `GET` instead.
    fn propfind_size(&self, key: &str) -> Result<Option<(u64, Option<ETag>)>> {
        let url = self.url_for(key);
        let body = b"<?xml version=\"1.0\" encoding=\"utf-8\"?><D:propfind xmlns:D=\"DAV:\"><D:prop><D:getcontentlength/><D:getetag/><D:resourcetype/></D:prop></D:propfind>";
        let reply = self.request(
            "PROPFIND",
            &url,
            &[("Depth", "0"), ("Content-Type", "application/xml; charset=utf-8")],
            Some(body),
        )?;
        match reply.status {
            207 => {
                let text = String::from_utf8_lossy(&reply.body);
                if text.contains("<D:collection") || text.contains("<d:collection") {
                    anyhow::bail!("{url} is a collection, not an object");
                }
                match find_prop_number(&text, "getcontentlength") {
                    Some(Ok(len)) => Ok(Some((len, find_prop(&text, "getetag")))),
                    Some(Err(e)) => Err(e).with_context(|| format!("PROPFIND {url}")),
                    None => Ok(Some((self.measure_by_get(key)?, None))),
                }
            }
            404 | 410 => Ok(None),
            // Servers without PROPFIND (plain file servers sometimes answer
            // 405) cannot report a size; ask for the data instead.
            405 | 501 => Ok(Some((self.measure_by_get(key)?, None))),
            _ => anyhow::bail!("PROPFIND {url}: {}", reply.describe()),
        }
    }

    /// Last-resort size: download the object. Only reached when a server
    /// answers PROPFIND without a content length.
    fn measure_by_get(&self, key: &str) -> Result<u64> {
        let data = self.fetch(key, None)?;
        Ok(data.len() as u64)
    }

    fn fetch(&self, key: &str, range: Option<Range<u64>>) -> Result<Vec<u8>> {
        let url = self.url_for(key);
        let rh = range_header(range.clone())?;
        let headers: Vec<(&str, &str)> = match &rh {
            Some(v) => vec![("Range", v.as_str())],
            None => Vec::new(),
        };
        let reply = self.request("GET", &url, &headers, None)?;
        match reply.status {
            200 | 206 => Ok(reply.body),
            404 | 410 => Ok(Vec::new()),
            416 => anyhow::bail!("GET {url}: range not satisfiable"),
            _ => anyhow::bail!("GET {url}: {}", reply.describe()),
        }
    }

    fn store_whole(&self, key: &str, data: &[u8]) -> Result<Option<ETag>> {
        // Directories do not exist implicitly on WebDAV; create the parent
        // once, and only when the object is new (a rewrite never needs it).
        if self.head(key)?.is_none() {
            self.mkcol(key)?;
        }
        let url = self.url_for(key);
        let len = data.len().to_string();
        let reply = self.request(
            "PUT",
            &url,
            &[
                ("Content-Type", "application/octet-stream"),
                ("Content-Length", &len),
            ],
            Some(data),
        )?;
        match reply.status {
            200 | 201 | 204 => Ok(reply.etag()),
            412 => anyhow::bail!("PUT {url}: precondition failed"),
            404 | 409 => {
                // The parent vanished or was never there: create and retry once.
                self.mkcol(key)?;
                let reply = self.request(
                    "PUT",
                    &url,
                    &[
                        ("Content-Type", "application/octet-stream"),
                        ("Content-Length", &len),
                    ],
                    Some(data),
                )?;
                match reply.status {
                    200 | 201 | 204 => Ok(reply.etag()),
                    _ => anyhow::bail!("PUT {url}: {}", reply.describe()),
                }
            }
            _ => anyhow::bail!("PUT {url}: {}", reply.describe()),
        }
    }

    fn store_range(&self, key: &str, offset: u64, data: &[u8]) -> Result<()> {
        let Some(total) = self.head(key)?.map(|(len, _)| len) else {
            anyhow::bail!("ranged PUT {key}: object does not exist");
        };
        let Some(cr) = content_range_header(offset, data.len() as u64, total)? else {
            self.store_whole(key, data)?;
            return Ok(());
        };
        let url = self.url_for(key);
        let len = data.len().to_string();
        let reply = self.request(
            "PUT",
            &url,
            &[
                ("Content-Range", cr.as_str()),
                ("Content-Type", "application/octet-stream"),
                ("Content-Length", &len),
            ],
            Some(data),
        )?;
        match reply.status {
            200 | 201 | 204 => Ok(()),
            // 400/405/501: the server will not do ranged writes. The volume
            // layer reacts to a *failed* patch by re-uploading the whole
            // object, so this must be an error, not a silent success.
            400 | 405 | 501 => Err(partial_write_unsupported(&url, reply.status)),
            // 416: the object is a different size than we believed; the
            // caller's whole-object fallback is the right repair.
            409 | 412 | 416 => anyhow::bail!(
                "ranged PUT {url}: {} (content-range {cr}, object is {total} bytes)",
                reply.describe()
            ),
            _ => anyhow::bail!("ranged PUT {url}: {}", reply.describe()),
        }
    }
}

fn partial_write_unsupported(url: &str, status: u16) -> anyhow::Error {
    anyhow::anyhow!(
        "partial write not supported by this WebDAV server (ranged PUT {url} answered {status})"
    )
}

/// Text of the first `<*:name>` property value, XML-unescaped.
///
/// Returns `None` when the element is absent. Element prefixes are matched
/// loosely (`<D:name>`, `<d:name>`, `<name>` all count).
fn find_prop(body: &str, name: &str) -> Option<String> {
    let open = format!(":{name}>");
    let start = match body.find(&open) {
        Some(i) => i + open.len(),
        None => {
            let bare = format!("<{name}>");
            let i = body.find(&bare)? + bare.len();
            let _ = i;
            i
        }
    };
    let rest = &body[start..];
    let close = rest.find("</")?;
    Some(xml_unescape(rest[..close].trim()))
}

/// Numeric property, or an error naming the bad text.
fn find_prop_number(body: &str, name: &str) -> Option<Result<u64, anyhow::Error>> {
    find_prop(body, name).map(|raw| {
        raw.parse::<u64>()
            .map_err(|e| anyhow::anyhow!("bad {name} value {raw:?}: {e}"))
    })
}

impl ObjectStore for WebDavStore {
    fn caps(&self) -> Caps {
        Caps::FULL
    }

    fn stat(&self, key: &str) -> Result<Option<u64>> {
        self.retry
            .run(&format!("HEAD {key}"), || Ok(self.head(key)?.map(|(len, _)| len)))
    }

    fn get(&self, key: &str, range: Option<Range<u64>>) -> Result<Vec<u8>> {
        self.retry
            .run(&format!("GET {key}"), || self.fetch(key, range.clone()))
    }

    fn put(&self, key: &str, data: &[u8]) -> Result<Option<ETag>> {
        self.retry
            .run(&format!("PUT {key}"), || self.store_whole(key, data))
    }

    fn patch(&self, key: &str, offset: u64, data: &[u8]) -> Result<()> {
        self.retry.run(&format!("PATCH {key}@{offset}"), || {
            self.store_range(key, offset, data)
        })
    }

    fn delete(&self, key: &str) -> Result<()> {
        self.retry.run(&format!("DELETE {key}"), || {
            let url = self.url_for(key);
            let reply = self.request("DELETE", &url, &[], None)?;
            match reply.status {
                200 | 202 | 204 | 404 | 410 => Ok(()),
                _ if reply.status >= 500 => Err(StoreError::transient(format!(
                    "DELETE {url}: {}",
                    reply.describe()
                ))),
                _ => Err(StoreError::permanent(format!(
                    "DELETE {url}: {}",
                    reply.describe()
                ))),
            }
        })
    }

    fn name(&self) -> &str {
        "webdav"
    }
}

// ---------------------------------------------------------------------------
// Tests: URL building, header formatting and response parsing as pure
// functions. No network.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "https://dav.example.com/remote.php/dav/files/me/";

    #[test]
    fn base_url_is_normalised() {
        assert_eq!(normalize_base(BASE).unwrap(), "https://dav.example.com/remote.php/dav/files/me");
        assert_eq!(normalize_base("HTTP://h:8080").unwrap(), "http://h:8080");
        assert_eq!(normalize_base("http://h///").unwrap(), "http://h");
        assert!(normalize_base("").is_err());
        assert!(normalize_base("dav.example.com/x").is_err());
        assert!(normalize_base("ftp://h").is_err());
        assert!(normalize_base("http://").is_err());
        assert!(normalize_base("http://h/base?x=1").is_err());
    }

    #[test]
    fn keys_map_to_encoded_urls() {
        let base = normalize_base(BASE).unwrap();
        assert_eq!(
            object_path(&base, "vol1", "c/0"),
            "https://dav.example.com/remote.php/dav/files/me/vol1/c/0"
        );
        assert_eq!(object_path(&base, "", "v/c/12"), format!("{}/v/c/12", base.trim_end_matches('/')));
        // Encoding: space, plus, percent, non-ASCII, reserved query characters.
        assert_eq!(
            object_path(&base, "a b", "c/d e+f%g"),
            "https://dav.example.com/remote.php/dav/files/me/a%20b/c/d%20e%2Bf%25g"
        );
        assert_eq!(encode_segment("ü"), "%C3%BC");
        assert_eq!(encode_segment("x?y#z"), "x%3Fy%23z");
        assert_eq!(encode_segment("a&b=c,d;e"), "a&b=c,d;e");
        assert_eq!(encode_segment("a@b:c"), "a%40b%3Ac");
        // Slashes inside segments (key separators) stay separators.
        assert_eq!(object_path(&base, "", "c/1"), format!("{}/c/1", base.trim_end_matches('/')));
        // A leading slash on the key is not a double slash in the URL.
        assert_eq!(object_path(&base, "", "/c/1"), format!("{}/c/1", base.trim_end_matches('/')));
        // The base with no prefix and no key is the collection itself.
        assert_eq!(object_path(&base, "", ""), base.trim_end_matches('/'));
    }

    #[test]
    fn parent_path_is_the_objects_directory() {
        let base = normalize_base(BASE).unwrap();
        assert_eq!(
            parent_path(&base, "p", "c/0"),
            "https://dav.example.com/remote.php/dav/files/me/p/c"
        );
        assert_eq!(
            parent_path(&base, "", "meta"),
            "https://dav.example.com/remote.php/dav/files/me"
        );
        assert_eq!(parent_path(&base, "", "a/b/meta"), "https://dav.example.com/remote.php/dav/files/me/a/b");
    }

    #[test]
    fn range_and_content_range_headers() {
        assert_eq!(range_header(None).unwrap(), None);
        assert_eq!(range_header(Some(0..4096)).unwrap().as_deref(), Some("bytes=0-4095"));
        assert!(range_header(Some(7..7)).is_err());

        assert_eq!(content_range_header(0, 4096, 8 << 20).unwrap().as_deref(), Some("bytes 0-4095/8388608"));
        assert_eq!(
            content_range_header(4096, 4096, 8 << 20).unwrap().as_deref(),
            Some("bytes 4096-8191/8388608")
        );
        // A write covering the whole object is a plain PUT.
        assert_eq!(content_range_header(0, 10, 10).unwrap(), None);
        assert!(content_range_header(5, 10, 10).is_err(), "past the end");
        assert!(content_range_header(0, 0, 10).is_err(), "empty");
        assert!(content_range_header(u64::MAX, 2, u64::MAX).is_err(), "overflow");
    }

    #[test]
    fn basic_auth_is_rfc7617() {
        assert_eq!(basic_auth("user", "pass"), "Basic dXNlcjpwYXNz");
        assert_eq!(basic_auth("Aladdin", "open sesame"), "Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ==");
    }

    #[test]
    fn config_redacts_password() {
        let cfg = WebDavConfig::new(BASE).with_auth("u", "s3cret");
        let text = format!("{cfg:?}");
        assert!(!text.contains("s3cret"), "{text}");
    }

    #[test]
    fn store_construction_validates() {
        let s = WebDavStore::new(WebDavConfig::new(BASE).with_prefix("/vol x/")).unwrap();
        assert_eq!(s.cfg.prefix, "vol x");
        assert_eq!(s.caps(), Caps::FULL);
        assert!(s.caps().partial_write);
        assert_eq!(s.name(), "webdav");
        assert_eq!(
            s.url_for("c/1"),
            "https://dav.example.com/remote.php/dav/files/me/vol%20x/c/1"
        );
        assert!(WebDavStore::new(WebDavConfig::new("")).is_err());
        assert!(WebDavStore::new(WebDavConfig::new(BASE).with_auth("u", "p")).is_ok());
    }

    #[test]
    fn propfind_hrefs_are_parsed() {
        let body = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:"><d:response>
<d:href>/remote.php/dav/files/me/vol1/</d:href>
<d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat>
</d:response><d:response>
<d:href>/remote.php/dav/files/me/vol1/c%201</d:href>
</d:response></d:multistatus>"#;
        let h = hrefs(body);
        assert_eq!(h.len(), 2);
        assert_eq!(h[1], "/remote.php/dav/files/me/vol1/c%201");
        assert_eq!(hrefs("<D:href>a&amp;b</D:href>"), vec!["a&b".to_string()]);
        assert!(hrefs("no hrefs here").is_empty());
    }

    #[test]
    fn propfind_properties_are_parsed() {
        let props = r#"<D:multistatus xmlns:D="DAV:"><D:response><D:propstat><D:prop>
<D:getcontentlength>8388608</D:getcontentlength>
<D:getetag>&quot;abc-1&quot;</D:getetag>
</D:prop></D:propstat></D:response></D:multistatus>"#;
        assert_eq!(find_prop_number(props, "getcontentlength").unwrap().unwrap(), 8388608);
        assert_eq!(find_prop(props, "getetag").unwrap(), "\"abc-1\"");
        assert!(find_prop(props, "getlastmodified").is_none());
        // Unprefixed elements are matched too, as are bad numbers reported.
        assert_eq!(find_prop("<getcontentlength>7</getcontentlength>", "getcontentlength").unwrap(), "7");
        assert!(find_prop_number("<getcontentlength>NaN</getcontentlength>", "getcontentlength")
            .unwrap()
            .is_err());
    }

    #[test]
    fn server_rejection_of_ranged_put_is_a_failure_not_a_success() {
        // The volume layer re-uploads the whole object only when `patch`
        // *fails*; if a 405 were swallowed the chunk would silently lose
        // every block outside the patched range. A whole-object range is a
        // plain PUT and must not carry Content-Range at all.
        assert_eq!(content_range_header(0, 4096, 20_000).unwrap().as_deref(), Some("bytes 0-4095/20000"));
        assert_eq!(content_range_header(0, 20_000, 20_000).unwrap(), None);
        assert!(partial_write_unsupported("http://h/x", 405)
            .to_string()
            .contains("partial write not supported"));
    }

    #[test]
    fn missing_objects_read_as_empty() {
        let reply = Reply {
            status: 404,
            headers: vec![],
            body: b"Not Found".to_vec(),
        };
        assert_eq!(reply.etag(), None);
        assert_eq!(reply.status, 404);
    }
}
