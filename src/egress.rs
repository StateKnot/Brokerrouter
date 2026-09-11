use crate::{
    error::{Error, Result},
    store::Endpoint,
};
use reqwest::Client;
use std::{
    net::{IpAddr, SocketAddr},
    time::Duration,
};

fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            let o = v.octets();
            !v.is_private()
                && !v.is_loopback()
                && !v.is_link_local()
                && !v.is_broadcast()
                && !v.is_documentation()
                && !v.is_multicast()
                && !v.is_unspecified()
                && o[0] != 0
                && o[0] < 240
                && !(o[0] == 100 && (64..=127).contains(&o[1]))
                && !(o[0] == 198 && (o[1] == 18 || o[1] == 19))
                && !(o[0] == 192 && o[1] == 0 && o[2] == 0)
        }
        IpAddr::V6(v) => {
            // Restrict to ordinary global unicast, excluding transition/tunnel ranges.
            let s = v.segments();
            s[0] & 0xe000 == 0x2000
                && s[0] != 0x2002
                && !(s[0] == 0x2001 && (s[1] < 0x0200 || s[1] == 0x0db8))
                && !(s[0] == 0x3fff && (s[1] & 0xf000) == 0)
        }
    }
}

pub async fn client(e: &Endpoint, timeout: Duration) -> Result<(Client, url::Url)> {
    e.validate()?;
    let path = if e.operation == "chat" {
        "chat/completions"
    } else {
        "embeddings"
    };
    let url = url::Url::parse(&format!("{}/{path}", e.base_url.trim_end_matches('/')))
        .map_err(|_| Error::Configuration)?;
    let host = url.host_str().ok_or(Error::Configuration)?;
    let port = url.port_or_known_default().ok_or(Error::Configuration)?;
    let addrs: Vec<SocketAddr> = match url.host().ok_or(Error::Configuration)? {
        url::Host::Ipv4(ip) => vec![(ip, port).into()],
        url::Host::Ipv6(ip) => vec![(ip, port).into()],
        url::Host::Domain(name) => tokio::time::timeout(
            Duration::from_secs(3).min(timeout),
            tokio::net::lookup_host((name, port)),
        )
        .await
        .map_err(|_| Error::NotSubmitted)?
        .map_err(|_| Error::NotSubmitted)?
        .take(16)
        .collect(),
    };
    if addrs.is_empty() {
        return Err(Error::NotSubmitted);
    }
    if !e.allow_private && addrs.iter().any(|a| !public_ip(a.ip())) {
        return Err(Error::Invalid("endpoint resolves to a prohibited network"));
    }
    // Resolve once, validate, then pin these addresses. No redirects or implicit retries/proxies.
    let client = Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .timeout(timeout)
        .connect_timeout(Duration::from_secs(5).min(timeout))
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .no_zstd()
        .referer(false)
        .resolve_to_addrs(host, &addrs)
        .build()
        .map_err(|_| Error::Configuration)?;
    Ok((client, url))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn network_boundaries() {
        for ip in [
            "127.0.0.1",
            "10.0.0.1",
            "169.254.169.254",
            "100.64.0.1",
            "198.19.0.1",
            "224.0.0.1",
            "0.0.0.0",
            "::1",
            "::ffff:127.0.0.1",
            "fc00::1",
            "2002:7f00:1::",
            "2001:db8::1",
            "64:ff9b::7f00:1",
        ] {
            assert!(!public_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["1.1.1.1", "8.8.8.8", "2606:4700:4700::1111"] {
            assert!(public_ip(ip.parse().unwrap()), "{ip}");
        }
    }
}
