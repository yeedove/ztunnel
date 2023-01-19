// Copyright Istio Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::fmt::Debug;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::{fmt, io};

use boring::error::ErrorStack;
use drain::Watch;
use hyper::{header, Body, Request};
use rand::Rng;
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tracing::{error, trace, warn, Instrument};

use inbound::Inbound;

use crate::identity::CertificateProvider;
use crate::metrics::Metrics;
use crate::proxy::inbound_passthrough::InboundPassthrough;
use crate::proxy::outbound::Outbound;
use crate::proxy::socks5::Socks5;
use crate::workload::WorkloadInformation;
use crate::{config, identity, socket, tls};

mod inbound;
mod inbound_passthrough;
mod outbound;
mod socks5;
mod util;

pub struct Proxy {
    inbound: Inbound,
    inbound_passthrough: InboundPassthrough,
    outbound: Outbound,
    socks5: Socks5,
}

#[derive(Clone)]
pub(super) struct ProxyInputs {
    cfg: config::Config,
    cert_manager: Box<dyn CertificateProvider>,
    hbone_port: u16,
    workloads: WorkloadInformation,
    metrics: Arc<Metrics>,
}

impl Proxy {
    pub async fn new(
        cfg: config::Config,
        workloads: WorkloadInformation,
        cert_manager: Box<dyn CertificateProvider>,
        metrics: Arc<Metrics>,
        drain: Watch,
    ) -> Result<Proxy, Error> {
        let mut pi = ProxyInputs {
            cfg,
            workloads,
            cert_manager,
            metrics,
            hbone_port: 0,
        };
        // We setup all the listeners first so we can capture any errors that should block startup
        let inbound = Inbound::new(pi.clone(), drain.clone()).await?;
        pi.hbone_port = inbound.address().port();

        let inbound_passthrough = InboundPassthrough::new(pi.clone()).await?;
        let outbound = Outbound::new(pi.clone(), drain.clone()).await?;
        let socks5 = Socks5::new(pi.clone(), drain).await?;
        Ok(Proxy {
            inbound,
            inbound_passthrough,
            outbound,
            socks5,
        })
    }

    pub async fn run(self) {
        let tasks = vec![
            tokio::spawn(self.inbound_passthrough.run().in_current_span()),
            tokio::spawn(self.inbound.run().in_current_span()),
            tokio::spawn(self.outbound.run().in_current_span()),
            tokio::spawn(self.socks5.run().in_current_span()),
        ];

        futures::future::join_all(tasks).await;
    }

    pub fn addresses(&self) -> Addresses {
        Addresses {
            outbound: self.outbound.address(),
            inbound: self.inbound.address(),
            socks5: self.socks5.address(),
        }
    }
}

#[derive(Copy, Clone)]
pub struct Addresses {
    pub outbound: SocketAddr,
    pub inbound: SocketAddr,
    pub socks5: SocketAddr,
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("failed to bind to address {0}: {1}")]
    Bind(SocketAddr, io::Error),

    #[error("io error: {0}")]
    Io(#[from] io::Error),

    #[error("tls handshake failed: {0:?}")]
    TlsHandshake(#[from] tokio_boring::HandshakeError<TcpStream>),

    #[error("http handshake failed: {0}")]
    HttpHandshake(#[source] hyper::Error),

    #[error("http failed: {0}")]
    Http(#[from] hyper::Error),

    #[error("http status: {0}")]
    HttpStatus(hyper::StatusCode),

    #[error("tls error: {0}")]
    Tls(#[from] tls::Error),

    #[error("ssl error: {0}")]
    Ssl(#[from] ErrorStack),

    #[error("identity error: {0}")]
    Identity(#[from] identity::Error),

    #[error("unknown source: {0}")]
    UnknownSource(IpAddr),

    #[error("unknown destination: {0}")]
    UnknownDestination(IpAddr),
}

// TLS record size max is 16k. But we also have a H2 frame header, so leave a bit of room for that.
const HBONE_BUFFER_SIZE: usize = 16_384 - 64;

pub async fn copy_hbone(
    upgraded: &mut hyper::upgrade::Upgraded,
    stream: &mut TcpStream,
) -> Result<(u64, u64), std::io::Error> {
    use tokio::io::AsyncWriteExt;
    let (mut ri, mut wi) = tokio::io::split(upgraded);
    let (mut ro, mut wo) = stream.split();

    let (mut sent, mut received): (u64, u64) = (0, 0);

    let client_to_server = async {
        let mut ri = tokio::io::BufReader::with_capacity(HBONE_BUFFER_SIZE, &mut ri);
        let mut wo = tokio::io::BufWriter::with_capacity(HBONE_BUFFER_SIZE, &mut wo);
        let res = tokio::io::copy(&mut ri, &mut wo).await;
        trace!(?res, "hbone -> tcp");
        received = res?;
        wo.shutdown().await
    };

    let server_to_client = async {
        let mut ro = tokio::io::BufReader::with_capacity(HBONE_BUFFER_SIZE, &mut ro);
        let mut wi = tokio::io::BufWriter::with_capacity(HBONE_BUFFER_SIZE, &mut wi);
        let res = tokio::io::copy(&mut ro, &mut wi).await;
        trace!(?res, "tcp -> hbone");
        sent = res?;
        wi.shutdown().await
    };

    tokio::try_join!(client_to_server, server_to_client).map(|_| (sent, received))
}

/// Represents a traceparent, as defined by https://www.w3.org/TR/trace-context/
#[derive(Eq, PartialEq)]
pub struct TraceParent {
    version: u8,
    trace_id: u128,
    parent_id: u64,
    flags: u8,
}

pub const BAGGAGE_HEADER: &str = "baggage";
pub const TRACEPARENT_HEADER: &str = "traceparent";

impl TraceParent {
    pub fn header(&self) -> hyper::header::HeaderValue {
        hyper::header::HeaderValue::from_bytes(format!("{self:?}").as_bytes()).unwrap()
    }
}
impl TraceParent {
    fn new() -> Self {
        let mut rng = rand::thread_rng();
        Self {
            version: 0,
            trace_id: rng.gen(),
            parent_id: rng.gen(),
            flags: 0,
        }
    }
}

impl fmt::Debug for TraceParent {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{:02x}-{:032x}-{:016x}-{:02x}",
            self.version, self.trace_id, self.parent_id, self.flags
        )
    }
}

impl fmt::Display for TraceParent {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:032x}", self.trace_id,)
    }
}

impl TryFrom<&str> for TraceParent {
    type Error = anyhow::Error;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        if value.len() != 55 {
            anyhow::bail!("traceparent malformed length was {}", value.len())
        }

        let segs: Vec<&str> = value.split('-').collect();

        Ok(Self {
            version: u8::from_str_radix(segs[0], 16)?,
            trace_id: u128::from_str_radix(segs[1], 16)?,
            parent_id: u64::from_str_radix(segs[2], 16)?,
            flags: u8::from_str_radix(segs[3], 16)?,
        })
    }
}

pub(super) fn maybe_set_transparent(
    pi: &ProxyInputs,
    listener: &TcpListener,
) -> Result<bool, Error> {
    Ok(match pi.cfg.enable_original_source {
        Some(true) => {
            // Explicitly enabled. Return error if we cannot set it.
            socket::set_transparent(listener)?;
            true
        }
        Some(false) => {
            // Explicitly disabled, don't even attempt to set it.
            false
        }
        None => {
            // Best effort
            socket::set_transparent(listener).is_ok()
        }
    })
}

fn parse_socket_or_ip(i: &str) -> Option<IpAddr> {
    // Remove square brackets around IPv6 address.
    let i = i
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(i);
    i.parse::<SocketAddr>()
        .ok()
        .map(|i| i.ip())
        .or_else(|| i.parse::<IpAddr>().ok())
}

pub fn get_original_src_from_fwded(req: &Request<Body>) -> Option<IpAddr> {
    req.headers()
        .get(header::FORWARDED)
        .and_then(|rh| rh.to_str().ok())
        .and_then(|rh| http_types::proxies::Forwarded::parse(rh).ok())
        .and_then(|ph| {
            ph.forwarded_for()
                .last()
                .and_then(|f| parse_socket_or_ip(f))
        })
}

pub fn get_original_src_from_stream(stream: &TcpStream) -> Option<IpAddr> {
    stream
        .peer_addr()
        .map_or(None, |sa| Some(socket::to_canonical(sa).ip()))
}

pub async fn freebind_connect(local: Option<IpAddr>, addr: SocketAddr) -> io::Result<TcpStream> {
    match local {
        None => Ok(TcpStream::connect(addr).await?),
        // TODO: Need figure out how to handle case of loadbalancing to itself.
        //       We use ztunnel addr instead, otherwise app side will be confused.
        Some(src) if src == socket::to_canonical(addr).ip() => Ok(TcpStream::connect(addr).await?),
        Some(src) => {
            let socket = if src.is_ipv4() {
                TcpSocket::new_v4()?
            } else {
                TcpSocket::new_v6()?
            };

            let local_addr = SocketAddr::new(src, 0);
            match socket::set_freebind_and_transparent(&socket) {
                Err(err) => warn!("failed to set freebind: {:?}", err),
                _ => {
                    if let Err(err) = socket.bind(local_addr) {
                        warn!("failed to bind local addr: {:?}", err)
                    }
                }
            };
            Ok(socket.connect(addr).await?)
        }
    }
}

#[cfg(test)]
mod tests {
    use hyper::http::request;
    use test_case::test_case;

    use super::*;

    #[test_case(r#""#, None; "empty")]
    #[test_case(r#"proto=https"#, None; "no for")]
    #[test_case(r#"abc"#, None; "malformed")]
    #[test_case(r#"for=192.0.2.43"#, Some("192.0.2.43"); "ipv4")]
    #[test_case(r#"for="192.0.2.43""#, Some("192.0.2.43"); "quoted ipv4")]
    #[test_case(r#"for="192.0.2.43:80""#, Some("192.0.2.43"); "ipv4 port")]
    #[test_case(r#"for=192.0.2.43:80"#, None; "unquoted ipv4 port")]
    #[test_case(r#"for="[2001:db8:cafe::17]""#, Some("2001:db8:cafe::17"); "ipv6")]
    #[test_case(r#"for=[2001:db8:cafe::17]"#, None; "unquoted ipv6")]
    #[test_case(r#"for="[2001:db8:cafe::17]:80""#, Some("2001:db8:cafe::17"); "ipv6 port")]
    #[test_case(r#"for=192.0.2.43;proto=https"#, Some("192.0.2.43"); "sections")]
    #[test_case(r#"for=192.0.2.43, for="[2001:db8:cafe::17]";proto=https"#, Some("2001:db8:cafe::17"); "multiple")]
    #[test_case(r#"for=192.0.2.43, for="[2001:db8:cafe::17]", for=unknown;proto=https"#, None; "multiple unmatched")]
    fn string_match(header: &str, expect: Option<&str>) {
        let headers = request::Builder::new()
            .header(header::FORWARDED, header)
            .body(Body::empty())
            .unwrap();
        let expect = expect.map(|i| i.parse::<IpAddr>().unwrap());
        assert_eq!(get_original_src_from_fwded(&headers), expect)
    }
}