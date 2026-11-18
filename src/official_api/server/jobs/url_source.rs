//! Server-side downloads for `url` task inputs.
//!
//! Every hop is resolved and validated before connecting, then pinned to the
//! validated address while the URL host still supplies Host, SNI and the
//! certificate name. This mirrors upstream `parser/url_source.py`.
use super::{ServiceInner, fail_with};
use crate::official_api::wire::{TaskFileState, TaskStatus};
use bytes::Bytes;
use futures_util::StreamExt;
use sha2::Digest;
use std::{
    net::{IpAddr, Ipv6Addr, SocketAddr},
    sync::{Arc, atomic::Ordering},
    time::Duration,
};
use tokio::sync::Mutex;

use super::TaskRecord;

const URL_TIMEOUT_SECS: u64 = 60;
const URL_BYTE_CAP: u64 = 64 * 1024 * 1024;
const MAX_REDIRECTS: usize = 10;
const PUBLIC_ONLY: &str = "URL sources must resolve only to public unicast IP addresses";

/// Blocked IPv4 ranges: the private/special set plus the ranges upstream adds
/// because some Python versions classify them as global.
const BLOCKED_V4: [(u32, u32); 18] = [
    (0x0000_0000, 8),
    (0x0A00_0000, 8),
    (0x6440_0000, 10),
    (0x7F00_0000, 8),
    (0xA9FE_0000, 16),
    (0xAC10_0000, 12),
    (0xC000_0000, 29),
    (0xC000_0200, 24),
    (0xC0A8_0000, 16),
    (0xC612_0000, 15),
    (0xC633_6400, 24),
    (0xCB00_7100, 24),
    (0xE000_0000, 4),
    (0xF000_0000, 4),
    (0xFFFF_FFFF, 32),
    (0xC000_0000, 24),
    (0xC058_6300, 24),
    (0xA83F_8110, 32),
];

#[derive(Debug)]
struct FetchError {
    code: &'static str,
    message: String,
}

impl FetchError {
    fn failed() -> Self {
        Self {
            code: "url_source_failed",
            message: "URL download failed".to_owned(),
        }
    }

    fn unsupported(message: &'static str) -> Self {
        Self {
            code: "unsupported_source",
            message: message.to_owned(),
        }
    }
}

/// Resolves every address and rejects the whole set before any connection.
async fn resolve(host: &str, port: u16) -> Result<Vec<IpAddr>, FetchError> {
    let resolved = tokio::net::lookup_host((host, port))
        .await
        .map_err(|_| FetchError::failed())?;
    let mut addresses: Vec<IpAddr> = Vec::new();
    for socket in resolved {
        let address = public_address(socket.ip())?;
        if !addresses.contains(&address) {
            addresses.push(address);
        }
    }
    if addresses.is_empty() {
        return Err(FetchError::failed());
    }
    Ok(addresses)
}

fn public_address(address: IpAddr) -> Result<IpAddr, FetchError> {
    let address = match address {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
        IpAddr::V4(_) => address,
    };
    if is_public(address) {
        Ok(address)
    } else {
        Err(FetchError::unsupported(PUBLIC_ONLY))
    }
}

fn is_public(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => {
            let value = u32::from(v4);
            !BLOCKED_V4.iter().any(|(network, bits)| {
                let mask = u32::MAX << (32 - bits);
                (value & mask) == (network & mask)
            })
        }
        IpAddr::V6(v6) => public_v6(v6),
    }
}

fn public_v6(v6: Ipv6Addr) -> bool {
    let s = v6.segments();
    let unspecified = s == [0; 8];
    let loopback = s == [0, 0, 0, 0, 0, 0, 0, 1];
    let unique_local = (s[0] & 0xfe00) == 0xfc00;
    let link_local = (s[0] & 0xffc0) == 0xfe80;
    let multicast = (s[0] & 0xff00) == 0xff00;
    let discard_only = s[0] == 0x0100 && s[1..4].iter().all(|part| *part == 0);
    let documentation = s[0] == 0x2001 && s[1] == 0x0db8;
    let mapped = s[..5].iter().all(|part| *part == 0) && s[5] == 0xffff;
    let blocked = (s[0] == 0x0064 && s[1] == 0xff9b && s[2..6].iter().all(|part| *part == 0))
        || (s[0] == 0x0064 && s[1] == 0xff9b && s[2] == 1)
        || (s[0] == 0x2001 && s[1] <= 0x01ff)
        || s[0] == 0x2002;
    !(unspecified
        || loopback
        || unique_local
        || link_local
        || multicast
        || discard_only
        || documentation
        || mapped
        || blocked)
}

pub(super) async fn download(
    s: &ServiceInner,
    shared: &Arc<Mutex<TaskRecord>>,
    index: usize,
) -> Result<(), ()> {
    let (url, declared, sha) = {
        let mut task = shared.lock().await;
        let file = task.files.get_mut(index).ok_or(())?;
        file.started_at = file
            .started_at
            .clone()
            .or_else(|| Some(super::super::store::timestamp()));
        (file.url.clone(), file.declared, file.sha256sum.clone())
    };
    let Some(url) = url else {
        fail_with(
            s,
            shared,
            index,
            "url_source_failed",
            "URL input is missing a source URL",
        )
        .await;
        return Err(());
    };
    let cap = s.store.file_cap().min(URL_BYTE_CAP);
    let bytes = match fetch(&url, cap).await {
        Ok(bytes) => bytes,
        Err(error) => {
            fail_with(s, shared, index, error.code, &error.message).await;
            return Err(());
        }
    };
    if bytes.is_empty() {
        fail_with(
            s,
            shared,
            index,
            "invalid_request",
            "Empty files are not supported",
        )
        .await;
        return Err(());
    }
    if declared.is_some_and(|declared| declared != bytes.len() as u64) {
        fail_with(
            s,
            shared,
            index,
            "bytes_mismatch",
            "Declared size does not match downloaded bytes",
        )
        .await;
        return Err(());
    }
    if let Some(expected) = &sha {
        let digest: String = sha2::Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        if &digest != expected {
            fail_with(s, shared, index, "file_hash_mismatch", "SHA-256 mismatch").await;
            return Err(());
        }
    }
    let size = bytes.len() as u64;
    let Some(reservation) = s.store.input_reservation(size) else {
        fail_with(
            s,
            shared,
            index,
            "capacity_exceeded",
            "Input storage capacity is exhausted.",
        )
        .await;
        return Err(());
    };
    let data: Bytes = s.store.charged(bytes, reservation);
    let mut task = shared.lock().await;
    if task.status == TaskStatus::Canceled || s.stopping.load(Ordering::Acquire) {
        return Err(());
    }
    if let Some(file) = task.files.get_mut(index) {
        file.data = Some(data);
        file.actual = Some(size);
        file.uploaded = true;
        file.upload_active = false;
        file.status = TaskFileState::Pending;
    }
    s.store.aggregate(&mut task);
    Ok(())
}

/// Validates and follows redirects one hop at a time, so every hop is checked.
async fn fetch(url: &str, cap: u64) -> Result<Vec<u8>, FetchError> {
    let mut current = reqwest::Url::parse(url).map_err(|_| FetchError::failed())?;
    for _ in 0..=MAX_REDIRECTS {
        if !matches!(current.scheme(), "http" | "https") {
            return Err(FetchError::unsupported(
                "URL sources must use HTTP or HTTPS",
            ));
        }
        let host = current
            .host_str()
            .ok_or_else(FetchError::failed)?
            .to_owned();
        if host.contains('%') {
            return Err(FetchError::unsupported(
                "URL sources must not contain an IPv6 zone identifier",
            ));
        }
        let port = current
            .port_or_known_default()
            .ok_or_else(FetchError::failed)?;
        let addresses = resolve(&host, port).await?;
        let response = get(&current, &host, port, &addresses).await?;
        if response.status().is_redirection() {
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(FetchError::failed)?;
            current = current.join(location).map_err(|_| FetchError::failed())?;
            continue;
        }
        if !response.status().is_success() {
            return Err(FetchError {
                code: "url_source_failed",
                message: format!(
                    "URL download failed with status {}",
                    response.status().as_u16()
                ),
            });
        }
        return read_body(response, cap).await;
    }
    Err(FetchError {
        code: "url_source_failed",
        message: "URL download failed: too many redirects".to_owned(),
    })
}

async fn read_body(response: reqwest::Response, cap: u64) -> Result<Vec<u8>, FetchError> {
    let too_large = || FetchError {
        code: "url_source_failed",
        message: "URL source exceeds the file size limit".to_owned(),
    };
    if response.content_length().is_some_and(|length| length > cap) {
        return Err(too_large());
    }
    let mut stream = response.bytes_stream();
    let mut data = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| FetchError::failed())?;
        if data.len() as u64 + chunk.len() as u64 > cap {
            return Err(too_large());
        }
        data.extend_from_slice(&chunk);
    }
    Ok(data)
}

/// Pins each validated address in turn; only connection failures fall through.
async fn get(
    url: &reqwest::Url,
    host: &str,
    port: u16,
    addresses: &[IpAddr],
) -> Result<reqwest::Response, FetchError> {
    for address in addresses {
        let Ok(client) = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(URL_TIMEOUT_SECS))
            .resolve(host, SocketAddr::new(*address, port))
            .build()
        else {
            continue;
        };
        if let Ok(response) = client.get(url.clone()).send().await {
            return Ok(response);
        }
    }
    Err(FetchError::failed())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn public(value: &str) -> bool {
        is_public(value.parse().expect("valid IP"))
    }

    #[test]
    fn rejects_non_public_addresses() {
        for blocked in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "192.0.0.8",
            "192.88.99.1",
            "168.63.129.16",
            "0.0.0.0",
            "255.255.255.255",
            "224.0.0.1",
            "::1",
            "::",
            "fd00::1",
            "fe80::1",
            "ff02::1",
            "2001:db8::1",
            "64:ff9b::7f00:1",
            "2002:7f00:1::",
        ] {
            assert!(!public(blocked), "{blocked}");
        }
    }

    #[test]
    fn accepts_public_addresses() {
        for allowed in ["1.1.1.1", "93.184.216.34", "2606:4700:4700::1111"] {
            assert!(public(allowed), "{allowed}");
        }
    }

    #[test]
    fn maps_ipv4_mapped_addresses_before_validation() {
        let mapped: IpAddr = "::ffff:127.0.0.1".parse().expect("valid IP");
        assert!(public_address(mapped).is_err());
        let mapped: IpAddr = "::ffff:1.1.1.1".parse().expect("valid IP");
        assert_eq!(
            public_address(mapped).expect("public"),
            "1.1.1.1".parse::<IpAddr>().expect("valid IP")
        );
    }
}
