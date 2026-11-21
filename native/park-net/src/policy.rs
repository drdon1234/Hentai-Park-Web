use crate::Error;
use http::{header, HeaderMap, Method, StatusCode};
use std::net::IpAddr;
use url::Url;

pub fn public_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !matches!(a, 0 | 10 | 127 | 224..=255)
                && !matches!(
                    (a, b),
                    (100, 64..=127) | (169, 254) | (172, 16..=31) | (192, 0 | 168)
                )
                && !matches!(
                    (a, b, c),
                    (192, 2, _) | (192, 88, 99) | (198, 51, 100) | (203, 0, 113)
                )
        }
        IpAddr::V6(ip) => {
            if let Some(v4) = ip.to_ipv4_mapped() {
                return public_address(v4.into());
            }
            let b = ip.octets();
            if b[..12] == [0, 100, 255, 155, 0, 0, 0, 0, 0, 0, 0, 0]
                || b[..6] == [0, 100, 255, 155, 0, 1]
            {
                let octets: [u8; 4] = b[12..].try_into().unwrap();
                return public_address(std::net::Ipv4Addr::from(octets).into());
            }
            b[..6] == [32, 1, 0, 2, 0, 0]
                || (b[0] & 0xe0 == 0x20
                    && !(b[0] == 32 && b[1] == 1 && (b[2] < 2 || b[2..4] == [13, 184]))
                    && b[..2] != [32, 2])
        }
    }
}

pub(crate) fn authorize(url: &Url, domains: &[String]) -> Result<(), Error> {
    let host = url.host_str().ok_or(Error::Blocked)?.trim_end_matches('.');
    if !matches!(url.scheme(), "https" | "http")
        || !url.username().is_empty()
        || url.password().is_some()
        || !domains.iter().any(|domain| {
            let lower = domain.trim_end_matches('.').to_ascii_lowercase();
            if let Some(zone) = lower.strip_prefix("*.") {
                host.strip_suffix(zone)
                    .and_then(|v| v.strip_suffix('.'))
                    .is_some_and(|v| !v.is_empty() && !v.contains('.'))
            } else {
                host == lower || host.strip_suffix(&lower).is_some_and(|v| v.ends_with('.'))
            }
        })
    {
        return Err(Error::Blocked);
    }
    Ok(())
}

pub(crate) fn redirect(
    from: &Url,
    to: &Url,
    status: StatusCode,
    method: &mut Method,
    headers: &mut HeaderMap,
    body: &mut Option<bytes::Bytes>,
) -> Result<(), Error> {
    if from.scheme() == "https" && to.scheme() != "https" {
        return Err(Error::Blocked);
    }
    let status = status.as_u16();
    if status == 303 && *method != Method::HEAD
        || matches!(status, 301 | 302) && *method == Method::POST
    {
        *method = Method::GET;
        *body = None;
        let drop: Vec<_> = headers
            .keys()
            .filter(|h| h.as_str().starts_with("content-") || h.as_str() == "digest")
            .cloned()
            .collect();
        for h in drop {
            headers.remove(h);
        }
    }
    if from.origin() != to.origin() {
        if body.is_some() || !matches!(*method, Method::GET | Method::HEAD) {
            return Err(Error::Blocked);
        }
        let mut safe = HeaderMap::new();
        for name in [
            header::ACCEPT,
            header::USER_AGENT,
            header::ACCEPT_LANGUAGE,
            header::RANGE,
            header::CACHE_CONTROL,
            header::PRAGMA,
        ] {
            for v in headers.get_all(&name) {
                safe.append(name.clone(), v.clone());
            }
        }
        *headers = safe;
    }
    Ok(())
}
