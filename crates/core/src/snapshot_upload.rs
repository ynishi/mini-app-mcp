/// S3-compatible snapshot upload for the `data_snapshot` MCP tool.
///
/// This module implements **S3 Protocol support, not AWS-specific support**:
/// the endpoint is injected via environment variables, so AWS S3, Backblaze B2
/// (S3-Compatible API), Cloudflare R2, and MinIO all work through the same
/// code path.
///
/// # Feature gating
///
/// The network `put` path (and the `object_store` dependency) is compiled only
/// with the `s3-upload` cargo feature. **Configuration resolution is compiled
/// unconditionally** so that callers can produce a precise
/// `UPLOAD_NOT_CONFIGURED` error (missing env vs disabled feature) in every
/// build flavour, and so the pure resolution logic stays unit-testable without
/// the heavy dependency.
///
/// # Configuration (environment only)
///
/// Credentials and endpoints are read from `MINI_APP_S3_*` environment
/// variables (never from tool-call arguments) so that secrets never travel
/// through the LLM-visible MCP layer. The variables ride the existing
/// `.mini-app-mcp.env` (dotenvy) loading path.
///
/// | env | required | meaning |
/// |---|---|---|
/// | `MINI_APP_S3_ENDPOINT` | yes | S3-compatible endpoint URL |
/// | `MINI_APP_S3_BUCKET` | yes | bucket name |
/// | `MINI_APP_S3_ACCESS_KEY_ID` | yes | access key id |
/// | `MINI_APP_S3_SECRET_ACCESS_KEY` | yes | secret access key |
/// | `MINI_APP_S3_PREFIX` | no | key prefix (default `mini-app-snapshots/`) |
/// | `MINI_APP_S3_REGION` | no | signing region (default `us-east-1`; B2: match the endpoint region) |
/// | `MINI_APP_S3_VIRTUAL_HOSTED_STYLE` | no | `true` = virtual-hosted addressing; default `false` = path style (MinIO-compatible) |
/// | `MINI_APP_S3_CHECKSUM` | no | `sha256` = send `x-amz-checksum-sha256` on put; default `none` (some S3-compatible providers reject checksum headers) |
///
/// # Failure semantics
///
/// - Missing configuration is detected **before** any snapshot is written
///   (the caller checks first) — [`MiniAppError::UploadNotConfigured`].
/// - A failed upload of an individual snapshot file is **non-fatal** for the
///   `data_snapshot` call: the local snapshot and purge results stand, and the
///   error is reported in the `upload_errors[]` response field.
/// - Remote retention is intentionally out of scope: old objects are expected
///   to be expired by bucket lifecycle rules (KNOWN LIMITATION).
use std::collections::HashMap;

use crate::error::MiniAppError;

/// Env var: S3-compatible endpoint URL (required).
pub const ENV_ENDPOINT: &str = "MINI_APP_S3_ENDPOINT";
/// Env var: bucket name (required).
pub const ENV_BUCKET: &str = "MINI_APP_S3_BUCKET";
/// Env var: access key id (required).
pub const ENV_ACCESS_KEY_ID: &str = "MINI_APP_S3_ACCESS_KEY_ID";
/// Env var: secret access key (required).
pub const ENV_SECRET_ACCESS_KEY: &str = "MINI_APP_S3_SECRET_ACCESS_KEY";
/// Env var: key prefix (optional).
pub const ENV_PREFIX: &str = "MINI_APP_S3_PREFIX";
/// Env var: signing region (optional).
pub const ENV_REGION: &str = "MINI_APP_S3_REGION";
/// Env var: addressing style (optional, `true`/`false`).
pub const ENV_VIRTUAL_HOSTED_STYLE: &str = "MINI_APP_S3_VIRTUAL_HOSTED_STYLE";
/// Env var: upload checksum algorithm (optional, `none`/`sha256`).
pub const ENV_CHECKSUM: &str = "MINI_APP_S3_CHECKSUM";

/// Default key prefix used when `MINI_APP_S3_PREFIX` is not set.
pub const DEFAULT_PREFIX: &str = "mini-app-snapshots/";

/// Dummy signing region used when `MINI_APP_S3_REGION` is not set.
///
/// Several S3-compatible providers accept any region string but the SigV4
/// signer requires one; B2 rejects mismatched regions, so B2 users must set
/// `MINI_APP_S3_REGION` to the region embedded in their endpoint.
pub const DEFAULT_REGION: &str = "us-east-1";

/// Resolved upload configuration for an S3-compatible destination.
#[derive(Debug, Clone)]
pub struct S3UploadConfig {
    /// S3-compatible endpoint URL (e.g. `https://s3.us-west-004.backblazeb2.com`).
    pub endpoint: String,
    /// Bucket name.
    pub bucket: String,
    /// Access key id.
    pub access_key_id: String,
    /// Secret access key.
    pub secret_access_key: String,
    /// Key prefix; joined with the snapshot file name by [`S3UploadConfig::key_for`].
    pub prefix: String,
    /// Optional signing region ([`DEFAULT_REGION`] is used when absent).
    pub region: Option<String>,
    /// Addressing style: `true` = virtual-hosted (`bucket.endpoint/key`),
    /// `false` = path style (`endpoint/bucket/key`, default — required by
    /// MinIO and accepted by AWS / B2 / R2).
    pub virtual_hosted_style: bool,
    /// When `true`, puts carry an `x-amz-checksum-sha256` header (the only
    /// algorithm `object_store` supports). Default `false` — some
    /// S3-compatible providers reject checksum headers with
    /// `400 InvalidArgument: Unsupported header`.
    pub checksum_sha256: bool,
}

impl S3UploadConfig {
    /// Resolves the configuration from an arbitrary variable map.
    ///
    /// Pure function over `vars` — no process-environment access — so unit
    /// tests can exercise every branch without mutating `std::env` (which is
    /// unsafe under parallel test execution).
    ///
    /// Empty-string values are treated as missing.
    ///
    /// # Errors
    /// - [`MiniAppError::UploadNotConfigured`] listing every missing required
    ///   variable by name.
    pub fn from_vars(vars: &HashMap<String, String>) -> Result<Self, MiniAppError> {
        let get = |key: &str| -> Option<String> {
            vars.get(key)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };

        let mut missing: Vec<&str> = Vec::new();
        let endpoint = get(ENV_ENDPOINT);
        if endpoint.is_none() {
            missing.push(ENV_ENDPOINT);
        }
        let bucket = get(ENV_BUCKET);
        if bucket.is_none() {
            missing.push(ENV_BUCKET);
        }
        let access_key_id = get(ENV_ACCESS_KEY_ID);
        if access_key_id.is_none() {
            missing.push(ENV_ACCESS_KEY_ID);
        }
        let secret_access_key = get(ENV_SECRET_ACCESS_KEY);
        if secret_access_key.is_none() {
            missing.push(ENV_SECRET_ACCESS_KEY);
        }

        if !missing.is_empty() {
            return Err(MiniAppError::UploadNotConfigured(format!(
                "missing env: {}",
                missing.join(", ")
            )));
        }

        let virtual_hosted_style = match get(ENV_VIRTUAL_HOSTED_STYLE).as_deref() {
            None => false,
            Some(v) if v.eq_ignore_ascii_case("true") || v == "1" => true,
            Some(v) if v.eq_ignore_ascii_case("false") || v == "0" => false,
            Some(other) => {
                return Err(MiniAppError::UploadNotConfigured(format!(
                    "{ENV_VIRTUAL_HOSTED_STYLE} must be true/false, got '{other}'"
                )));
            }
        };

        let checksum_sha256 = match get(ENV_CHECKSUM).as_deref() {
            None => false,
            Some(v) if v.eq_ignore_ascii_case("none") => false,
            Some(v) if v.eq_ignore_ascii_case("sha256") => true,
            Some(other) => {
                return Err(MiniAppError::UploadNotConfigured(format!(
                    "{ENV_CHECKSUM} must be none/sha256, got '{other}'"
                )));
            }
        };

        // SAFETY of unwraps: all four options were verified Some above
        // (missing.is_empty() implies each individual check passed).
        Ok(S3UploadConfig {
            endpoint: endpoint.unwrap(),
            bucket: bucket.unwrap(),
            access_key_id: access_key_id.unwrap(),
            secret_access_key: secret_access_key.unwrap(),
            prefix: get(ENV_PREFIX).unwrap_or_else(|| DEFAULT_PREFIX.to_string()),
            region: get(ENV_REGION),
            virtual_hosted_style,
            checksum_sha256,
        })
    }

    /// Resolves the configuration from the process environment.
    ///
    /// # Errors
    /// - [`MiniAppError::UploadNotConfigured`] when required variables are
    ///   missing (see [`S3UploadConfig::from_vars`]).
    pub fn from_env() -> Result<Self, MiniAppError> {
        let vars: HashMap<String, String> = std::env::vars().collect();
        Self::from_vars(&vars)
    }

    /// Builds the object key for a snapshot file name by joining `prefix` and
    /// `file_name` with exactly one `/` (an empty prefix yields the bare name).
    pub fn key_for(&self, file_name: &str) -> String {
        let trimmed = self.prefix.trim_end_matches('/');
        if trimmed.is_empty() {
            file_name.to_string()
        } else {
            format!("{}/{}", trimmed, file_name)
        }
    }
}

/// Whether this binary was built with the `s3-upload` feature.
///
/// Callers use this to distinguish "feature disabled" from "env incomplete"
/// in `UPLOAD_NOT_CONFIGURED` messages.
pub const fn upload_feature_enabled() -> bool {
    cfg!(feature = "s3-upload")
}

/// Uploads one local snapshot file to `{bucket}/{key}` on the configured
/// S3-compatible endpoint. Returns the number of bytes uploaded.
///
/// The whole file is read into memory before the put — snapshot files are
/// SQLite databases of modest size (mini-app tables are small by design), so
/// multipart streaming is intentionally not implemented.
///
/// # Errors
/// - [`MiniAppError::Upload`] on local read failure, client construction
///   failure, or a rejected/failed put.
#[cfg(feature = "s3-upload")]
pub async fn upload_snapshot(
    config: &S3UploadConfig,
    local_path: &std::path::Path,
    key: &str,
) -> Result<u64, MiniAppError> {
    use object_store::ObjectStore;
    use object_store::aws::AmazonS3Builder;

    let bytes = tokio::fs::read(local_path)
        .await
        .map_err(|e| MiniAppError::Upload(format!("cannot read snapshot file: {e}")))?;
    let len = bytes.len() as u64;

    let mut builder = AmazonS3Builder::new()
        .with_endpoint(&config.endpoint)
        .with_bucket_name(&config.bucket)
        .with_access_key_id(&config.access_key_id)
        .with_secret_access_key(&config.secret_access_key)
        // SigV4 requires a region even for providers that ignore it; B2 users
        // must set MINI_APP_S3_REGION to match their endpoint region.
        .with_region(config.region.as_deref().unwrap_or(DEFAULT_REGION))
        .with_virtual_hosted_style_request(config.virtual_hosted_style);
    if config.checksum_sha256 {
        builder = builder.with_checksum_algorithm(object_store::aws::Checksum::SHA256);
    }
    // Allow plain-http endpoints (local MinIO smoke); TLS endpoints unaffected.
    if config.endpoint.starts_with("http://") {
        builder = builder.with_allow_http(true);
    }
    let store = builder
        .build()
        .map_err(|e| MiniAppError::Upload(format!("cannot build s3 client: {e}")))?;

    let object_path = object_store::path::Path::from(key);
    store
        .put(&object_path, bytes::Bytes::from(bytes).into())
        .await
        .map_err(|e| MiniAppError::Upload(format!("put '{key}' failed: {e}")))?;

    Ok(len)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_vars() -> HashMap<String, String> {
        HashMap::from([
            (ENV_ENDPOINT.to_string(), "https://s3.example.com".to_string()),
            (ENV_BUCKET.to_string(), "my-bucket".to_string()),
            (ENV_ACCESS_KEY_ID.to_string(), "AKID".to_string()),
            (ENV_SECRET_ACCESS_KEY.to_string(), "SECRET".to_string()),
        ])
    }

    /// T1: full required vars resolve, optionals fall back to defaults.
    #[test]
    fn from_vars_resolves_with_defaults() {
        let config = S3UploadConfig::from_vars(&full_vars()).expect("must resolve");
        assert_eq!(config.endpoint, "https://s3.example.com");
        assert_eq!(config.bucket, "my-bucket");
        assert_eq!(config.prefix, DEFAULT_PREFIX);
        assert_eq!(config.region, None);
        assert!(!config.virtual_hosted_style, "default must be path style");
        assert!(!config.checksum_sha256, "default must send no checksum");
    }

    /// T2: checksum flag parses none/sha256 strictly.
    #[test]
    fn from_vars_checksum_parse() {
        for (raw, expected) in [("none", false), ("NONE", false), ("sha256", true), ("SHA256", true)] {
            let mut vars = full_vars();
            vars.insert(ENV_CHECKSUM.to_string(), raw.to_string());
            let config = S3UploadConfig::from_vars(&vars).expect("must resolve");
            assert_eq!(config.checksum_sha256, expected, "raw value '{raw}'");
        }

        let mut vars = full_vars();
        vars.insert(ENV_CHECKSUM.to_string(), "crc32".to_string());
        let err = S3UploadConfig::from_vars(&vars).expect_err("unsupported algo must fail");
        let MiniAppError::UploadNotConfigured(msg) = &err else {
            panic!("expected UploadNotConfigured, got {err:?}");
        };
        assert!(
            msg.contains(ENV_CHECKSUM),
            "message must name the offending var: {msg}"
        );
    }

    /// T2: addressing-style flag parses true/false variants strictly.
    #[test]
    fn from_vars_virtual_hosted_style_parse() {
        for (raw, expected) in [("true", true), ("TRUE", true), ("1", true), ("false", false), ("0", false)] {
            let mut vars = full_vars();
            vars.insert(ENV_VIRTUAL_HOSTED_STYLE.to_string(), raw.to_string());
            let config = S3UploadConfig::from_vars(&vars).expect("must resolve");
            assert_eq!(config.virtual_hosted_style, expected, "raw value '{raw}'");
        }

        let mut vars = full_vars();
        vars.insert(ENV_VIRTUAL_HOSTED_STYLE.to_string(), "maybe".to_string());
        let err = S3UploadConfig::from_vars(&vars).expect_err("junk value must fail");
        let MiniAppError::UploadNotConfigured(msg) = &err else {
            panic!("expected UploadNotConfigured, got {err:?}");
        };
        assert!(
            msg.contains(ENV_VIRTUAL_HOSTED_STYLE),
            "message must name the offending var: {msg}"
        );
    }

    /// T1: optional vars are picked up when present.
    #[test]
    fn from_vars_resolves_optionals() {
        let mut vars = full_vars();
        vars.insert(ENV_PREFIX.to_string(), "backups/mini".to_string());
        vars.insert(ENV_REGION.to_string(), "us-west-004".to_string());
        let config = S3UploadConfig::from_vars(&vars).expect("must resolve");
        assert_eq!(config.prefix, "backups/mini");
        assert_eq!(config.region.as_deref(), Some("us-west-004"));
    }

    /// T3: empty map lists every missing required var by name.
    #[test]
    fn from_vars_empty_reports_all_missing() {
        let err = S3UploadConfig::from_vars(&HashMap::new()).expect_err("must fail");
        let MiniAppError::UploadNotConfigured(msg) = &err else {
            panic!("expected UploadNotConfigured, got {err:?}");
        };
        for var in [
            ENV_ENDPOINT,
            ENV_BUCKET,
            ENV_ACCESS_KEY_ID,
            ENV_SECRET_ACCESS_KEY,
        ] {
            assert!(msg.contains(var), "message must name '{var}': {msg}");
        }
        assert_eq!(err.code(), crate::error::codes::UPLOAD_NOT_CONFIGURED);
    }

    /// T2: empty-string values count as missing (e.g. `MINI_APP_S3_BUCKET=`).
    #[test]
    fn from_vars_empty_string_counts_as_missing() {
        let mut vars = full_vars();
        vars.insert(ENV_BUCKET.to_string(), "  ".to_string());
        let err = S3UploadConfig::from_vars(&vars).expect_err("must fail");
        let MiniAppError::UploadNotConfigured(msg) = &err else {
            panic!("expected UploadNotConfigured, got {err:?}");
        };
        assert!(msg.contains(ENV_BUCKET), "message must name bucket: {msg}");
        assert!(
            !msg.contains(ENV_ENDPOINT),
            "endpoint was provided and must not be listed: {msg}"
        );
    }

    /// T2: key_for joins with exactly one slash regardless of prefix shape.
    #[test]
    fn key_for_prefix_join() {
        let mut config = S3UploadConfig::from_vars(&full_vars()).expect("must resolve");

        config.prefix = "snaps/".to_string();
        assert_eq!(config.key_for("issue.100.db"), "snaps/issue.100.db");

        config.prefix = "snaps".to_string();
        assert_eq!(config.key_for("issue.100.db"), "snaps/issue.100.db");

        config.prefix = String::new();
        assert_eq!(config.key_for("issue.100.db"), "issue.100.db");
    }
}
