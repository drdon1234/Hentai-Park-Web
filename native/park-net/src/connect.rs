use crate::{policy::public_address, Error};
use rustls::{pki_types::ServerName, ClientConfig};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{lookup_host, TcpStream};
use tokio_rustls::TlsConnector;
use url::Url;

pub(crate) trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}
pub(crate) type Socket = Box<dyn Stream>;

async fn tls(io: Socket, host: &str, config: Arc<ClientConfig>) -> Result<Socket, Error> {
    let name = ServerName::try_from(host.to_owned()).map_err(|_| Error::Blocked)?;
    Ok(Box::new(
        TlsConnector::from(config)
            .connect(name, io)
            .await
            .map_err(|_| Error::Tls)?,
    ))
}

async fn tunnel(
    proxy: &Url,
    target: SocketAddr,
    config: Arc<ClientConfig>,
) -> Result<Socket, Error> {
    let host = proxy
        .host_str()
        .ok_or(Error::Blocked)?
        .trim_matches(['[', ']']);
    let port = proxy.port_or_known_default().ok_or(Error::Blocked)?;
    let mut io: Socket = Box::new(
        TcpStream::connect((host, port))
            .await
            .map_err(|_| Error::Network)?,
    );
    if proxy.scheme() == "https" {
        io = tls(io, host, config).await?;
    }
    // 代理只接收已核准的目标 IP，不能通过二次解析改变目的地址。
    io.write_all(format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n").as_bytes())
        .await
        .map_err(|_| Error::Network)?;
    let mut reply = Vec::new();
    while !reply.ends_with(b"\r\n\r\n") {
        if reply.len() >= 8192 {
            return Err(Error::Limit);
        }
        reply.push(io.read_u8().await.map_err(|_| Error::Network)?);
    }
    let line = reply.split(|&b| b == b'\r').next().ok_or(Error::Network)?;
    let mut columns = line.split(|&b| b == b' ');
    if !matches!(columns.next(), Some(b"HTTP/1.1" | b"HTTP/1.0")) || columns.next() != Some(b"200")
    {
        return Err(Error::Network);
    }
    Ok(io)
}

pub(crate) async fn connect(
    url: &Url,
    pinned: Option<IpAddr>,
    proxy: Option<&Url>,
    config: Arc<ClientConfig>,
) -> Result<Socket, Error> {
    connect_socket(url, pinned, proxy, config, true).await
}

/// 仅原生用户配置的 AI 服务可使用本机地址，插件始终走公共地址检查。
pub(crate) async fn connect_model(
    url: &Url,
    proxy: Option<&Url>,
    config: Arc<ClientConfig>,
) -> Result<Socket, Error> {
    let local = matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
    if !local && url.scheme() != "https" {
        return Err(Error::Blocked);
    }
    let pinned = match url.host_str() {
        Some("localhost" | "127.0.0.1") => Some(std::net::Ipv4Addr::LOCALHOST.into()),
        Some("[::1]") => Some(std::net::Ipv6Addr::LOCALHOST.into()),
        _ => None,
    };
    connect_socket(url, pinned, if local { None } else { proxy }, config, false).await
}
async fn connect_socket(
    url: &Url,
    pinned: Option<IpAddr>,
    proxy: Option<&Url>,
    config: Arc<ClientConfig>,
    public_only: bool,
) -> Result<Socket, Error> {
    let host = url
        .host_str()
        .ok_or(Error::Blocked)?
        .trim_matches(['[', ']']);
    let port = url.port_or_known_default().ok_or(Error::Blocked)?;
    let addresses = if let Some(ip) = pinned {
        vec![SocketAddr::new(ip, port)]
    } else {
        lookup_host((host, port))
            .await
            .map_err(|_| Error::Network)?
            .collect()
    };
    if addresses.is_empty() || public_only && addresses.iter().any(|a| !public_address(a.ip())) {
        return Err(Error::Blocked);
    }
    let mut last = Error::Network;
    for address in addresses {
        let attempt = async {
            let socket = if let Some(proxy) = proxy {
                tunnel(proxy, address, config.clone()).await?
            } else {
                let stream = TcpStream::connect(address)
                    .await
                    .map_err(|_| Error::Network)?;
                if stream.peer_addr().map_err(|_| Error::Network)? != address {
                    return Err(Error::Blocked);
                }
                Box::new(stream) as Socket
            };
            if url.scheme() == "https" {
                tls(socket, host, config.clone()).await
            } else {
                Ok(socket)
            }
        };
        match tokio::time::timeout(Duration::from_secs(5), attempt).await {
            Ok(Ok(io)) => return Ok(io),
            Ok(Err(error)) => last = error,
            Err(_) => last = Error::Timeout,
        }
    }
    Err(last)
}
