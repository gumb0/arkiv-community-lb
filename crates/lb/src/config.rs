//! Configuration: one `config.toml`, read once at startup — there is no
//! reload. The defaults below are the settled values; the committed
//! `config.example.toml` mirrors them, documents every field, and a test
//! keeps it parsing — field semantics are commented there.
//!
//! The reference endpoint is deliberately not here: the LB reads
//! `ARKIV_RPC_URL` / `ARKIV_API_KEY` from the environment — the same two
//! variables the chain-writer sidecar uses, so both components point at
//! the same endpoint and secrets never enter this file.

use std::{
    collections::HashSet,
    fs,
    net::{Ipv4Addr, SocketAddr},
    path::Path,
    time::Duration,
};

use bytesize::ByteSize;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub listen: Listen,
    pub health: Health,
    pub proxy: Proxy,
    pub providers: Vec<Provider>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Listen {
    pub public: SocketAddr,
    pub admin: SocketAddr,
}

impl Default for Listen {
    fn default() -> Self {
        Self {
            public: SocketAddr::from((Ipv4Addr::UNSPECIFIED, 8545)), // 0.0.0.0:8545
            admin: SocketAddr::from((Ipv4Addr::LOCALHOST, 9545)),    // 127.0.0.1:9545
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Health {
    #[serde(with = "humantime_serde")]
    pub probe_interval: Duration,
    #[serde(with = "humantime_serde")]
    pub probe_timeout: Duration,
    pub flip_after: u32,
    #[serde(with = "humantime_serde")]
    pub max_probe_backoff: Duration,
    #[serde(with = "humantime_serde")]
    pub ref_height_interval: Duration,
    #[serde(with = "humantime_serde")]
    pub chainid_check_interval: Duration,
    pub lag_tolerance_blocks: u64,
    /// Test hook, unreachable from the toml: proxy tests set eligibility
    /// by hand and must not race a probe sweep.
    #[serde(skip)]
    pub disable_probing: bool,
}

impl Default for Health {
    fn default() -> Self {
        Self {
            probe_interval: Duration::from_secs(5),
            probe_timeout: Duration::from_secs(2),
            flip_after: 3,
            max_probe_backoff: Duration::from_secs(5 * 60),
            ref_height_interval: Duration::ZERO,
            chainid_check_interval: Duration::from_secs(5 * 60),
            lag_tolerance_blocks: 30,
            disable_probing: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Proxy {
    #[serde(with = "humantime_serde")]
    pub attempt_timeout: Duration,
    #[serde(with = "humantime_serde")]
    pub request_timeout: Duration,
    pub max_retries: u32,
    pub max_request_size: ByteSize,
    pub max_response_size: ByteSize,
}

impl Default for Proxy {
    fn default() -> Self {
        Self {
            attempt_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(30),
            max_retries: 2,
            max_request_size: ByteSize::mib(2),
            max_response_size: ByteSize::mib(64),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    pub id: String,
    pub url: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {path}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} does not parse")]
    Parse {
        path: String,
        #[source]
        source: Box<toml::de::Error>,
    },
    #[error("{0}")]
    Invalid(String),
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.display().to_string(),
            source,
        })?;
        Self::parse(&text, &path.display().to_string())
    }

    fn parse(text: &str, path: &str) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(text).map_err(|source| ConfigError::Parse {
            path: path.to_string(),
            source: Box::new(source),
        })?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.health.probe_interval.is_zero() {
            return Err(ConfigError::Invalid(
                "health.probe_interval must be greater than zero".into(),
            ));
        }
        if self.health.flip_after < 2 {
            return Err(ConfigError::Invalid(format!(
                "health.flip_after is {}, minimum is 2: a provider's health may \
                 only change after at least two results in a row agree",
                self.health.flip_after
            )));
        }
        let mut seen = HashSet::new();
        for provider in &self.providers {
            let id = &provider.id;
            if id.is_empty()
                || !id
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            {
                return Err(ConfigError::Invalid(format!(
                    "provider id {id:?} is not a valid handle (lowercase letters, digits, '-')"
                )));
            }
            if !seen.insert(id) {
                return Err(ConfigError::Invalid(format!(
                    "provider id {id:?} appears twice"
                )));
            }
            if !provider.url.starts_with("http://") && !provider.url.starts_with("https://") {
                return Err(ConfigError::Invalid(format!(
                    "provider {id:?}: url {:?} is not http(s)",
                    provider.url
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Config, ConfigError> {
        Config::parse(text, "test")
    }

    #[test]
    fn empty_config_yields_the_settled_defaults() {
        let config = parse("").expect("empty config is valid");
        assert_eq!(config.health.probe_interval, Duration::from_secs(5));
        assert_eq!(config.health.flip_after, 3);
        assert_eq!(config.health.ref_height_interval, Duration::ZERO);
        assert_eq!(config.health.lag_tolerance_blocks, 30);
        assert_eq!(config.proxy.max_retries, 2);
        assert_eq!(config.proxy.max_response_size, ByteSize::mib(64));
        assert_eq!(config.listen.public.port(), 8545);
        assert!(config.providers.is_empty());
    }

    #[test]
    fn zero_probe_interval_is_refused() {
        let error = parse("[health]\nprobe_interval = \"0s\"\n").expect_err("must refuse");
        assert!(error.to_string().contains("probe_interval"), "{error}");
    }

    #[test]
    fn the_probing_test_hook_is_not_reachable_from_toml() {
        parse("[health]\ndisable_probing = true\n")
            .expect_err("a serde-skipped field must stay unknown to the toml");
    }

    #[test]
    fn flip_after_below_two_refuses_with_the_invariant_named() {
        let error = parse("[health]\nflip_after = 1\n").expect_err("must refuse");
        assert!(error.to_string().contains("flip_after"), "{error}");
        assert!(error.to_string().contains("in a row"), "{error}");
    }

    #[test]
    fn duplicate_provider_id_is_named() {
        let error = parse(
            "[[providers]]\nid = \"node-1\"\nurl = \"http://127.0.0.1:1\"\n\
             [[providers]]\nid = \"node-1\"\nurl = \"http://127.0.0.1:2\"\n",
        )
        .expect_err("must refuse");
        assert!(error.to_string().contains("node-1"), "{error}");
    }

    #[test]
    fn provider_id_charset_is_enforced() {
        let error = parse("[[providers]]\nid = \"Node_1\"\nurl = \"http://127.0.0.1:1\"\n")
            .expect_err("must refuse");
        assert!(error.to_string().contains("Node_1"), "{error}");
    }

    #[test]
    fn http_and_https_urls_are_accepted() {
        let config = parse(
            "[[providers]]\nid = \"node-1\"\nurl = \"http://127.0.0.1:18545\"\n\
             [[providers]]\nid = \"node-2\"\nurl = \"https://example.org\"\n",
        )
        .expect("http and https providers are valid");
        assert_eq!(config.providers.len(), 2);
    }

    #[test]
    fn non_http_url_is_refused() {
        let error =
            parse("[[providers]]\nid = \"node-1\"\nurl = \"ftp://x\"\n").expect_err("must refuse");
        assert!(error.to_string().contains("http"), "{error}");
    }

    #[test]
    fn unknown_keys_are_errors_not_silence() {
        let error = parse("[health]\nprobe_intreval = \"5s\"\n").expect_err("must refuse");
        // The detail sits in the error's source, where the binary's
        // chain-printer surfaces it — assert on the chain, not Display.
        let mut rendered = error.to_string();
        let mut source = std::error::Error::source(&error);
        while let Some(cause) = source {
            rendered.push_str(&format!(": {cause}"));
            source = cause.source();
        }
        assert!(rendered.contains("probe_intreval"), "{rendered}");
    }

    #[test]
    fn the_committed_example_parses_and_validates() {
        parse(include_str!("../../../config.example.toml")).expect("example must stay valid");
    }
}
