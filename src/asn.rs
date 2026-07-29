//! Resolves a client IP to its announcing ASN + org name, for the
//! `client_asn_requests_total` metric. Two data sources, cheapest first:
//!
//! 1. The local Prometheus instance — if this subnet was already resolved
//!    by a *previous* process lifetime, its (subnet, asn, asn_org) labels
//!    are still sitting in Prometheus's TSDB even though our in-process
//!    cache reset on restart. Same box, sub-millisecond.
//! 2. [Team Cymru's DNS-based ASN lookup](https://team-cymru.com/community-services/ip-asn-mapping/),
//!    a free, keyless service: one TXT query to resolve IP -> ASN, a
//!    second to resolve ASN -> org name.
//!
//! Both are best-effort from the caller's perspective — see
//! `state.rs::AppState::lookup_asn_cached`, which this module doesn't know
//! about, for how failures become "unknown" instead of ever holding up a
//! response.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

use hickory_resolver::TokioResolver;
use hickory_resolver::proto::rr::RData;

pub const UNKNOWN: &str = "unknown";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AsnInfo {
    pub asn: u32,
    pub org: String,
}

impl AsnInfo {
    pub fn asn_label(&self) -> String {
        self.asn.to_string()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AsnLookupError {
    #[error("origin lookup {query:?} failed: {source}")]
    OriginQuery {
        query: String,
        #[source]
        source: hickory_resolver::net::NetError,
    },
    #[error("origin lookup {query:?} returned no TXT records")]
    OriginEmpty { query: String },
    #[error(
        "origin lookup {query:?} returned TXT {raw:?}, which didn't parse as 'asn | prefix | cc | registry | date'"
    )]
    OriginParse { query: String, raw: String },
    #[error("name lookup {query:?} failed: {source}")]
    NameQuery {
        query: String,
        #[source]
        source: hickory_resolver::net::NetError,
    },
    #[error("name lookup {query:?} returned no TXT records")]
    NameEmpty { query: String },
    #[error(
        "name lookup {query:?} returned TXT {raw:?}, which didn't parse as 'asn | cc | registry | date | name'"
    )]
    NameParse { query: String, raw: String },
}

/// RFC 1918 / RFC 4193 / loopback / link-local addresses never have a
/// public ASN — skip both the Prometheus check and the Cymru DNS round
/// trips entirely rather than logging a "failure" for what's actually
/// expected (this server's own internal traffic: Prometheus scraping
/// `/metrics`, healthchecks, etc.).
pub fn is_publicly_routable(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation())
        }
        IpAddr::V6(v6) => {
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_unique_local()
                || v6.is_unicast_link_local())
        }
    }
}

/// Reverse-octet query name Cymru's IPv4 ASN-origin service expects, e.g.
/// `203.0.113.42` -> `42.113.0.203.origin.asn.cymru.com`.
fn origin4_query_name(v4: Ipv4Addr) -> String {
    let o = v4.octets();
    format!("{}.{}.{}.{}.origin.asn.cymru.com", o[3], o[2], o[1], o[0])
}

/// Reverse-nibble query name Cymru's IPv6 ASN-origin service expects (same
/// scheme as `ip6.arpa` reverse DNS, different suffix).
fn origin6_query_name(v6: Ipv6Addr) -> String {
    let nibbles: String = v6
        .octets()
        .iter()
        .flat_map(|byte| [byte >> 4, byte & 0x0f])
        .rev()
        .map(|nibble| format!("{nibble:x}"))
        .collect::<Vec<_>>()
        .join(".");
    format!("{nibbles}.origin6.asn.cymru.com")
}

fn origin_query_name(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => origin4_query_name(v4),
        IpAddr::V6(v6) => origin6_query_name(v6),
    }
}

/// Cymru's origin TXT looks like `"15169 | 8.8.8.0/24 | US | arin |
/// 1992-12-01"`, or `"701 703 | 208.66.44.0/24 | ..."` for a
/// multi-origin prefix — take the first ASN of the first field.
fn parse_origin_txt(raw: &str) -> Option<u32> {
    raw.split('|')
        .next()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// Cymru's name TXT looks like `"15169 | US | arin | 1992-12-01 | GOOGLE,
/// US"` — the org name is the last field.
fn parse_name_txt(raw: &str) -> Option<String> {
    let name = raw.split('|').next_back()?.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

async fn txt_lookup(
    resolver: &TokioResolver,
    query: &str,
) -> Result<String, hickory_resolver::net::NetError> {
    let lookup = resolver.txt_lookup(query).await?;
    Ok(lookup
        .answers()
        .iter()
        .find_map(|record| match &record.data {
            RData::TXT(txt) => Some(txt.to_string()),
            _ => None,
        })
        .unwrap_or_default())
}

/// Resolves `ip` to its announcing ASN and org name via two sequential
/// Cymru DNS TXT lookups (the second depends on the first's result, so
/// they can't run concurrently) — never called on the request path itself,
/// only from a background task (see `state.rs`).
pub async fn lookup(resolver: &TokioResolver, ip: IpAddr) -> Result<AsnInfo, AsnLookupError> {
    let origin_query = origin_query_name(ip);
    let origin_txt = txt_lookup(resolver, &origin_query)
        .await
        .map_err(|source| AsnLookupError::OriginQuery {
            query: origin_query.clone(),
            source,
        })?;
    if origin_txt.is_empty() {
        return Err(AsnLookupError::OriginEmpty {
            query: origin_query,
        });
    }
    let asn = parse_origin_txt(&origin_txt).ok_or_else(|| AsnLookupError::OriginParse {
        query: origin_query.clone(),
        raw: origin_txt.clone(),
    })?;

    let name_query = format!("AS{asn}.asn.cymru.com");
    let name_txt =
        txt_lookup(resolver, &name_query)
            .await
            .map_err(|source| AsnLookupError::NameQuery {
                query: name_query.clone(),
                source,
            })?;
    if name_txt.is_empty() {
        return Err(AsnLookupError::NameEmpty { query: name_query });
    }
    let org = parse_name_txt(&name_txt).ok_or_else(|| AsnLookupError::NameParse {
        query: name_query.clone(),
        raw: name_txt.clone(),
    })?;

    Ok(AsnInfo { asn, org })
}

const PROMETHEUS_QUERY_TIMEOUT: Duration = Duration::from_secs(2);

/// Best-effort check of the local Prometheus for a `subnet` this process
/// (or a prior instance of it) has already resolved and reported — Cymru
/// DNS is the source of truth, this is purely an optimization to skip it
/// when the answer is already sitting on the same box. Any failure (network,
/// parse, no match, or a stale `asn="unknown"` sample) just means "ask Cymru
/// instead", never propagated as an error.
pub async fn lookup_from_prometheus(
    http: &reqwest::Client,
    prometheus_url: &str,
    subnet: &str,
) -> Option<AsnInfo> {
    // subnet is always a `client_subnet()` output (digits/hex/./:  /), never
    // user-controlled text, so interpolating it into the PromQL selector
    // can't break out of the string literal.
    let query = format!(
        r#"mcp_info_server_client_asn_requests_total{{subnet="{subnet}", asn!="{UNKNOWN}"}}"#
    );
    // reqwest's `.query()` convenience needs its own cargo feature; this
    // project doesn't otherwise need query-string building, so just append
    // it via `Url` directly (always available, no extra feature).
    let mut url = reqwest::Url::parse(&format!("{prometheus_url}/api/v1/query")).ok()?;
    url.query_pairs_mut().append_pair("query", &query);
    let resp = http
        .get(url)
        .timeout(PROMETHEUS_QUERY_TIMEOUT)
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .json::<serde_json::Value>()
        .await
        .ok()?;

    let metric = resp
        .get("data")?
        .get("result")?
        .as_array()?
        .first()?
        .get("metric")?;
    let asn: u32 = metric.get("asn")?.as_str()?.parse().ok()?;
    let org = metric.get("asn_org")?.as_str()?.to_owned();
    Some(AsnInfo { asn, org })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin4_query_name_reverses_octets() {
        assert_eq!(
            origin4_query_name(Ipv4Addr::new(203, 0, 113, 42)),
            "42.113.0.203.origin.asn.cymru.com"
        );
    }

    #[test]
    fn origin6_query_name_reverses_all_nibbles() {
        assert_eq!(
            origin6_query_name("2001:db8::1".parse().unwrap()),
            "1.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.8.b.d.0.1.0.0.2.origin6.asn.cymru.com"
        );
    }

    #[test]
    fn parse_origin_txt_takes_first_asn_of_multi_origin_prefix() {
        assert_eq!(
            parse_origin_txt("701 703 | 208.66.44.0/24 | US | arin | 1998-09-25"),
            Some(701)
        );
    }

    #[test]
    fn parse_origin_txt_rejects_garbage() {
        assert_eq!(parse_origin_txt("not a cymru response"), None);
    }

    #[test]
    fn parse_name_txt_takes_last_field() {
        assert_eq!(
            parse_name_txt("15169 | US | arin | 1992-12-01 | GOOGLE, US"),
            Some("GOOGLE, US".to_owned())
        );
    }

    #[test]
    fn parse_name_txt_rejects_empty_name_field() {
        assert_eq!(parse_name_txt("15169 | US | arin | 1992-12-01 | "), None);
    }

    #[test]
    fn is_publicly_routable_rejects_private_and_loopback() {
        assert!(!is_publicly_routable("10.251.254.20".parse().unwrap()));
        assert!(!is_publicly_routable("127.0.0.1".parse().unwrap()));
        assert!(!is_publicly_routable("::1".parse().unwrap()));
        assert!(!is_publicly_routable("fd00:251::20".parse().unwrap()));
        assert!(!is_publicly_routable("fe80::1".parse().unwrap()));
    }

    #[test]
    fn is_publicly_routable_accepts_public_addresses() {
        // 203.0.113.0/24 is TEST-NET-3 (RFC 5737 documentation space) - not
        // actually routable, hence 8.8.8.8 here instead.
        assert!(is_publicly_routable("8.8.8.8".parse().unwrap()));
        assert!(is_publicly_routable(
            "2600:1702:7310:20e0::69".parse().unwrap()
        ));
    }

    #[tokio::test]
    async fn lookup_from_prometheus_returns_none_on_http_error() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/api/v1/query")
            .match_query(mockito::Matcher::Any)
            .with_status(500)
            .create_async()
            .await;
        let http = reqwest::Client::new();

        assert_eq!(
            lookup_from_prometheus(&http, &server.url(), "203.0.113.0/24").await,
            None
        );
    }

    #[tokio::test]
    async fn lookup_from_prometheus_returns_none_on_empty_result() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/api/v1/query")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(r#"{"status":"success","data":{"resultType":"vector","result":[]}}"#)
            .create_async()
            .await;
        let http = reqwest::Client::new();

        assert_eq!(
            lookup_from_prometheus(&http, &server.url(), "203.0.113.0/24").await,
            None
        );
    }

    #[tokio::test]
    async fn lookup_from_prometheus_parses_a_matching_series() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/api/v1/query")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_body(
                r#"{"status":"success","data":{"resultType":"vector","result":[
                    {"metric":{"subnet":"203.0.113.0/24","asn":"15169","asn_org":"GOOGLE, US"},"value":[1.0,"3"]}
                ]}}"#,
            )
            .create_async()
            .await;
        let http = reqwest::Client::new();

        let info = lookup_from_prometheus(&http, &server.url(), "203.0.113.0/24")
            .await
            .unwrap();
        assert_eq!(info.asn, 15169);
        assert_eq!(info.org, "GOOGLE, US");
    }
}
