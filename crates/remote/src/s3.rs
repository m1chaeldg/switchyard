//! AWS S3 and S3-compatible object storage backend implementing [`RemoteFs`].
//!
//! Supports:
//! - AWS Profile authentication (`~/.aws/credentials` and `~/.aws/config`).
//! - Static AWS credentials (access key, secret key, session token).
//! - AWS SigV4 request signing.
//! - Virtual-host and path-style addressing.
//! - Custom S3 endpoints (MinIO, LocalStack, Cloudflare R2, Wasabi).
//! - Listing buckets, object prefixes, streaming upload/download, resume, delete, rename.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use futures::future::BoxFuture;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use crate::fs::{
    EntryKind, FileEntry, FsError, FsReader, FsWriter, RemoteFs, file_name, sort_entries,
};

const EMPTY_PAYLOAD_SHA256: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// AWS authentication credentials.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AwsCredentials {
    /// AWS Access Key ID.
    pub access_key_id: String,
    /// AWS Secret Access Key.
    pub secret_access_key: String,
    /// Optional session token for temporary / SSO credentials.
    pub session_token: Option<String>,
}

/// Configuration for an S3 connection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct S3Config {
    /// AWS Region (e.g. "us-east-1").
    pub region: String,
    /// Specific bucket, or None to list all accessible buckets at root.
    pub bucket: Option<String>,
    /// Optional custom endpoint URL (e.g. `http://localhost:9000`).
    pub endpoint: Option<String>,
    /// Force path-style addressing (`http://endpoint/bucket/key`).
    pub force_path_style: bool,
    /// Credentials.
    pub credentials: AwsCredentials,
}

/// S3 file-system backend.
#[derive(Clone)]
pub struct S3Fs {
    config: S3Config,
    client: reqwest::Client,
}

impl S3Fs {
    /// Create a new S3 file-system client.
    pub fn new(config: S3Config) -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = reqwest::Client::builder()
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { config, client }
    }

    /// Access configuration.
    pub fn config(&self) -> &S3Config {
        &self.config
    }
}

// ---------------------------------------------------------------------------
// Profile & Credentials Loader
// ---------------------------------------------------------------------------

/// Locate AWS credentials and config files.
pub fn aws_config_paths() -> (PathBuf, PathBuf) {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    let creds_path = std::env::var("AWS_SHARED_CREDENTIALS_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home.join(".aws").join("credentials"));

    let config_path = std::env::var("AWS_CONFIG_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home.join(".aws").join("config"));

    (creds_path, config_path)
}

/// Parse an INI file into sections and key-value pairs.
fn parse_ini(content: &str) -> BTreeMap<String, BTreeMap<String, String>> {
    let mut sections: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut current_section = String::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }

        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            let section_name = trimmed[1..trimmed.len() - 1].trim();
            // In config files, named profiles are often "[profile dev]", normalize to "dev"
            let normalized = if let Some(stripped) = section_name.strip_prefix("profile ") {
                stripped.trim().to_string()
            } else {
                section_name.to_string()
            };
            current_section = normalized;
            sections.entry(current_section.clone()).or_default();
            continue;
        }

        if let Some((key, value)) = trimmed.split_once('=') {
            let k = key.trim().to_lowercase();
            let v = value.trim().to_string();
            if !current_section.is_empty() {
                sections
                    .entry(current_section.clone())
                    .or_default()
                    .insert(k, v);
            }
        }
    }

    sections
}

/// Load credentials and region for an AWS named profile from `~/.aws/credentials` and `~/.aws/config`.
pub fn load_aws_profile(profile: &str) -> Result<(AwsCredentials, Option<String>), FsError> {
    let (creds_path, config_path) = aws_config_paths();

    let mut key_id = None;
    let mut secret = None;
    let mut token = None;
    let mut region = None;

    if creds_path.exists() {
        let text = std::fs::read_to_string(&creds_path)
            .map_err(|e| FsError::Remote(format!("failed to read {}: {e}", creds_path.display())))?;
        let ini = parse_ini(&text);
        if let Some(sec) = ini.get(profile) {
            key_id = sec.get("aws_access_key_id").cloned();
            secret = sec.get("aws_secret_access_key").cloned();
            token = sec.get("aws_session_token").cloned();
        }
    }

    if config_path.exists() {
        let text = std::fs::read_to_string(&config_path)
            .map_err(|e| FsError::Remote(format!("failed to read {}: {e}", config_path.display())))?;
        let ini = parse_ini(&text);
        if let Some(sec) = ini.get(profile) {
            if key_id.is_none() {
                key_id = sec.get("aws_access_key_id").cloned();
            }
            if secret.is_none() {
                secret = sec.get("aws_secret_access_key").cloned();
            }
            if token.is_none() {
                token = sec.get("aws_session_token").cloned();
            }
            region = sec.get("region").cloned();
        }
    }

    // Fallback to environment variables if profile is "default" and not found
    if profile == "default" {
        if key_id.is_none() {
            key_id = std::env::var("AWS_ACCESS_KEY_ID").ok();
        }
        if secret.is_none() {
            secret = std::env::var("AWS_SECRET_ACCESS_KEY").ok();
        }
        if token.is_none() {
            token = std::env::var("AWS_SESSION_TOKEN").ok();
        }
        if region.is_none() {
            region = std::env::var("AWS_REGION")
                .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
                .ok();
        }
    }

    match (key_id, secret) {
        (Some(access_key_id), Some(secret_access_key)) => Ok((
            AwsCredentials {
                access_key_id,
                secret_access_key,
                session_token: token,
            },
            region,
        )),
        _ => Err(FsError::Remote(format!(
            "AWS profile '{profile}' not found in {} or {}",
            creds_path.display(),
            config_path.display()
        ))),
    }
}

// ---------------------------------------------------------------------------
// AWS SigV4 Signer
// ---------------------------------------------------------------------------

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let s_key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key);
    ring::hmac::sign(&s_key, data).as_ref().to_vec()
}

fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

/// URL encode a string per RFC 3986 (with unreserved characters `-_.~`).
fn uri_encode(s: &str, encode_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.' || b == b'~' {
            out.push(b as char);
        } else if b == b'/' && !encode_slash {
            out.push('/');
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

/// Sign an HTTP request using AWS Signature Version 4.
pub fn sign_request(
    method: &str,
    url: &Url,
    headers: &mut HeaderMap,
    payload_sha256: &str,
    creds: &AwsCredentials,
    region: &str,
    date_time: DateTime<Utc>,
) {
    let date_stamp = date_time.format("%Y%m%d").to_string();
    let amz_date = date_time.format("%Y%m%dT%H%M%SZ").to_string();

    headers.insert(
        HeaderName::from_static("x-amz-date"),
        HeaderValue::from_str(&amz_date).unwrap(),
    );
    headers.insert(
        HeaderName::from_static("x-amz-content-sha256"),
        HeaderValue::from_str(payload_sha256).unwrap(),
    );

    if let Some(host) = url.host_str() {
        let host_val = match url.port() {
            Some(port) => format!("{}:{}", host, port),
            None => host.to_string(),
        };
        headers.insert(
            reqwest::header::HOST,
            HeaderValue::from_str(&host_val).unwrap(),
        );
    }

    if let Some(ref token) = creds.session_token {
        headers.insert(
            HeaderName::from_static("x-amz-security-token"),
            HeaderValue::from_str(token).unwrap(),
        );
    }

    // 1. Canonical URI
    let canonical_uri = if url.path().is_empty() {
        "/".to_string()
    } else {
        uri_encode(url.path(), false)
    };

    // 2. Canonical Query String
    let mut query_params: Vec<(String, String)> = url
        .query_pairs()
        .map(|(k, v)| (uri_encode(&k, true), uri_encode(&v, true)))
        .collect();
    query_params.sort();
    let canonical_query = query_params
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");

    // 3. Canonical Headers
    let mut header_pairs: Vec<(String, String)> = Vec::new();
    for (name, val) in headers.iter() {
        let n = name.as_str().to_lowercase();
        let v = val.to_str().unwrap_or("").trim().to_string();
        header_pairs.push((n, v));
    }
    header_pairs.sort_by(|a, b| a.0.cmp(&b.0));

    let signed_headers = header_pairs
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");

    let canonical_headers = header_pairs
        .iter()
        .map(|(k, v)| format!("{k}:{v}\n"))
        .collect::<Vec<_>>()
        .concat();

    // 4. Canonical Request
    let canonical_request = format!(
        "{method}\n{canonical_uri}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{payload_sha256}"
    );

    let hashed_canonical_request = sha256_hex(canonical_request.as_bytes());

    // 5. String to Sign
    let credential_scope = format!("{date_stamp}/{region}/s3/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{credential_scope}\n{hashed_canonical_request}"
    );

    // 6. Signature Derivation
    let k_secret = format!("AWS4{}", creds.secret_access_key);
    let k_date = hmac_sha256(k_secret.as_bytes(), date_stamp.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, b"s3");
    let k_signing = hmac_sha256(&k_service, b"aws4_request");
    let signature = hex::encode(hmac_sha256(&k_signing, string_to_sign.as_bytes()));

    let auth_header = format!(
        "AWS4-HMAC-SHA256 Credential={}/{credential_scope}, SignedHeaders={signed_headers}, Signature={signature}",
        creds.access_key_id
    );

    headers.insert(
        reqwest::header::AUTHORIZATION,
        HeaderValue::from_str(&auth_header).unwrap(),
    );
}

// ---------------------------------------------------------------------------
// XML Parsing for S3 REST Responses
// ---------------------------------------------------------------------------

fn extract_xml_text(s: &str) -> String {
    quick_xml::escape::unescape(s)
        .map(|c| c.into_owned())
        .unwrap_or_else(|_| s.to_string())
}

/// Parse bucket names from `<ListAllMyBucketsResult>`.
pub fn parse_list_buckets(xml: &str) -> Result<Vec<String>, FsError> {
    use quick_xml::events::Event;
    use quick_xml::reader::Reader;

    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut buckets = Vec::new();
    let mut in_bucket = false;
    let mut in_name = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) => match e.name().as_ref() {
                "Bucket" => in_bucket = true,
                "Name" if in_bucket => in_name = true,
                _ => {}
            },
            Ok(Event::Text(ref e)) if in_name => {
                let name = extract_xml_text(e.as_ref());
                if !name.is_empty() {
                    buckets.push(name);
                }
            }
            Ok(Event::End(ref e)) => match e.name().as_ref() {
                "Bucket" => in_bucket = false,
                "Name" => in_name = false,
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(e) => return Err(FsError::Remote(format!("invalid XML in bucket list: {e}"))),
            _ => {}
        }
    }

    Ok(buckets)
}

/// S3 object entry parsed from XML.
#[derive(Debug, Clone, PartialEq)]
pub struct S3Object {
    pub key: String,
    pub size: u64,
    pub last_modified: Option<i64>,
}

/// Parse objects and prefixes from `<ListBucketResult>` (ListObjectsV2).
pub fn parse_list_objects_v2(
    xml: &str,
) -> Result<(Vec<S3Object>, Vec<String>, bool, Option<String>), FsError> {
    use quick_xml::events::Event;
    use quick_xml::reader::Reader;

    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut objects = Vec::new();
    let mut prefixes = Vec::new();
    let mut is_truncated = false;
    let mut next_token = None;

    let mut in_contents = false;
    let mut in_common_prefixes = false;
    let mut current_tag = String::new();

    let mut key = String::new();
    let mut size: u64 = 0;
    let mut last_modified = None;

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) => {
                let name = e.name().as_ref().to_string();
                match name.as_str() {
                    "Contents" => {
                        in_contents = true;
                        key.clear();
                        size = 0;
                        last_modified = None;
                    }
                    "CommonPrefixes" => in_common_prefixes = true,
                    _ => current_tag = name,
                }
            }
            Ok(Event::Text(ref e)) => {
                let text = extract_xml_text(e.as_ref());
                if in_contents {
                    match current_tag.as_str() {
                        "Key" => key = text,
                        "Size" => size = text.parse().unwrap_or(0),
                        "LastModified" => {
                            if let Ok(dt) = DateTime::parse_from_rfc3339(&text) {
                                last_modified = Some(dt.timestamp_millis());
                            }
                        }
                        _ => {}
                    }
                } else if in_common_prefixes && current_tag == "Prefix" {
                    prefixes.push(text);
                } else {
                    match current_tag.as_str() {
                        "IsTruncated" => is_truncated = text.eq_ignore_ascii_case("true"),
                        "NextContinuationToken" => next_token = Some(text),
                        _ => {}
                    }
                }
            }
            Ok(Event::End(ref e)) => {
                match e.name().as_ref() {
                    "Contents" => {
                        in_contents = false;
                        if !key.is_empty() {
                            objects.push(S3Object {
                                key: key.clone(),
                                size,
                                last_modified,
                            });
                        }
                    }
                    "CommonPrefixes" => in_common_prefixes = false,
                    _ => {}
                }
                current_tag.clear();
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(FsError::Remote(format!("invalid XML in object list: {e}"))),
            _ => {}
        }
    }

    Ok((objects, prefixes, is_truncated, next_token))
}

/// Parse S3 error message from XML response.
pub fn parse_s3_error(xml: &str) -> Option<String> {
    use quick_xml::events::Event;
    use quick_xml::reader::Reader;

    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut code = String::new();
    let mut message = String::new();
    let mut current_tag = String::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) => {
                current_tag = e.name().as_ref().to_string();
            }
            Ok(Event::Text(ref e)) => {
                let text = extract_xml_text(e.as_ref());
                match current_tag.as_str() {
                    "Code" => code = text,
                    "Message" => message = text,
                    _ => {}
                }
            }
            Ok(Event::End(_)) => current_tag.clear(),
            Ok(Event::Eof) => break,
            _ => {}
        }
    }

    if !code.is_empty() || !message.is_empty() {
        Some(format!("{}: {}", code, message))
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Path and URL Resolution
// ---------------------------------------------------------------------------

impl S3Fs {
    /// Normalize a path into `(Option<bucket>, Option<key>)`.
    ///
    /// If `self.config.bucket` is configured:
    /// - `/` -> `(Some(bucket), None)`
    /// - `/foo/bar` -> `(Some(bucket), Some("foo/bar"))`
    ///
    /// If `self.config.bucket` is None:
    /// - `/` -> `(None, None)` (bucket list)
    /// - `/my-bucket` -> `(Some("my-bucket"), None)`
    /// - `/my-bucket/foo/bar` -> `(Some("my-bucket"), Some("foo/bar"))`
    pub fn resolve_path(&self, path: &Path) -> (Option<String>, Option<String>) {
        let p = path.to_string_lossy().replace('\\', "/");
        let trimmed = p.trim_start_matches('/');

        if let Some(ref bucket) = self.config.bucket {
            let key = if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            };
            (Some(bucket.clone()), key)
        } else if trimmed.is_empty() {
            (None, None)
        } else {
            match trimmed.split_once('/') {
                Some((bucket, key)) => {
                    let k = if key.is_empty() {
                        None
                    } else {
                        Some(key.to_string())
                    };
                    (Some(bucket.to_string()), k)
                }
                None => (Some(trimmed.to_string()), None),
            }
        }
    }

    /// Construct the base endpoint URL.
    fn base_url(&self, bucket: Option<&str>) -> Url {
        if let Some(ref custom) = self.config.endpoint {
            let mut u = Url::parse(custom).unwrap_or_else(|_| {
                Url::parse(&format!("https://{}", custom)).expect("valid endpoint url")
            });
            if self.config.force_path_style || bucket.is_none() {
                if let Some(b) = bucket {
                    let path = format!("{}/{}", u.path().trim_end_matches('/'), b);
                    u.set_path(&path);
                }
            } else if let Some(b) = bucket {
                let host = u.host_str().unwrap_or("");
                u.set_host(Some(&format!("{b}.{host}"))).ok();
            }
            u
        } else {
            // Standard AWS S3
            let region = &self.config.region;
            let host = if self.config.force_path_style || bucket.is_none() {
                if region == "us-east-1" {
                    "s3.amazonaws.com".to_string()
                } else {
                    format!("s3.{region}.amazonaws.com")
                }
            } else {
                let b = bucket.unwrap();
                if region == "us-east-1" {
                    format!("{b}.s3.amazonaws.com")
                } else {
                    format!("{b}.s3.{region}.amazonaws.com")
                }
            };

            let mut u = Url::parse(&format!("https://{host}")).unwrap();
            if self.config.force_path_style {
                if let Some(b) = bucket {
                    u.set_path(&format!("/{b}"));
                }
            }
            u
        }
    }

    /// Build and sign an S3 request.
    pub fn build_request(
        &self,
        method: &str,
        bucket: Option<&str>,
        key: Option<&str>,
        query: &[(&str, &str)],
        payload_sha256: &str,
    ) -> Result<reqwest::RequestBuilder, FsError> {
        let mut url = self.base_url(bucket);

        if let Some(k) = key {
            let current = url.path().trim_end_matches('/');
            let joined = if current.is_empty() {
                format!("/{}", k)
            } else {
                format!("{}/{}", current, k)
            };
            url.set_path(&joined);
        }

        if !query.is_empty() {
            let mut pairs = url.query_pairs_mut();
            for (k, v) in query {
                pairs.append_pair(k, v);
            }
        }

        let mut headers = HeaderMap::new();
        sign_request(
            method,
            &url,
            &mut headers,
            payload_sha256,
            &self.config.credentials,
            &self.config.region,
            Utc::now(),
        );

        let req_method = match method {
            "GET" => reqwest::Method::GET,
            "PUT" => reqwest::Method::PUT,
            "DELETE" => reqwest::Method::DELETE,
            "HEAD" => reqwest::Method::HEAD,
            _ => reqwest::Method::GET,
        };

        Ok(self.client.request(req_method, url).headers(headers))
    }
}

// ---------------------------------------------------------------------------
// RemoteFs Implementation
// ---------------------------------------------------------------------------

impl RemoteFs for S3Fs {
    fn name(&self) -> &str {
        "S3"
    }

    fn home(&self) -> PathBuf {
        PathBuf::from("/")
    }

    fn list<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, Result<Vec<FileEntry>, FsError>> {
        Box::pin(async move {
            let (bucket, key) = self.resolve_path(path);

            // 1. Root listing with no pinned bucket -> list all buckets
            if bucket.is_none() {
                let req = self.build_request("GET", None, None, &[], EMPTY_PAYLOAD_SHA256)?;
                let resp = req.send().await.map_err(|e| FsError::Remote(e.to_string()))?;
                let text = resp.text().await.map_err(|e| FsError::Remote(e.to_string()))?;

                if let Some(err_msg) = parse_s3_error(&text) {
                    return Err(FsError::Remote(err_msg));
                }

                let buckets = parse_list_buckets(&text)?;
                let mut out: Vec<FileEntry> = buckets
                    .into_iter()
                    .map(|name| FileEntry {
                        name,
                        kind: EntryKind::Dir,
                        size: 0,
                        modified_ms: None,
                        mode: None,
                    })
                    .collect();
                sort_entries(&mut out);
                return Ok(out);
            }

            // 2. Listing inside a bucket
            let b = bucket.as_deref().unwrap();
            let mut prefix = key.unwrap_or_default();
            if !prefix.is_empty() && !prefix.ends_with('/') {
                prefix.push('/');
            }

            let mut out = Vec::new();
            let mut continuation_token: Option<String> = None;

            loop {
                let mut query = vec![("list-type", "2"), ("delimiter", "/")];
                if !prefix.is_empty() {
                    query.push(("prefix", &prefix));
                }
                if let Some(ref token) = continuation_token {
                    query.push(("continuation-token", token));
                }

                let req = self.build_request("GET", Some(b), None, &query, EMPTY_PAYLOAD_SHA256)?;
                let resp = req.send().await.map_err(|e| FsError::Remote(e.to_string()))?;
                let text = resp.text().await.map_err(|e| FsError::Remote(e.to_string()))?;

                if let Some(err_msg) = parse_s3_error(&text) {
                    return Err(FsError::Remote(err_msg));
                }

                let (objects, prefixes, is_truncated, next_token) = parse_list_objects_v2(&text)?;

                for dir_prefix in prefixes {
                    let clean = dir_prefix.trim_end_matches('/');
                    let name = clean
                        .rsplit_once('/')
                        .map(|(_, name)| name)
                        .unwrap_or(clean)
                        .to_string();
                    if !name.is_empty() {
                        out.push(FileEntry {
                            name,
                            kind: EntryKind::Dir,
                            size: 0,
                            modified_ms: None,
                            mode: None,
                        });
                    }
                }

                for obj in objects {
                    // Skip the directory marker itself
                    if obj.key == prefix {
                        continue;
                    }
                    let name = obj
                        .key
                        .rsplit_once('/')
                        .map(|(_, name)| name)
                        .unwrap_or(&obj.key)
                        .to_string();

                    // If key ends with '/', treat as dir
                    if obj.key.ends_with('/') {
                        let dir_name = obj.key.trim_end_matches('/');
                        let dname = dir_name
                            .rsplit_once('/')
                            .map(|(_, name)| name)
                            .unwrap_or(dir_name)
                            .to_string();
                        if !dname.is_empty() {
                            out.push(FileEntry {
                                name: dname,
                                kind: EntryKind::Dir,
                                size: 0,
                                modified_ms: obj.last_modified,
                                mode: None,
                            });
                        }
                    } else {
                        out.push(FileEntry {
                            name,
                            kind: EntryKind::File,
                            size: obj.size,
                            modified_ms: obj.last_modified,
                            mode: None,
                        });
                    }
                }

                if is_truncated && next_token.is_some() {
                    continuation_token = next_token;
                } else {
                    break;
                }
            }

            sort_entries(&mut out);
            Ok(out)
        })
    }

    fn mkdir<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, Result<(), FsError>> {
        Box::pin(async move {
            let (bucket, key) = self.resolve_path(path);
            match (bucket, key) {
                // Root bucket creation
                (Some(b), None) if self.config.bucket.is_none() => {
                    let req = self.build_request("PUT", Some(&b), None, &[], EMPTY_PAYLOAD_SHA256)?;
                    let resp = req.send().await.map_err(|e| FsError::Remote(e.to_string()))?;
                    if !resp.status().is_success() {
                        let text = resp.text().await.unwrap_or_default();
                        return Err(FsError::Remote(
                            parse_s3_error(&text).unwrap_or_else(|| "failed to create bucket".into()),
                        ));
                    }
                    Ok(())
                }
                // Directory marker object (folder/)
                (Some(b), Some(k)) => {
                    let marker_key = if k.ends_with('/') { k } else { format!("{k}/") };
                    let req = self.build_request(
                        "PUT",
                        Some(&b),
                        Some(&marker_key),
                        &[],
                        EMPTY_PAYLOAD_SHA256,
                    )?;
                    let resp = req
                        .body(Vec::<u8>::new())
                        .send()
                        .await
                        .map_err(|e| FsError::Remote(e.to_string()))?;
                    if !resp.status().is_success() {
                        let text = resp.text().await.unwrap_or_default();
                        return Err(FsError::Remote(
                            parse_s3_error(&text).unwrap_or_else(|| "failed to create folder".into()),
                        ));
                    }
                    Ok(())
                }
                _ => Err(FsError::Unsupported("cannot mkdir at S3 root")),
            }
        })
    }

    fn rename<'a>(&'a self, from: &'a Path, to: &'a Path) -> BoxFuture<'a, Result<(), FsError>> {
        Box::pin(async move {
            let (src_bucket, src_key) = self.resolve_path(from);
            let (dst_bucket, dst_key) = self.resolve_path(to);

            let (sb, sk) = match (src_bucket, src_key) {
                (Some(b), Some(k)) => (b, k),
                _ => return Err(FsError::Unsupported("cannot rename root S3 bucket")),
            };
            let (db, dk) = match (dst_bucket, dst_key) {
                (Some(b), Some(k)) => (b, k),
                _ => return Err(FsError::Unsupported("cannot rename to S3 root")),
            };

            // S3 CopyObject: PUT to destination with x-amz-copy-source: /source-bucket/source-key
            let copy_source = format!("/{sb}/{sk}");
            let mut headers = HeaderMap::new();
            headers.insert(
                HeaderName::from_static("x-amz-copy-source"),
                HeaderValue::from_str(&copy_source)
                    .map_err(|e| FsError::Remote(e.to_string()))?,
            );

            let mut req = self.build_request(
                "PUT",
                Some(&db),
                Some(&dk),
                &[],
                EMPTY_PAYLOAD_SHA256,
            )?;
            req = req.headers(headers);

            let resp = req.send().await.map_err(|e| FsError::Remote(e.to_string()))?;
            if !resp.status().is_success() {
                let text = resp.text().await.unwrap_or_default();
                return Err(FsError::Remote(
                    parse_s3_error(&text).unwrap_or_else(|| "failed to copy object".into()),
                ));
            }

            // Delete original object
            self.delete(from).await?;
            Ok(())
        })
    }

    fn delete<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, Result<(), FsError>> {
        Box::pin(async move {
            let (bucket, key) = self.resolve_path(path);
            match (bucket, key) {
                (Some(b), Some(k)) => {
                    // Try deleting key, and also key/ marker if it exists
                    let req = self.build_request(
                        "DELETE",
                        Some(&b),
                        Some(&k),
                        &[],
                        EMPTY_PAYLOAD_SHA256,
                    )?;
                    let resp = req.send().await.map_err(|e| FsError::Remote(e.to_string()))?;
                    if !resp.status().is_success() {
                        let text = resp.text().await.unwrap_or_default();
                        return Err(FsError::Remote(
                            parse_s3_error(&text).unwrap_or_else(|| "failed to delete object".into()),
                        ));
                    }

                    // Also try deleting marker key if not ending with '/'
                    if !k.ends_with('/') {
                        let marker = format!("{k}/");
                        let req_marker = self.build_request(
                            "DELETE",
                            Some(&b),
                            Some(&marker),
                            &[],
                            EMPTY_PAYLOAD_SHA256,
                        )?;
                        let _ = req_marker.send().await;
                    }

                    Ok(())
                }
                (Some(b), None) if self.config.bucket.is_none() => {
                    // Delete bucket
                    let req = self.build_request(
                        "DELETE",
                        Some(&b),
                        None,
                        &[],
                        EMPTY_PAYLOAD_SHA256,
                    )?;
                    let resp = req.send().await.map_err(|e| FsError::Remote(e.to_string()))?;
                    if !resp.status().is_success() {
                        let text = resp.text().await.unwrap_or_default();
                        return Err(FsError::Remote(
                            parse_s3_error(&text).unwrap_or_else(|| "failed to delete bucket".into()),
                        ));
                    }
                    Ok(())
                }
                _ => Err(FsError::Unsupported("cannot delete S3 root")),
            }
        })
    }

    fn stat<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, Result<FileEntry, FsError>> {
        Box::pin(async move {
            let (bucket, key) = self.resolve_path(path);
            let name = file_name(path);

            match (bucket, key) {
                (None, None) => Ok(FileEntry {
                    name: "s3".into(),
                    kind: EntryKind::Dir,
                    size: 0,
                    modified_ms: None,
                    mode: None,
                }),
                (Some(_), None) => Ok(FileEntry {
                    name,
                    kind: EntryKind::Dir,
                    size: 0,
                    modified_ms: None,
                    mode: None,
                }),
                (Some(b), Some(k)) => {
                    // 1. Try HEAD object
                    let req = self.build_request(
                        "HEAD",
                        Some(&b),
                        Some(&k),
                        &[],
                        EMPTY_PAYLOAD_SHA256,
                    )?;
                    let resp = req.send().await.map_err(|e| FsError::Remote(e.to_string()))?;

                    if resp.status().is_success() {
                        let size = resp.content_length().unwrap_or(0);
                        let modified_ms = resp
                            .headers()
                            .get(reqwest::header::LAST_MODIFIED)
                            .and_then(|v| v.to_str().ok())
                            .and_then(|s| DateTime::parse_from_rfc2822(s).ok())
                            .map(|dt| dt.timestamp_millis());

                        let is_dir = k.ends_with('/');
                        return Ok(FileEntry {
                            name,
                            kind: if is_dir { EntryKind::Dir } else { EntryKind::File },
                            size: if is_dir { 0 } else { size },
                            modified_ms,
                            mode: None,
                        });
                    }

                    // 2. Check if prefix folder exists
                    let prefix = if k.ends_with('/') { k.clone() } else { format!("{k}/") };
                    let query = [("list-type", "2"), ("prefix", &prefix), ("max-keys", "1")];
                    let req = self.build_request("GET", Some(&b), None, &query, EMPTY_PAYLOAD_SHA256)?;
                    let resp = req.send().await.map_err(|e| FsError::Remote(e.to_string()))?;
                    let text = resp.text().await.unwrap_or_default();
                    let (objects, prefixes, _, _) = parse_list_objects_v2(&text).unwrap_or_default();

                    if !objects.is_empty() || !prefixes.is_empty() {
                        return Ok(FileEntry {
                            name,
                            kind: EntryKind::Dir,
                            size: 0,
                            modified_ms: None,
                            mode: None,
                        });
                    }

                    Err(FsError::Io(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!("{k}: not found on S3"),
                    )))
                }
                (None, Some(_)) => Err(FsError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "object not found",
                ))),
            }
        })
    }

    fn open_read<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, Result<FsReader, FsError>> {
        self.open_read_from(path, 0)
    }

    fn open_read_from<'a>(
        &'a self,
        path: &'a Path,
        offset: u64,
    ) -> BoxFuture<'a, Result<FsReader, FsError>> {
        Box::pin(async move {
            let (bucket, key) = self.resolve_path(path);
            let (b, k) = match (bucket, key) {
                (Some(b), Some(k)) => (b, k),
                _ => return Err(FsError::Unsupported("cannot read directory as file")),
            };

            let mut req = self.build_request(
                "GET",
                Some(&b),
                Some(&k),
                &[],
                EMPTY_PAYLOAD_SHA256,
            )?;

            if offset > 0 {
                req = req.header("Range", format!("bytes={offset}-"));
            }

            let resp = req.send().await.map_err(|e| FsError::Remote(e.to_string()))?;
            if !resp.status().is_success() {
                let text = resp.text().await.unwrap_or_default();
                return Err(FsError::Remote(
                    parse_s3_error(&text).unwrap_or_else(|| "failed to read object".into()),
                ));
            }

            let bytes = resp.bytes().await.map_err(|e| FsError::Remote(e.to_string()))?;
            let cursor = std::io::Cursor::new(bytes);
            Ok(Box::new(cursor) as FsReader)
        })
    }

    fn create<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, Result<FsWriter, FsError>> {
        self.open_write_from(path, 0)
    }

    fn open_write_from<'a>(
        &'a self,
        path: &'a Path,
        offset: u64,
    ) -> BoxFuture<'a, Result<FsWriter, FsError>> {
        Box::pin(async move {
            let (bucket, key) = self.resolve_path(path);
            let (b, k) = match (bucket, key) {
                (Some(b), Some(k)) => (b, k),
                _ => return Err(FsError::Unsupported("cannot write to S3 directory")),
            };

            let mut initial_data = Vec::new();
            if offset > 0 {
                // Fetch existing content up to offset
                let mut req = self.build_request(
                    "GET",
                    Some(&b),
                    Some(&k),
                    &[],
                    EMPTY_PAYLOAD_SHA256,
                )?;
                req = req.header("Range", format!("bytes=0-{}", offset - 1));
                if let Ok(resp) = req.send().await
                    && resp.status().is_success()
                    && let Ok(bytes) = resp.bytes().await
                {
                    initial_data = bytes.to_vec();
                }
            }

            let writer = S3Writer {
                fs: self.clone(),
                bucket: b,
                key: k,
                buffer: initial_data,
                upload_task: None,
            };

            Ok(Box::new(writer) as FsWriter)
        })
    }
}

// ---------------------------------------------------------------------------
// S3 Async Writer
// ---------------------------------------------------------------------------

struct S3Writer {
    fs: S3Fs,
    bucket: String,
    key: String,
    buffer: Vec<u8>,
    upload_task: Option<tokio::task::JoinHandle<Result<(), std::io::Error>>>,
}

impl tokio::io::AsyncWrite for S3Writer {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<Result<usize, std::io::Error>> {
        self.buffer.extend_from_slice(buf);
        std::task::Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        if self.upload_task.is_none() {
            let fs = self.fs.clone();
            let bucket = self.bucket.clone();
            let key = self.key.clone();
            let data = std::mem::take(&mut self.buffer);

            let handle = tokio::spawn(async move {
                let payload_sha = sha256_hex(&data);
                let req = fs
                    .build_request("PUT", Some(&bucket), Some(&key), &[], &payload_sha)
                    .map_err(|e| std::io::Error::other(e.to_string()))?;

                let resp = req
                    .body(data)
                    .send()
                    .await
                    .map_err(|e| std::io::Error::other(e.to_string()))?;

                if !resp.status().is_success() {
                    let text = resp.text().await.unwrap_or_default();
                    return Err(std::io::Error::other(
                        parse_s3_error(&text).unwrap_or_else(|| "failed to upload object".into()),
                    ));
                }

                Ok(())
            });
            self.upload_task = Some(handle);
        }

        if let Some(ref mut handle) = self.upload_task {
            use std::future::Future;
            match std::pin::Pin::new(handle).poll(cx) {
                std::task::Poll::Ready(Ok(Ok(()))) => std::task::Poll::Ready(Ok(())),
                std::task::Poll::Ready(Ok(Err(e))) => std::task::Poll::Ready(Err(e)),
                std::task::Poll::Ready(Err(join_err)) => {
                    std::task::Poll::Ready(Err(std::io::Error::other(join_err.to_string())))
                }
                std::task::Poll::Pending => std::task::Poll::Pending,
            }
        } else {
            std::task::Poll::Ready(Ok(()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_aws_ini() {
        let creds = r#"
[default]
aws_access_key_id = AKIAIOSFODNN7EXAMPLE
aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY

[profile dev]
aws_access_key_id = AKIADEVEXAMPLE
aws_secret_access_key = devsecret123
region = eu-west-1
"#;
        let ini = parse_ini(creds);
        let def = ini.get("default").unwrap();
        assert_eq!(
            def.get("aws_access_key_id").unwrap(),
            "AKIAIOSFODNN7EXAMPLE"
        );
        let dev = ini.get("dev").unwrap();
        assert_eq!(dev.get("aws_access_key_id").unwrap(), "AKIADEVEXAMPLE");
        assert_eq!(dev.get("region").unwrap(), "eu-west-1");
    }

    #[test]
    fn parses_list_buckets_xml() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListAllMyBucketsResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
    <Owner>
        <ID>bcaf1ffd86f461ca5fb16fd081034f</ID>
        <DisplayName>webfile</DisplayName>
    </Owner>
    <Buckets>
        <Bucket>
            <Name>quotes</Name>
            <CreationDate>2006-02-03T16:45:09.000Z</CreationDate>
        </Bucket>
        <Bucket>
            <Name>samples</Name>
            <CreationDate>2006-02-03T16:41:58.000Z</CreationDate>
        </Bucket>
    </Buckets>
</ListAllMyBucketsResult>"#;
        let buckets = parse_list_buckets(xml).unwrap();
        assert_eq!(buckets, vec!["quotes", "samples"]);
    }

    #[test]
    fn parses_list_objects_v2_xml() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
    <Name>my-bucket</Name>
    <Prefix>photos/</Prefix>
    <KeyCount>2</KeyCount>
    <MaxKeys>1000</MaxKeys>
    <IsTruncated>false</IsTruncated>
    <Contents>
        <Key>photos/cat.jpg</Key>
        <LastModified>2026-10-09T12:00:00.000Z</LastModified>
        <ETag>"70ee1738b6b21e2c8a43f3a5ab0eee71"</ETag>
        <Size>43008</Size>
        <StorageClass>STANDARD</StorageClass>
    </Contents>
    <CommonPrefixes>
        <Prefix>photos/2026/</Prefix>
    </CommonPrefixes>
</ListBucketResult>"#;
        let (objects, prefixes, truncated, _) = parse_list_objects_v2(xml).unwrap();
        assert!(!truncated);
        assert_eq!(prefixes, vec!["photos/2026/"]);
        assert_eq!(objects.len(), 1);
        assert_eq!(objects[0].key, "photos/cat.jpg");
        assert_eq!(objects[0].size, 43008);
    }

    #[test]
    fn resolves_paths_correctly() {
        let creds = AwsCredentials {
            access_key_id: "test".into(),
            secret_access_key: "test".into(),
            session_token: None,
        };

        // Without pinned bucket
        let fs = S3Fs::new(S3Config {
            region: "us-east-1".into(),
            bucket: None,
            endpoint: None,
            force_path_style: false,
            credentials: creds.clone(),
        });
        assert_eq!(fs.resolve_path(Path::new("/")), (None, None));
        assert_eq!(
            fs.resolve_path(Path::new("/my-bucket")),
            (Some("my-bucket".into()), None)
        );
        assert_eq!(
            fs.resolve_path(Path::new("/my-bucket/docs/plan.md")),
            (Some("my-bucket".into()), Some("docs/plan.md".into()))
        );

        // With pinned bucket
        let fs_pinned = S3Fs::new(S3Config {
            region: "us-east-1".into(),
            bucket: Some("my-bucket".into()),
            endpoint: None,
            force_path_style: false,
            credentials: creds,
        });
        assert_eq!(
            fs_pinned.resolve_path(Path::new("/")),
            (Some("my-bucket".into()), None)
        );
        assert_eq!(
            fs_pinned.resolve_path(Path::new("/docs/plan.md")),
            (Some("my-bucket".into()), Some("docs/plan.md".into()))
        );
    }

    #[test]
    fn test_sigv4_signing() {
        let creds = AwsCredentials {
            access_key_id: "AKIAIOSFODNN7EXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
            session_token: None,
        };
        let url = Url::parse("https://my-bucket.s3.us-east-1.amazonaws.com/test.txt").unwrap();
        let mut headers = HeaderMap::new();
        let dt = DateTime::parse_from_rfc3339("2026-10-09T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        sign_request(
            "GET",
            &url,
            &mut headers,
            EMPTY_PAYLOAD_SHA256,
            &creds,
            "us-east-1",
            dt,
        );

        assert!(headers.contains_key("x-amz-date"));
        assert_eq!(
            headers.get("x-amz-date").unwrap().to_str().unwrap(),
            "20261009T120000Z"
        );
        assert!(headers.contains_key("authorization"));
        let auth = headers.get("authorization").unwrap().to_str().unwrap();
        assert!(auth.starts_with(
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20261009/us-east-1/s3/aws4_request"
        ));
        assert!(auth.contains("SignedHeaders="));
        assert!(auth.contains("Signature="));
    }
}
