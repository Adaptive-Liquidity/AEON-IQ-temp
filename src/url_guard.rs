//! SSRF / outbound-egress guard for provider base URLs.
//!
//! `validate_provider_url` is called at startup for every configurable
//! provider URL (`UPSTREAM_BASE_URL`, `EMBEDDING_BASE_URL`,
//! `EXTRACTOR_BASE_URL`).  A hostile or misconfigured URL that resolves to a
//! private/link-local/metadata address is rejected before any request is made,
//! so no client-supplied or server-side API key can be leaked to an internal
//! endpoint.
//!
//! ## Dev flag: `AEON_ALLOW_INSECURE_PROVIDER_URLS=true`
//!
//! When this env var is set to `"true"` (never do this in production), the
//! guard relaxes its rules to support test harnesses and local development:
//!
//! **Permitted with the flag:**
//! - `http` scheme (in addition to `https`)
//! - Loopback addresses: 127.0.0.0/8, ::1
//! - Private RFC-1918 ranges: 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16
//! - IPv6 unique-local: fc00::/7
//!
//! **Still blocked even with the flag:**
//! - Link-local: 169.254.0.0/16 (covers the 169.254.169.254 cloud metadata IP)
//!   and fe80::/10 — pointing a provider at the instance-metadata service is
//!   never legitimate and is treated as a SSRF tripwire.
//! - Multicast and unspecified addresses.
//! - IPv4-mapped IPv6 addresses wrapping any of the above (e.g.
//!   `::ffff:169.254.169.254` is still rejected).
//!
//! Rationale: test mocks live on loopback or private Docker networks; the flag
//! is opt-in and never set in production images.  But cloud metadata endpoints
//! are reachable from both prod and dev, so they stay blocked unconditionally.
//!
//! ## Residual DNS-rebinding / TOCTOU risk
//!
//! This guard validates URLs **at config-load time** (process startup).
//! Because the HTTP client re-resolves DNS for every outbound request, a
//! hostname that passes the startup check could later be made to resolve to an
//! internal address by an attacker who controls DNS (DNS rebinding / TOCTOU).
//! To eliminate this residual risk in production:
//! - Prefer **IP-literal** provider URLs (e.g. `https://1.2.3.4/v1`) so DNS
//!   is never involved at request time.
//! - Enforce egress controls at the **infrastructure layer** (firewall /
//!   security-group rules, VPC egress policies) to block outbound traffic to
//!   RFC-1918 / link-local ranges regardless of what the application does.

use std::net::{IpAddr, ToSocketAddrs};

use anyhow::bail;
use reqwest::Url;

/// Returns `true` when the process env flag `AEON_ALLOW_INSECURE_PROVIDER_URLS`
/// is set to `"true"` (case-insensitive).
fn insecure_urls_allowed() -> bool {
    std::env::var("AEON_ALLOW_INSECURE_PROVIDER_URLS")
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Classify an IP address and return `Some(reason)` if it must be blocked.
///
/// This function classifies addresses **unconditionally** — it does not
/// consult the dev flag.  Callers are responsible for skipping the check for
/// ranges that the dev flag permits (loopback and private).
///
/// ### IPv4 ranges blocked (always)
/// - Loopback      127.0.0.0/8    (std: `is_loopback`)
/// - Link-local    169.254.0.0/16 (std: `is_link_local`) — covers cloud metadata 169.254.169.254
/// - Private       10/8, 172.16/12, 192.168/16 (std: `is_private`)
/// - Unspecified   0.0.0.0        (std: `is_unspecified`)
/// - Multicast     224.0.0.0/4    (std: `is_multicast`)
///
/// ### IPv6 ranges blocked (always)
/// - Loopback      ::1             (std: `is_loopback`)
/// - Unspecified   ::              (std: `is_unspecified`)
/// - Multicast     ff00::/8        (std: `is_multicast`)
/// - Link-local    fe80::/10       (leading 10 bits = 1111 1110 10; manual check)
/// - Unique-local  fc00::/7        (leading 7 bits  = 1111 110;  manual check)
fn blocked_reason(addr: IpAddr) -> Option<&'static str> {
    match addr {
        IpAddr::V4(v4) => {
            if v4.is_loopback() {
                Some("loopback address")
            } else if v4.is_link_local() {
                // Covers 169.254.169.254 (AWS/GCP/Azure instance metadata).
                Some("link-local / instance-metadata address")
            } else if v4.is_private() {
                Some("private RFC-1918 address")
            } else if v4.is_unspecified() {
                Some("unspecified address (0.0.0.0)")
            } else if v4.is_multicast() {
                Some("multicast address")
            } else {
                None
            }
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback() {
                Some("loopback address (::1)")
            } else if v6.is_unspecified() {
                Some("unspecified address (::)")
            } else if v6.is_multicast() {
                Some("multicast address")
            } else {
                // Link-local: fe80::/10  — first 10 bits are 1111 1110 10
                // In the first two octets: 0xfe, and the second octet's top 2 bits are 0b10
                // i.e., octets[0] == 0xfe && octets[1] & 0xc0 == 0x80
                let octets = v6.octets();
                if octets[0] == 0xfe && (octets[1] & 0xc0) == 0x80 {
                    return Some("IPv6 link-local address (fe80::/10)");
                }
                // Unique-local: fc00::/7 — first 7 bits are 1111 110
                // octets[0] & 0xfe == 0xfc
                if octets[0] & 0xfe == 0xfc {
                    return Some("IPv6 unique-local address (fc00::/7)");
                }
                // IPv4-mapped IPv6 (::ffff:0:0/96) — e.g. ::ffff:127.0.0.1 or
                // ::ffff:169.254.169.254.  These are syntactically V6 but
                // route to the embedded IPv4 address, so we must re-classify
                // them against the V4 ruleset to close the bypass.
                if let Some(v4) = v6.to_ipv4_mapped() {
                    return blocked_reason(IpAddr::V4(v4));
                }
                None
            }
        }
    }
}

/// Returns `true` for addresses that the dev flag is permitted to allow.
///
/// These are ranges that are safe for local development and Docker test
/// harnesses but that must never appear in a production provider URL:
/// - Loopback (127/8, ::1)
/// - Private RFC-1918 (10/8, 172.16/12, 192.168/16)
/// - IPv6 unique-local (fc00::/7)
///
/// Link-local, multicast, unspecified, and IPv4-mapped variants of the above
/// are NOT included here — they are handled by `blocked_reason` regardless of
/// the flag.
fn is_dev_allowed(addr: &IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private(),
        IpAddr::V6(v6) => {
            if v6.is_loopback() {
                return true;
            }
            // IPv4-mapped addresses: apply the V4 dev-allowed check so that
            // e.g. ::ffff:127.0.0.1 is treated the same as 127.0.0.1.
            // (::ffff:169.254.x.x will return false here because link-local
            // is not in the dev-allowed set, and blocked_reason will catch it.)
            if let Some(v4) = v6.to_ipv4_mapped() {
                return v4.is_loopback() || v4.is_private();
            }
            // Unique-local fc00::/7: octets[0] & 0xfe == 0xfc
            let octets = v6.octets();
            octets[0] & 0xfe == 0xfc
        }
    }
}

/// Parse and validate a provider base URL.
///
/// Validation rules (applied in order):
/// 1. **Parseable** — must be a valid URL.
/// 2. **Scheme** — must be `https`.  When `AEON_ALLOW_INSECURE_PROVIDER_URLS=true`
///    (dev-only flag), `http` is also permitted.  All other schemes are rejected.
/// 3. **Host present** — URL must have a resolvable hostname or IP literal.
/// 4. **IP classification** — the host is resolved to IP address(es) via
///    `std::net::ToSocketAddrs`.  If *any* resolved address falls into a blocked
///    range the URL is rejected.
///    - When `AEON_ALLOW_INSECURE_PROVIDER_URLS=true`, loopback (127/8, ::1) and
///      private RFC-1918 ranges (10/8, 172.16/12, 192.168/16) and IPv6
///      unique-local (fc00::/7) are permitted for local development and Docker
///      test harnesses.
///    - Link-local (169.254.0.0/16 including 169.254.169.254, fe80::/10),
///      multicast, and unspecified addresses are **always blocked** even with
///      the dev flag — pointing a provider at the cloud metadata service is
///      never legitimate.
///
/// On success the function returns the normalized URL string (as produced by
/// `reqwest::Url::to_string()`).  On failure it returns an `anyhow::Error`
/// whose message names the offending env-var and the rejection reason.
pub fn validate_provider_url(var_name: &str, raw: &str) -> anyhow::Result<String> {
    let url = Url::parse(raw)
        .map_err(|e| anyhow::anyhow!("{var_name}: failed to parse URL {raw:?}: {e}"))?;

    let scheme = url.scheme();
    let allow_insecure = insecure_urls_allowed();

    match scheme {
        "https" => {} // always OK
        "http" if allow_insecure => {
            tracing::warn!(
                "{var_name}: using insecure http scheme — only permitted because \
                 AEON_ALLOW_INSECURE_PROVIDER_URLS=true (never enable in production)"
            );
        }
        "http" => {
            bail!(
                "{var_name}: URL {raw:?} uses http scheme. \
                 Only https is permitted. To allow http for local development, \
                 set AEON_ALLOW_INSECURE_PROVIDER_URLS=true."
            );
        }
        other => {
            bail!(
                "{var_name}: URL {raw:?} uses unsupported scheme {other:?}. \
                 Only https (or http with AEON_ALLOW_INSECURE_PROVIDER_URLS=true) is allowed."
            );
        }
    }

    let host = url
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("{var_name}: URL {raw:?} has no host"))?;

    // Determine the port to use for socket-addr resolution.
    // If the URL has an explicit port, use it; otherwise fall back to the
    // scheme default (443 for https, 80 for http).
    let port = url.port().unwrap_or(match scheme {
        "https" => 443,
        _ => 80,
    });

    // Resolve the host.  `ToSocketAddrs` performs DNS when given a hostname
    // string, or parses the IP literal directly.
    let socket_addr_str = format!("{host}:{port}");
    let addrs: Vec<IpAddr> = socket_addr_str
        .to_socket_addrs()
        .map_err(|e| {
            anyhow::anyhow!("{var_name}: could not resolve host {host:?} in URL {raw:?}: {e}")
        })?
        .map(|sa| sa.ip())
        .collect();

    if addrs.is_empty() {
        bail!("{var_name}: host {host:?} in URL {raw:?} resolved to zero addresses");
    }

    for addr in &addrs {
        // When the dev flag is on, allow loopback and private ranges so that
        // `http://localhost:8080` or `http://mock-service:11435` (which resolves
        // to a private Docker IP) works in local development and test harnesses.
        // Link-local (including 169.254.169.254), multicast, and unspecified are
        // still blocked unconditionally — those are never legitimate provider
        // targets even in development.
        if allow_insecure && is_dev_allowed(addr) {
            tracing::warn!(
                "{var_name}: address {addr} permitted because \
                 AEON_ALLOW_INSECURE_PROVIDER_URLS=true (never enable in production)"
            );
            continue;
        }

        if let Some(reason) = blocked_reason(*addr) {
            bail!(
                "{var_name}: URL {raw:?} resolved to {addr} which is a {reason} — \
                 blocked to prevent SSRF / metadata endpoint access. \
                 Only public HTTPS provider endpoints are allowed."
            );
        }
    }

    Ok(url.to_string())
}

/// Validation for the **local embedding lane** (`LOCAL_EMBEDDING_BASE_URL`).
///
/// Unlike `validate_provider_url` (a deny-list — public HTTPS endpoints are
/// the expected target, only dangerous ranges are blocked), this is an
/// **allow-list**: every resolved address MUST be loopback or RFC-1918
/// private or IPv6 unique-local (`is_dev_allowed`), or the URL is rejected —
/// including ordinary public addresses. `http`, loopback, and private
/// addresses are permitted *for this variable only*, without the global
/// `AEON_ALLOW_INSECURE_PROVIDER_URLS` escape hatch (which would also loosen
/// the upstream and extractor guards). This lane exists precisely so
/// sensitivity-labeled content can be embedded by a model running on this
/// host or private network — a public address here would silently defeat
/// that guarantee, so it is never permitted, not even the ordinary public
/// addresses `validate_provider_url` allows.
pub fn validate_local_provider_url(var_name: &str, raw: &str) -> anyhow::Result<String> {
    let url = Url::parse(raw)
        .map_err(|e| anyhow::anyhow!("{var_name}: failed to parse URL {raw:?}: {e}"))?;

    let scheme = url.scheme();
    if !matches!(scheme, "http" | "https") {
        bail!(
            "{var_name}: URL {raw:?} uses unsupported scheme {scheme:?}.              Only http or https is allowed for the local embedding lane."
        );
    }

    let host = url
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("{var_name}: URL {raw:?} has no host"))?;
    let port = url.port().unwrap_or(match scheme {
        "https" => 443,
        _ => 80,
    });
    let socket_addr_str = format!("{host}:{port}");
    let addrs: Vec<IpAddr> = socket_addr_str
        .to_socket_addrs()
        .map_err(|e| {
            anyhow::anyhow!("{var_name}: could not resolve host {host:?} in URL {raw:?}: {e}")
        })?
        .map(|sa| sa.ip())
        .collect();

    if addrs.is_empty() {
        bail!("{var_name}: host {host:?} in URL {raw:?} resolved to zero addresses");
    }

    for addr in &addrs {
        // Loopback and private ranges are the expected — and ONLY permitted —
        // targets of this lane. Anything else (including an ordinary public
        // address that isn't in blocked_reason's always-blocked set) must
        // also be rejected: this function exists specifically so
        // private/secret content never leaves the host/private network, so a
        // public host here would silently defeat that guarantee — even from
        // an innocent misconfiguration, not just an attacker.
        if is_dev_allowed(addr) {
            continue;
        }
        if let Some(reason) = blocked_reason(*addr) {
            bail!(
                "{var_name}: URL {raw:?} resolved to {addr} which is a {reason} —                  blocked even for the local lane (metadata/link-local/multicast                  addresses are never legitimate embedding targets)."
            );
        }
        bail!(
            "{var_name}: URL {raw:?} resolved to {addr}, a public address —              the local embedding lane only accepts loopback or RFC-1918/ULA              private addresses, so that private/secret content never leaves              this host or private network. Point it at a locally-reachable              embedding service instead."
        );
    }

    Ok(url.to_string())
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Inner helpers that accept the flag as a parameter ──────────────────
    // This avoids touching the process-global env in parallel tests.

    fn validate_with_flag(
        var_name: &str,
        raw: &str,
        allow_insecure: bool,
    ) -> anyhow::Result<String> {
        let url = Url::parse(raw)
            .map_err(|e| anyhow::anyhow!("{var_name}: failed to parse URL {raw:?}: {e}"))?;

        let scheme = url.scheme();

        match scheme {
            "https" => {}
            "http" if allow_insecure => {}
            "http" => {
                bail!(
                    "{var_name}: URL {raw:?} uses http scheme. \
                     Only https is permitted without AEON_ALLOW_INSECURE_PROVIDER_URLS=true."
                );
            }
            other => {
                bail!("{var_name}: URL {raw:?} uses unsupported scheme {other:?}.");
            }
        }

        let host = url
            .host_str()
            .ok_or_else(|| anyhow::anyhow!("{var_name}: URL {raw:?} has no host"))?;

        let port = url.port().unwrap_or(match scheme {
            "https" => 443,
            _ => 80,
        });

        let socket_addr_str = format!("{host}:{port}");
        let addrs: Vec<IpAddr> = socket_addr_str
            .to_socket_addrs()
            .map_err(|e| anyhow::anyhow!("{var_name}: could not resolve host {host:?}: {e}"))?
            .map(|sa| sa.ip())
            .collect();

        if addrs.is_empty() {
            bail!("{var_name}: host {host:?} resolved to zero addresses");
        }

        for addr in &addrs {
            if allow_insecure && is_dev_allowed(addr) {
                continue; // dev: loopback and private allowed
            }
            if let Some(reason) = blocked_reason(*addr) {
                bail!("{var_name}: URL {raw:?} resolved to {addr} which is a {reason} — blocked.");
            }
        }

        Ok(url.to_string())
    }

    // ── Rejection cases (no dev flag) ──────────────────────────────────────

    #[test]
    fn rejects_ipv4_metadata_endpoint() {
        // 169.254.169.254 — cloud instance metadata (link-local)
        let err = validate_with_flag("TEST_VAR", "http://169.254.169.254", false).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("TEST_VAR"), "error should name the var: {msg}");
        assert!(
            msg.contains("169.254.169.254"),
            "error should name the address: {msg}"
        );
    }

    #[test]
    fn rejects_ipv4_loopback() {
        let err = validate_with_flag("TEST_VAR", "http://127.0.0.1", false).unwrap_err();
        assert!(err.to_string().contains("127.0.0.1"), "{}", err);
    }

    #[test]
    fn rejects_ipv4_private_10_block() {
        let err = validate_with_flag("TEST_VAR", "https://10.0.0.1", false).unwrap_err();
        assert!(err.to_string().contains("10.0.0.1"), "{}", err);
    }

    #[test]
    fn rejects_http_without_dev_flag() {
        // api.openai.com is a valid public host — but http is not allowed without dev flag.
        // We test this with an IP that would pass the IP check so we hit the scheme guard.
        let err = validate_with_flag("TEST_VAR", "http://api.openai.com", false).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("http scheme") || msg.contains("https"),
            "error should mention http/https: {msg}"
        );
    }

    // ── Rejection cases: blocked_reason unit tests ─────────────────────────

    #[test]
    fn local_lane_allows_loopback_http() {
        assert!(
            validate_local_provider_url("LOCAL_EMBEDDING_BASE_URL", "http://127.0.0.1:11434")
                .is_ok()
        );
        assert!(
            validate_local_provider_url("LOCAL_EMBEDDING_BASE_URL", "http://localhost:8081")
                .is_ok()
        );
    }

    #[test]
    fn local_lane_blocks_metadata_and_bad_schemes() {
        assert!(
            validate_local_provider_url("LOCAL_EMBEDDING_BASE_URL", "http://169.254.169.254")
                .is_err(),
            "cloud metadata must stay blocked even on the local lane"
        );
        assert!(
            validate_local_provider_url("LOCAL_EMBEDDING_BASE_URL", "ftp://127.0.0.1").is_err()
        );
    }

    /// Regression test: the local embedding lane must be an allow-list
    /// (loopback/private only), not a deny-list. A public address — even a
    /// perfectly ordinary one with no SSRF properties at all — must be
    /// rejected, because this lane's entire purpose is keeping
    /// private/secret content off the public internet. Prior to this fix,
    /// `validate_local_provider_url` only consulted `blocked_reason` (which
    /// is a deny-list scoped to metadata/link-local/multicast/unspecified)
    /// and silently accepted any other address, including public ones.
    #[test]
    fn local_lane_rejects_public_address() {
        let err =
            validate_local_provider_url("LOCAL_EMBEDDING_BASE_URL", "https://1.1.1.1").unwrap_err();
        assert!(
            err.to_string().contains("1.1.1.1"),
            "public address must be rejected: {err}"
        );
    }

    #[test]
    fn blocked_reason_identifies_ipv4_link_local() {
        let addr: IpAddr = "169.254.169.254".parse().unwrap();
        assert!(
            blocked_reason(addr).is_some(),
            "169.254.169.254 must be blocked"
        );
    }

    #[test]
    fn blocked_reason_identifies_ipv4_loopback() {
        let addr: IpAddr = "127.0.0.1".parse().unwrap();
        assert!(blocked_reason(addr).is_some());
    }

    #[test]
    fn blocked_reason_identifies_ipv4_private_ranges() {
        for ip in &["10.0.0.1", "172.16.0.1", "172.31.255.255", "192.168.1.1"] {
            let addr: IpAddr = ip.parse().unwrap();
            assert!(
                blocked_reason(addr).is_some(),
                "{ip} should be blocked as private"
            );
        }
    }

    #[test]
    fn blocked_reason_identifies_ipv4_unspecified() {
        let addr: IpAddr = "0.0.0.0".parse().unwrap();
        assert!(blocked_reason(addr).is_some());
    }

    #[test]
    fn blocked_reason_identifies_ipv4_multicast() {
        let addr: IpAddr = "224.0.0.1".parse().unwrap();
        assert!(blocked_reason(addr).is_some());
    }

    #[test]
    fn blocked_reason_identifies_ipv6_loopback() {
        let addr: IpAddr = "::1".parse().unwrap();
        assert!(blocked_reason(addr).is_some());
    }

    #[test]
    fn blocked_reason_identifies_ipv6_link_local() {
        // fe80::1 — link-local
        let addr: IpAddr = "fe80::1".parse().unwrap();
        assert!(blocked_reason(addr).is_some(), "fe80::1 must be blocked");
    }

    #[test]
    fn blocked_reason_identifies_ipv6_unique_local() {
        // fc00::1 and fd00::1 both in fc00::/7
        for ip in &["fc00::1", "fd00::1"] {
            let addr: IpAddr = ip.parse().unwrap();
            assert!(
                blocked_reason(addr).is_some(),
                "{ip} should be blocked as unique-local"
            );
        }
    }

    #[test]
    fn blocked_reason_allows_public_ipv4() {
        // 1.1.1.1 — Cloudflare public DNS
        let addr: IpAddr = "1.1.1.1".parse().unwrap();
        assert!(blocked_reason(addr).is_none(), "1.1.1.1 should be allowed");
    }

    // ── Acceptance case: valid public HTTPS URL (resolved via IP literal) ──
    // We use an IP-literal that is public so the test is hermetic (no DNS needed).
    // 1.1.1.1:443 is Cloudflare DNS / public — any public IP would do.

    #[test]
    fn accepts_https_public_ip_literal() {
        // Use a public IP literal to avoid DNS dependency.
        let result = validate_with_flag("TEST_VAR", "https://1.1.1.1", false);
        assert!(
            result.is_ok(),
            "public https IP should be accepted: {:?}",
            result.err()
        );
    }

    // ── Dev-flag path ──────────────────────────────────────────────────────

    #[test]
    fn dev_flag_allows_localhost() {
        // With dev flag: http://localhost:8080 should be accepted.
        let result = validate_with_flag("TEST_VAR", "http://localhost:8080", true);
        assert!(
            result.is_ok(),
            "localhost should be allowed with dev flag: {:?}",
            result.err()
        );
    }

    #[test]
    fn dev_flag_still_rejects_metadata_ip() {
        // Even with dev flag, 169.254.169.254 must be blocked.
        let err = validate_with_flag("TEST_VAR", "http://169.254.169.254", true).unwrap_err();
        assert!(
            err.to_string().contains("169.254.169.254"),
            "metadata IP should still be blocked with dev flag: {}",
            err
        );
    }

    #[test]
    fn dev_flag_allows_private_range() {
        // With dev flag, private RFC-1918 ranges must be allowed so that Docker
        // service names resolving to private IPs work in test harnesses.
        let result = validate_with_flag("TEST_VAR", "http://192.168.1.1", true);
        assert!(
            result.is_ok(),
            "private IP 192.168.1.1 should be allowed with dev flag: {:?}",
            result.err()
        );
        let result = validate_with_flag("TEST_VAR", "http://10.0.0.1", true);
        assert!(
            result.is_ok(),
            "private IP 10.0.0.1 should be allowed with dev flag: {:?}",
            result.err()
        );
        let result = validate_with_flag("TEST_VAR", "http://127.0.0.1", true);
        assert!(
            result.is_ok(),
            "loopback 127.0.0.1 should be allowed with dev flag: {:?}",
            result.err()
        );
    }

    // ── IPv4-mapped IPv6 bypass regression tests (FIX 1) ──────────────────
    // ::ffff:<v4> addresses are syntactically IPv6 but route to the embedded
    // IPv4.  Without the to_ipv4_mapped() re-classification they would fall
    // through the V6 checks and be permitted — a critical SSRF bypass.

    #[test]
    fn blocked_reason_rejects_ipv4_mapped_loopback() {
        // ::ffff:127.0.0.1  (loopback embedded in IPv6)
        let addr: IpAddr = "::ffff:127.0.0.1".parse().unwrap();
        assert!(
            blocked_reason(addr).is_some(),
            "::ffff:127.0.0.1 must be blocked (IPv4-mapped loopback)"
        );
    }

    #[test]
    fn blocked_reason_rejects_ipv4_mapped_link_local() {
        // ::ffff:169.254.169.254  (cloud instance-metadata embedded in IPv6)
        let addr: IpAddr = "::ffff:169.254.169.254".parse().unwrap();
        assert!(
            blocked_reason(addr).is_some(),
            "::ffff:169.254.169.254 must be blocked (IPv4-mapped link-local / metadata)"
        );
    }

    #[test]
    fn blocked_reason_rejects_ipv4_mapped_private() {
        // ::ffff:10.0.0.1  (RFC-1918 embedded in IPv6)
        let addr: IpAddr = "::ffff:10.0.0.1".parse().unwrap();
        assert!(
            blocked_reason(addr).is_some(),
            "::ffff:10.0.0.1 must be blocked (IPv4-mapped private range)"
        );
    }

    #[test]
    fn rejects_ipv4_mapped_loopback_url() {
        // Full URL round-trip: https://[::ffff:127.0.0.1] must be rejected.
        let err = validate_with_flag("TEST_VAR", "https://[::ffff:127.0.0.1]", false).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("loopback") || msg.contains("::ffff"),
            "URL with IPv4-mapped loopback must be blocked: {msg}"
        );
    }

    #[test]
    fn rejects_ipv4_mapped_metadata_url() {
        // Full URL round-trip: https://[::ffff:169.254.169.254] must be rejected.
        let err =
            validate_with_flag("TEST_VAR", "https://[::ffff:169.254.169.254]", false).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("link-local") || msg.contains("metadata") || msg.contains("169.254"),
            "URL with IPv4-mapped metadata IP must be blocked: {msg}"
        );
    }

    #[test]
    fn rejects_ipv4_mapped_private_url() {
        // Full URL round-trip: https://[::ffff:10.0.0.1] must be rejected.
        let err = validate_with_flag("TEST_VAR", "https://[::ffff:10.0.0.1]", false).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("private") || msg.contains("10.0.0.1"),
            "URL with IPv4-mapped private IP must be blocked: {msg}"
        );
    }
}
