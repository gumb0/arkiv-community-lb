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

use alloy_primitives::U256;
use bytesize::ByteSize;
use serde::Deserialize;

use crate::chain::{reader::PAGE_LIMIT, records::Wei};

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub listen: Listen,
    pub health: Health,
    pub proxy: Proxy,
    /// Absent: the LB runs on the static providers alone, with no
    /// discovery, no listing, no tunnel admission, and no Arkiv
    /// endpoint needed at startup.
    pub marketplace: Option<Marketplace>,
    pub providers: Vec<Provider>,
    /// The reference RPC endpoint. Comes from `ARKIV_RPC_URL` in the
    /// environment — the same variable the writer sidecar reads.
    #[serde(skip)]
    pub reference: Option<String>,
    /// Bearer token for the reference endpoint, from `ARKIV_API_KEY`;
    /// sent to the reference only, never to a provider.
    #[serde(skip)]
    pub reference_key: Option<String>,
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
            ref_height_interval: Duration::from_secs(60),
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

/// The marketplace runs when this section is present. Two fields are a
/// deployment's own and have no default: the price and the tunnel
/// address. The section as a whole has no default either, so the field
/// defaults are given one by one.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Marketplace {
    #[serde(default = "default_writer_url")]
    pub writer_url: String,
    pub wei_per_call: Wei,
    /// This host's tunnel address, `host:port`.
    pub tunnel_server: String,
    #[serde(default = "default_max_providers")]
    pub max_providers: u32,
    /// The slots' tunnel ports start here, one per slot.
    #[serde(default = "default_remote_port_start")]
    pub remote_port_start: u16,
    #[serde(with = "humantime_serde", default = "default_discovery_interval")]
    pub discovery_interval: Duration,
    #[serde(with = "humantime_serde", default = "default_accept_window")]
    pub accept_window: Duration,
    #[serde(with = "humantime_serde", default = "default_refresh_interval")]
    pub refresh_interval: Duration,
    #[serde(with = "humantime_serde", default = "default_agreement_life")]
    pub agreement_life: Duration,
    #[serde(with = "humantime_serde", default = "default_offer_max_lifetime")]
    pub offer_max_lifetime: Duration,
    #[serde(default = "default_offer_max_lag_blocks")]
    pub offer_max_lag_blocks: u64,
    /// In the toml as GLM, held as wei.
    #[serde(
        deserialize_with = "deserialize_glm",
        default = "default_gas_warn_below"
    )]
    pub gas_warn_below: Wei,
}

fn default_writer_url() -> String {
    "http://127.0.0.1:8560/".to_owned()
}
fn default_max_providers() -> u32 {
    100
}
fn default_remote_port_start() -> u16 {
    20000
}
fn default_discovery_interval() -> Duration {
    Duration::from_secs(5 * 60)
}
fn default_accept_window() -> Duration {
    Duration::from_secs(2 * 60 * 60)
}
fn default_refresh_interval() -> Duration {
    Duration::from_secs(60 * 60)
}
fn default_agreement_life() -> Duration {
    Duration::from_secs(3 * 24 * 60 * 60)
}
fn default_offer_max_lifetime() -> Duration {
    Duration::from_secs(2 * 24 * 60 * 60)
}
fn default_offer_max_lag_blocks() -> u64 {
    1000
}
fn default_gas_warn_below() -> Wei {
    glm("0.02").expect("the default parses")
}

impl Marketplace {
    /// One port per slot, from the start port up.
    pub fn remote_ports(&self) -> impl Iterator<Item = u16> {
        (self.remote_port_start..=u16::MAX).take(self.max_providers as usize)
    }
}

/// A GLM amount written the human way, "0.02", to wei: eighteen
/// decimals, no exponent, no sign.
fn glm(text: &str) -> Result<Wei, String> {
    const DECIMALS: usize = 18;
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    if whole.is_empty()
        || fraction.len() > DECIMALS
        || !whole
            .chars()
            .chain(fraction.chars())
            .all(|c| c.is_ascii_digit())
    {
        return Err(format!(
            "{text:?} is not a GLM amount: digits with at most {DECIMALS} decimals"
        ));
    }
    let digits = format!("{whole}{fraction:0<DECIMALS$}");
    U256::from_str_radix(&digits, 10)
        .map(Wei)
        .map_err(|_| format!("{text:?} is too large"))
}

fn deserialize_glm<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Wei, D::Error> {
    let text = String::deserialize(deserializer)?;
    glm(&text).map_err(serde::de::Error::custom)
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
            validate_http_url(&format!("provider {id:?}: url"), &provider.url)?;
        }
        match &self.marketplace {
            Some(marketplace) => marketplace.validate(),
            None => Ok(()),
        }
    }
}

fn validate_http_url(what: &str, url: &str) -> Result<(), ConfigError> {
    match reqwest::Url::parse(url) {
        Err(error) => Err(ConfigError::Invalid(format!(
            "{what} {url:?} does not parse: {error}"
        ))),
        Ok(parsed) if parsed.scheme() != "http" && parsed.scheme() != "https" => Err(
            ConfigError::Invalid(format!("{what} {url:?} is not http(s)")),
        ),
        Ok(_) => Ok(()),
    }
}

impl Marketplace {
    fn validate(&self) -> Result<(), ConfigError> {
        validate_http_url("marketplace.writer_url", &self.writer_url)?;
        for (name, duration) in [
            ("discovery_interval", self.discovery_interval),
            ("accept_window", self.accept_window),
            ("refresh_interval", self.refresh_interval),
            ("agreement_life", self.agreement_life),
            ("offer_max_lifetime", self.offer_max_lifetime),
        ] {
            if duration.is_zero() {
                return Err(ConfigError::Invalid(format!(
                    "marketplace.{name} must be greater than zero"
                )));
            }
        }
        // A lifetime is sent to the sidecar in seconds and becomes
        // whole blocks at two seconds each; anything else is refused
        // at the write, better refused here.
        for (name, duration) in [
            ("accept_window", self.accept_window),
            ("agreement_life", self.agreement_life),
            ("offer_max_lifetime", self.offer_max_lifetime),
        ] {
            if duration.subsec_nanos() != 0 || duration.as_secs() % 2 != 0 {
                return Err(ConfigError::Invalid(format!(
                    "marketplace.{name} must be an even number of seconds: lifetimes \
                     are whole blocks of two seconds"
                )));
            }
        }
        if u64::from(self.max_providers) >= PAGE_LIMIT {
            return Err(ConfigError::Invalid(format!(
                "marketplace.max_providers is {}, maximum is {}: the agreement records \
                 are reloaded in one page of {PAGE_LIMIT} at startup",
                self.max_providers,
                PAGE_LIMIT - 1
            )));
        }
        let last = u32::from(self.remote_port_start) + self.max_providers;
        if last > u32::from(u16::MAX) + 1 {
            return Err(ConfigError::Invalid(format!(
                "marketplace.remote_port_start {} plus max_providers {} runs past port 65535",
                self.remote_port_start, self.max_providers
            )));
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

    /// The error with its whole cause chain, the way the binary prints
    /// it: a parse error's detail sits in the source, not in Display.
    fn rendered(error: &ConfigError) -> String {
        let mut rendered = error.to_string();
        let mut source = std::error::Error::source(error);
        while let Some(cause) = source {
            rendered.push_str(&format!(": {cause}"));
            source = cause.source();
        }
        rendered
    }

    #[test]
    fn empty_config_yields_the_settled_defaults() {
        let config = parse("").expect("empty config is valid");
        assert_eq!(config.health.probe_interval, Duration::from_secs(5));
        assert_eq!(config.health.flip_after, 3);
        assert_eq!(config.health.ref_height_interval, Duration::from_secs(60));
        assert_eq!(config.health.lag_tolerance_blocks, 30);
        assert_eq!(config.proxy.max_retries, 2);
        assert_eq!(config.proxy.max_response_size, ByteSize::mib(64));
        assert_eq!(config.listen.public.port(), 8545);
        assert!(config.providers.is_empty());
        assert!(config.marketplace.is_none(), "no section, no marketplace");
    }

    /// The two lines a deployment must write; everything else in the
    /// section has a default.
    const MARKETPLACE: &str = "[marketplace]\nwei_per_call = \"1000000000000000\"\ntunnel_server = \"203.0.113.10:7000\"\n";

    #[test]
    fn the_marketplace_section_yields_the_settled_defaults() {
        let config = parse(MARKETPLACE).expect("valid");
        let marketplace = config.marketplace.expect("the section is present");
        assert_eq!(marketplace.writer_url, "http://127.0.0.1:8560/");
        assert_eq!(marketplace.wei_per_call, Wei::new(1_000_000_000_000_000));
        assert_eq!(marketplace.tunnel_server, "203.0.113.10:7000");
        assert_eq!(marketplace.max_providers, 100);
        assert!(marketplace.remote_ports().eq(20000..=20099));
        assert_eq!(marketplace.discovery_interval, Duration::from_secs(300));
        assert_eq!(marketplace.accept_window, Duration::from_secs(7200));
        assert_eq!(marketplace.refresh_interval, Duration::from_secs(3600));
        assert_eq!(marketplace.agreement_life, Duration::from_secs(259_200));
        assert_eq!(marketplace.offer_max_lifetime, Duration::from_secs(172_800));
        assert_eq!(marketplace.offer_max_lag_blocks, 1000);
        assert_eq!(marketplace.gas_warn_below, Wei::new(20_000_000_000_000_000));
    }

    #[test]
    fn a_marketplace_section_without_the_price_or_the_tunnel_is_refused() {
        for missing in ["wei_per_call", "tunnel_server"] {
            let text: String = MARKETPLACE
                .lines()
                .filter(|line| !line.starts_with(missing))
                .map(|line| format!("{line}\n"))
                .collect();
            let rendered = rendered(&parse(&text).expect_err("must refuse"));
            assert!(rendered.contains(missing), "{rendered}");
        }
    }

    #[test]
    fn the_marketplace_section_parses() {
        let config = parse(&format!(
            "{MARKETPLACE}writer_url = \"http://10.0.0.2:8560\"\n\
             max_providers = 10\nremote_port_start = 30000\n\
             discovery_interval = \"1m\"\naccept_window = \"30m\"\nagreement_life = \"1d\"\n\
             gas_warn_below = \"1.5\"\n"
        ))
        .expect("valid");
        let marketplace = config.marketplace.expect("the section is present");
        assert_eq!(marketplace.writer_url, "http://10.0.0.2:8560");
        assert_eq!(marketplace.max_providers, 10);
        assert!(marketplace.remote_ports().eq(30000..=30009));
        assert_eq!(marketplace.discovery_interval, Duration::from_secs(60));
        assert_eq!(marketplace.accept_window, Duration::from_secs(1800));
        assert_eq!(marketplace.agreement_life, Duration::from_secs(86_400));
        assert_eq!(
            marketplace.gas_warn_below,
            Wei::new(1_500_000_000_000_000_000)
        );
    }

    #[test]
    fn glm_amounts_are_decimal_with_eighteen_places() {
        assert_eq!(glm("0.02").unwrap(), Wei::new(20_000_000_000_000_000));
        assert_eq!(glm("1").unwrap(), Wei::new(1_000_000_000_000_000_000));
        assert_eq!(glm("0.000000000000000001").unwrap(), Wei::new(1));
        assert_eq!(glm("0").unwrap(), Wei::new(0));
        for bad in [
            "0.0000000000000000001",
            ".5",
            "1e3",
            "-1",
            "0x10",
            "one",
            "",
        ] {
            assert!(glm(bad).is_err(), "{bad:?} must be refused");
        }
        let error =
            parse(&format!("{MARKETPLACE}gas_warn_below = \"1e3\"\n")).expect_err("must refuse");
        let rendered = rendered(&error);
        assert!(rendered.contains("gas_warn_below"), "{rendered}");
        assert!(rendered.contains("GLM amount"), "{rendered}");
    }

    #[test]
    fn marketplace_zero_durations_are_refused() {
        for name in [
            "discovery_interval",
            "accept_window",
            "refresh_interval",
            "agreement_life",
            "offer_max_lifetime",
        ] {
            let error = parse(&format!("{MARKETPLACE}{name} = \"0s\"\n")).expect_err("must refuse");
            assert!(error.to_string().contains(name), "{error}");
        }
    }

    #[test]
    fn lifetimes_must_be_even_seconds() {
        for name in ["accept_window", "agreement_life", "offer_max_lifetime"] {
            for odd in ["3s", "1m 1s", "2500ms"] {
                let error =
                    parse(&format!("{MARKETPLACE}{name} = \"{odd}\"\n")).expect_err("must refuse");
                assert!(error.to_string().contains(name), "{odd}: {error}");
                assert!(error.to_string().contains("even"), "{odd}: {error}");
            }
            parse(&format!("{MARKETPLACE}{name} = \"4s\"\n")).expect("even seconds are fine");
        }
        parse(&format!("{MARKETPLACE}discovery_interval = \"3s\"\n"))
            .expect("an interval is not a lifetime");
    }

    #[test]
    fn the_cap_stays_below_the_page_limit() {
        let error = parse(&format!("{MARKETPLACE}max_providers = 200\n")).expect_err("must refuse");
        assert!(error.to_string().contains("max_providers"), "{error}");
        assert!(error.to_string().contains("one page"), "{error}");
        parse(&format!("{MARKETPLACE}max_providers = 199\n"))
            .expect("one below the page limit is fine");
        let config = parse(&format!("{MARKETPLACE}max_providers = 0\n"))
            .expect("a cap of zero accepts nobody");
        let marketplace = config.marketplace.expect("the section is present");
        assert_eq!(marketplace.remote_ports().count(), 0);
    }

    #[test]
    fn the_ports_stay_below_65536() {
        let error = parse(&format!(
            "{MARKETPLACE}max_providers = 3\nremote_port_start = 65534\n"
        ))
        .expect_err("must refuse");
        assert!(error.to_string().contains("remote_port_start"), "{error}");
        assert!(error.to_string().contains("65535"), "{error}");
        let config = parse(&format!(
            "{MARKETPLACE}max_providers = 3\nremote_port_start = 65533\n"
        ))
        .expect("ending on the last port is fine");
        let marketplace = config.marketplace.expect("the section is present");
        assert!(marketplace.remote_ports().eq(65533..=65535));
    }

    #[test]
    fn the_writer_url_must_be_http() {
        let error = parse(&format!(
            "{MARKETPLACE}writer_url = \"ws://127.0.0.1:8560\"\n"
        ))
        .expect_err("must refuse");
        assert!(error.to_string().contains("writer_url"), "{error}");
        let error =
            parse(&format!("{MARKETPLACE}writer_url = \"http://\"\n")).expect_err("must refuse");
        assert!(error.to_string().contains("does not parse"), "{error}");
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
    fn the_reference_key_is_not_reachable_from_toml() {
        parse("reference_key = \"secret\"\n").expect_err("the key comes from the environment only");
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
        let rendered = rendered(&error);
        assert!(rendered.contains("probe_intreval"), "{rendered}");
    }

    #[test]
    fn the_committed_example_parses_and_validates() {
        parse(include_str!("../../../config.example.toml")).expect("example must stay valid");
    }
}
