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
    /// The reference RPC endpoint. Comes from `ARKIV_RPC_URL` in the
    /// environment — the same variable the writer sidecar reads.
    #[serde(skip)]
    pub reference: Option<String>,
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
    pub chain_id: Option<u64>,
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
            chain_id: None,
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
        for (name, duration) in [
            ("health.probe_interval", self.health.probe_interval),
            ("health.probe_timeout", self.health.probe_timeout),
            ("proxy.attempt_timeout", self.proxy.attempt_timeout),
            ("proxy.request_timeout", self.proxy.request_timeout),
        ] {
            if duration.is_zero() {
                return Err(ConfigError::Invalid(format!(
                    "{name} must be greater than zero"
                )));
            }
        }
        for (name, size) in [
            ("proxy.max_request_size", self.proxy.max_request_size),
            ("proxy.max_response_size", self.proxy.max_response_size),
        ] {
            if size.as_u64() == 0 {
                return Err(ConfigError::Invalid(format!(
                    "{name} must be greater than zero"
                )));
            }
        }
        for (name, interval) in [
            ("ref_height_interval", self.health.ref_height_interval),
            ("chainid_check_interval", self.health.chainid_check_interval),
        ] {
            // These are only sampled at probe rounds: a value below
            // probe_interval silently behaves as "every sweep", which
            // is what zero already says.
            if !interval.is_zero() && interval < self.health.probe_interval {
                return Err(ConfigError::Invalid(format!(
                    "health.{name} is shorter than health.probe_interval: checks happen \
                     on probe rounds, so use 0 (every round) or at least the probe interval"
                )));
            }
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
            match reqwest::Url::parse(&provider.url) {
                Err(error) => {
                    return Err(ConfigError::Invalid(format!(
                        "provider {id:?}: url {:?} does not parse: {error}",
                        provider.url
                    )));
                }
                Ok(url) if url.scheme() != "http" && url.scheme() != "https" => {
                    return Err(ConfigError::Invalid(format!(
                        "provider {id:?}: url {:?} is not http(s)",
                        provider.url
                    )));
                }
                Ok(_) => {}
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
    fn chain_id_parses_and_defaults_to_unchecked() {
        assert_eq!(parse("").expect("valid").health.chain_id, None);
        let config = parse("[health]\nchain_id = 1337\n").expect("valid");
        assert_eq!(config.health.chain_id, Some(1337));
    }

    #[test]
    fn zero_durations_and_sizes_are_refused() {
        for (section, field) in [
            ("health", "probe_interval = \"0s\""),
            ("health", "probe_timeout = \"0s\""),
            ("proxy", "attempt_timeout = \"0s\""),
            ("proxy", "request_timeout = \"0s\""),
            ("proxy", "max_request_size = \"0B\""),
            ("proxy", "max_response_size = \"0B\""),
        ] {
            let error = parse(&format!("[{section}]\n{field}\n")).expect_err("must refuse");
            let name = field.split(' ').next().expect("field name");
            assert!(error.to_string().contains(name), "{error}");
        }
    }

    #[test]
    fn the_probing_test_hook_is_not_reachable_from_toml() {
        parse("[health]\ndisable_probing = true\n")
            .expect_err("a serde-skipped field must stay unknown to the toml");
    }

    #[test]
    fn zero_chainid_check_interval_means_every_round() {
        let config = parse("[health]\nchainid_check_interval = \"0s\"\n").expect("zero is legal");
        assert!(config.health.chainid_check_interval.is_zero());
    }

    #[test]
    fn check_intervals_below_the_probe_interval_are_refused() {
        for name in ["ref_height_interval", "chainid_check_interval"] {
            let error = parse(&format!("[health]\n{name} = \"1s\"\n")).expect_err("must refuse");
            assert!(error.to_string().contains(name), "{error}");
            parse(&format!("[health]\n{name} = \"5s\"\n"))
                .expect("equal to probe_interval is fine");
        }
    }

    #[test]
    fn the_reference_endpoint_is_not_reachable_from_toml() {
        parse("reference = \"http://example.org\"\n")
            .expect_err("the reference comes from the environment only");
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
    fn non_http_schemes_are_refused() {
        for url in [
            "ftp://x",
            "ws://127.0.0.1:8546",
            "wss://example.org",
            "file:///etc/hosts",
            "unix:/var/run/node.sock",
        ] {
            let error = parse(&format!(
                "[[providers]]\nid = \"node-1\"\nurl = \"{url}\"\n"
            ))
            .expect_err("must refuse");
            assert!(error.to_string().contains("http"), "{url}: {error}");
        }
    }

    #[test]
    fn an_unparsable_url_is_refused_with_the_provider_named() {
        // "http://" passes a prefix check but is not a URL: no host.
        let error =
            parse("[[providers]]\nid = \"node-1\"\nurl = \"http://\"\n").expect_err("must refuse");
        assert!(error.to_string().contains("node-1"), "{error}");
        assert!(error.to_string().contains("does not parse"), "{error}");
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
