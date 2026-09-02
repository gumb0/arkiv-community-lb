//! The method denylist: a case-insensitive substring search over the raw
//! request body, run before a provider is chosen. The body is never
//! parsed, on this path or any other, which is what keeps a forwarded
//! request byte-identical and a retry replayable.
//!
//! Two limitations come with the substring approach and are accepted:
//! a denied name appearing inside `params` rejects the request, and a
//! client that escapes the name (`admin_`) evades the check. No
//! client library escapes method names, and the names below are the ones
//! whose evasion buys nothing a node would honour.
//!
//! The list is code, not configuration: retries are only replay-safe
//! because every non-replay-safe method is here, so an operator must not
//! be able to void that from a toml file.

/// Method names the endpoint refuses.
const DENIED: &[&str] = &[
    // Control surfaces: they do things to a node.
    "admin_",
    "engine_",
    "miner_",
    // debug_'s own control names; its read side stays served.
    "debug_set",
    "debug_verbosity",
    "debug_vmodule",
    "debug_freezeclient",
    // Keys: community nodes hold none.
    "eth_sendtransaction",
    "eth_sign",
    "eth_accounts",
    "personal_",
    // State bound to one node, which does not survive a load balancer.
    "eth_newfilter",
    "eth_newblockfilter",
    "eth_newpendingtransactionfilter",
    "eth_getfilterchanges",
    "eth_getfilterlogs",
    "eth_uninstallfilter",
    "eth_subscribe",
    "eth_unsubscribe",
];

/// The denied name the body contains, if any. A batch is rejected whole
/// when any entry names one.
pub fn denied(body: &[u8]) -> Option<&'static str> {
    DENIED
        .iter()
        .copied()
        .find(|name| contains_ignore_case(body, name.as_bytes()))
}

fn contains_ignore_case(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.len() > haystack.len() {
        return false;
    }
    haystack
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(method: &str) -> Vec<u8> {
        format!(r#"{{"jsonrpc":"2.0","id":1,"method":"{method}","params":[]}}"#).into_bytes()
    }

    #[test]
    fn each_denied_group_is_caught() {
        for method in [
            "admin_peers",
            "engine_forkchoiceUpdatedV3",
            "miner_start",
            "debug_setHead",
            "debug_setTrieFlushInterval",
            "debug_verbosity",
            "debug_vmodule",
            "debug_freezeClient",
            "eth_sendTransaction",
            "eth_sign",
            "eth_signTypedData_v4",
            "eth_accounts",
            "personal_unlockAccount",
            "eth_newFilter",
            "eth_getFilterChanges",
            "eth_uninstallFilter",
            "eth_subscribe",
            "eth_unsubscribe",
        ] {
            assert!(denied(&call(method)).is_some(), "{method} must be denied");
        }
    }

    #[test]
    fn served_methods_pass() {
        for method in [
            "eth_blockNumber",
            "eth_getBalance",
            "eth_getLogs",
            "eth_call",
            "eth_getProof",
            "arkiv_query",
            "net_version",
            "web3_clientVersion",
            // Relayed deliberately: the SDK's entity-write path.
            "eth_sendRawTransaction",
            // Read-only and deliberately not denied: absent from the
            // served API is a node's answer to give, not ours.
            "txpool_status",
            "debug_traceTransaction",
            "trace_block",
        ] {
            assert_eq!(denied(&call(method)), None, "{method} must be served");
        }
    }

    #[test]
    fn case_does_not_evade() {
        assert!(denied(&call("ADMIN_peers")).is_some());
        assert!(denied(&call("eth_SubScribe")).is_some());
    }

    #[test]
    fn a_batch_is_denied_when_one_entry_is() {
        let batch = br#"[{"id":1,"method":"eth_blockNumber"},{"id":2,"method":"admin_peers"}]"#;
        assert_eq!(denied(batch), Some("admin_"));
    }

    #[test]
    fn params_carrying_a_denied_name_are_rejected() {
        // The accepted false positive, pinned so it cannot change by
        // accident.
        let body = br#"{"id":1,"method":"arkiv_query","params":["owner = 'admin_bot'"]}"#;
        assert_eq!(denied(body), Some("admin_"));
    }
}
