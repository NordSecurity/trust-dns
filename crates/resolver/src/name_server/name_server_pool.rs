// Copyright 2015-2019 Benjamin Fry <benjaminfry@me.com>
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// https://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// https://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

use std::cmp::Ordering;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::future::FutureExt;
use futures_util::stream::{once, FuturesUnordered, Stream, StreamExt};
use smallvec::SmallVec;

use proto::xfer::{DnsHandle, DnsRequest, DnsResponse, FirstAnswer};
use proto::Time;
use tracing::debug;

use rand::thread_rng as rng;
use rand::Rng;

use crate::config::{NameServerConfigGroup, ResolverConfig, ResolverOpts, ServerOrderingStrategy};
use crate::error::{ResolveError, ResolveErrorKind};
#[cfg(feature = "mdns")]
use crate::name_server;
use crate::name_server::connection_provider::{ConnectionProvider, GenericConnector};
use crate::name_server::name_server::NameServer;
use crate::name_server::RuntimeProvider;
#[cfg(test)]
#[cfg(feature = "tokio-runtime")]
use crate::name_server::TokioRuntimeProvider;

/// Abstract interface for mocking purpose
#[derive(Clone)]
pub struct NameServerPool<P: ConnectionProvider + Send + 'static> {
    // TODO: switch to FuturesMutex (Mutex will have some undesirable locking)
    datagram_conns: Arc<[NameServer<P>]>, /* All NameServers must be the same type */
    stream_conns: Arc<[NameServer<P>]>,   /* All NameServers must be the same type */
    #[cfg(feature = "mdns")]
    mdns_conns: NameServer<P>, /* All NameServers must be the same type */
    options: ResolverOpts,
}

/// A pool of NameServers
///
/// This is not expected to be used directly, see [crate::AsyncResolver].
pub type GenericNameServerPool<P> = NameServerPool<GenericConnector<P>>;

#[cfg(test)]
#[cfg(feature = "tokio-runtime")]
impl GenericNameServerPool<TokioRuntimeProvider> {
    pub(crate) fn tokio_from_config(
        config: &ResolverConfig,
        options: ResolverOpts,
        runtime: TokioRuntimeProvider,
    ) -> Self {
        Self::from_config_with_provider(config, options, GenericConnector::new(runtime))
    }
}

impl<P> NameServerPool<P>
where
    P: ConnectionProvider + 'static,
{
    pub(crate) fn from_config_with_provider(
        config: &ResolverConfig,
        options: ResolverOpts,
        conn_provider: P,
    ) -> Self {
        let datagram_conns: Vec<NameServer<P>> = config
            .name_servers()
            .iter()
            .filter(|ns_config| ns_config.protocol.is_datagram())
            .map(|ns_config| {
                #[cfg(feature = "dns-over-rustls")]
                let ns_config = {
                    let mut ns_config = ns_config.clone();
                    ns_config.tls_config = config.client_config().clone();
                    ns_config
                };
                #[cfg(not(feature = "dns-over-rustls"))]
                let ns_config = { ns_config.clone() };

                NameServer::new(ns_config, options.clone(), conn_provider.clone())
            })
            .collect();

        let stream_conns: Vec<NameServer<P>> = config
            .name_servers()
            .iter()
            .filter(|ns_config| ns_config.protocol.is_stream())
            .map(|ns_config| {
                #[cfg(feature = "dns-over-rustls")]
                let ns_config = {
                    let mut ns_config = ns_config.clone();
                    ns_config.tls_config = config.client_config().clone();
                    ns_config
                };
                #[cfg(not(feature = "dns-over-rustls"))]
                let ns_config = { ns_config.clone() };

                NameServer::new(ns_config, options.clone(), conn_provider.clone())
            })
            .collect();

        Self {
            datagram_conns: Arc::from(datagram_conns),
            stream_conns: Arc::from(stream_conns),
            #[cfg(feature = "mdns")]
            mdns_conns: name_server::mdns_nameserver(options, conn_provider.clone(), false),
            options,
        }
    }

    /// Construct a NameServerPool from a set of name server configs
    pub fn from_config(
        name_servers: NameServerConfigGroup,
        options: ResolverOpts,
        conn_provider: P,
    ) -> Self {
        let map_config_to_ns =
            |ns_config| NameServer::new(ns_config, options.clone(), conn_provider.clone());

        let (datagram, stream): (Vec<_>, Vec<_>) = name_servers
            .into_inner()
            .into_iter()
            .partition(|ns| ns.protocol.is_datagram());

        let datagram_conns: Vec<_> = datagram.into_iter().map(map_config_to_ns).collect();
        let stream_conns: Vec<_> = stream.into_iter().map(map_config_to_ns).collect();

        Self {
            datagram_conns: Arc::from(datagram_conns),
            stream_conns: Arc::from(stream_conns),
            #[cfg(feature = "mdns")]
            mdns_conns: name_server::mdns_nameserver(*options, conn_provider.clone(), false),
            options,
        }
    }

    #[doc(hidden)]
    #[cfg(not(feature = "mdns"))]
    pub fn from_nameservers(
        options: ResolverOpts,
        datagram_conns: Vec<NameServer<P>>,
        stream_conns: Vec<NameServer<P>>,
    ) -> Self {
        Self {
            datagram_conns: Arc::from(datagram_conns),
            stream_conns: Arc::from(stream_conns),
            options,
        }
    }

    #[doc(hidden)]
    #[cfg(feature = "mdns")]
    pub fn from_nameservers(
        options: ResolverOpts,
        datagram_conns: Vec<NameServer<P>>,
        stream_conns: Vec<NameServer<P>>,
        mdns_conns: NameServer<P>,
    ) -> Self {
        GenericNameServerPool {
            datagram_conns: Arc::from(datagram_conns),
            stream_conns: Arc::from(stream_conns),
            mdns_conns,
            options,
        }
    }

    #[cfg(test)]
    #[cfg(not(feature = "mdns"))]
    #[allow(dead_code)]
    fn from_nameservers_test(
        options: ResolverOpts,
        datagram_conns: Arc<[NameServer<P>]>,
        stream_conns: Arc<[NameServer<P>]>,
    ) -> Self {
        Self {
            datagram_conns,
            stream_conns,
            options,
        }
    }

    #[cfg(test)]
    #[cfg(feature = "mdns")]
    fn from_nameservers_test(
        options: &ResolverOpts,
        datagram_conns: Arc<[NameServer<P>]>,
        stream_conns: Arc<[NameServer<P>]>,
        mdns_conns: NameServer<P>,
    ) -> Self {
        GenericNameServerPool {
            datagram_conns,
            stream_conns,
            mdns_conns,
            options: *options,
        }
    }

    async fn try_send(
        opts: ResolverOpts,
        conns: Arc<[NameServer<P>]>,
        request: DnsRequest,
    ) -> Result<DnsResponse, ResolveError> {
        let mut conns: Vec<NameServer<P>> = conns.to_vec();

        match opts.server_ordering_strategy {
            // select the highest priority connection
            //   reorder the connections based on current view...
            //   this reorders the inner set
            ServerOrderingStrategy::QueryStatistics => conns.sort_unstable(),
            ServerOrderingStrategy::UserProvidedOrder => {}
        }
        let request_loop = request.clone();

        parallel_conn_loop(conns, request_loop, opts).await
    }
}

impl<P> DnsHandle for NameServerPool<P>
where
    P: ConnectionProvider + 'static,
{
    type Response = Pin<Box<dyn Stream<Item = Result<DnsResponse, ResolveError>> + Send>>;
    type Error = ResolveError;

    fn send<R: Into<DnsRequest>>(&self, request: R) -> Self::Response {
        tracing::debug!("Sending request");
        let opts = self.options.clone();
        let request = request.into();
        let datagram_conns = Arc::clone(&self.datagram_conns);
        let stream_conns = Arc::clone(&self.stream_conns);
        // TODO: remove this clone, return the Message in the error?
        let tcp_message = request.clone();

        // if it's a .local. query, then we *only* query mDNS, these should never be sent on to upstream resolvers
        #[cfg(feature = "mdns")]
        let mdns = mdns::maybe_local(&mut self.mdns_conns, request);

        // TODO: limited to only when mDNS is enabled, but this should probably always be enforced?
        #[cfg(not(feature = "mdns"))]
        let mdns = Local::NotMdns(request);

        // local queries are queried through mDNS
        if mdns.is_local() {
            return mdns.take_stream();
        }

        // TODO: should we allow mDNS to be used for standard lookups as well?

        // it wasn't a local query, continue with standard lookup path
        let request = mdns.take_request();
        Box::pin(once(async move {
            debug!("sending request: {:?}", request.queries());

            // First try the UDP connections
            let udp_res = match Self::try_send(opts.clone(), datagram_conns, request).await {
                Ok(response) if response.truncated() => {
                    debug!("truncated response received, retrying over TCP");
                    Ok(response)
                }
                Err(e) if opts.try_tcp_on_error || e.is_no_connections() => {
                    debug!("error from UDP, retrying over TCP: {}", e);
                    Err(e)
                }
                result => return result,
            };

            if stream_conns.is_empty() {
                debug!("no TCP connections available");
                return udp_res;
            }

            // Try query over TCP, as response to query over UDP was either truncated or was an
            // error.
            let tcp_res = Self::try_send(opts, stream_conns, tcp_message).await;

            let tcp_err = match tcp_res {
                res @ Ok(..) => return res,
                Err(e) => e,
            };

            // Even if the UDP result was truncated, return that
            let udp_err = match udp_res {
                Ok(response) => return Ok(response),
                Err(e) => e,
            };

            match udp_err.cmp_specificity(&tcp_err) {
                Ordering::Greater => Err(udp_err),
                _ => Err(tcp_err),
            }
        }))
    }
}

// TODO: we should be able to have a self-referential future here with Pin and not require cloned conns
/// An async function that will loop over all the conns with a max parallel request count of ops.num_concurrent_req
async fn parallel_conn_loop<P>(
    mut conns: Vec<NameServer<P>>,
    request: DnsRequest,
    opts: ResolverOpts,
) -> Result<DnsResponse, ResolveError>
where
    P: ConnectionProvider + 'static,
{
    let mut err = ResolveError::no_connections();
    // If the name server we're trying is giving us backpressure by returning ProtoErrorKind::Busy,
    // we will first try the other name servers (as for other error types). However, if the other
    // servers are also busy, we're going to wait for a little while and then retry each server that
    // returned Busy in the previous round. If the server is still Busy, this continues, while
    // the backoff increases exponentially (by a factor of 2), until it hits 300ms, in which case we
    // give up. The request might still be retried by the caller (likely the DnsRetryHandle).
    //
    // TODO: more principled handling of timeouts. Currently, timeouts appear to be handled mostly
    // close to the connection, which means the top level resolution might take substantially longer
    // to fire than the timeout configured in `ResolverOpts`.
    let mut backoff = Duration::from_millis(20);
    let mut busy = SmallVec::<[NameServer<P>; 2]>::new();

    loop {
        let request_cont = request.clone();

        // construct the parallel requests, 2 is the default
        let mut par_conns = SmallVec::<[NameServer<P>; 2]>::new();
        let count = conns.len().min(opts.num_concurrent_reqs.max(1));

        // Shuffe DNS NameServers to avoid overloads to the first configured ones
        if opts.shuffle_dns_servers {
            for _ in 0..count {
                let idx = rng().gen_range(0..conns.len());

                // UNWRAP: swap_remove has an implicit panicking bounds check. This should
                // never fail because we check that conns is not empty and generate the idx
                // to explicitly be in range.
                par_conns.push(conns.swap_remove(idx));
            }
        } else {
            for conn in conns.drain(..count) {
                par_conns.push(conn);
            }
        }

        if par_conns.is_empty() {
            if !busy.is_empty() && backoff < Duration::from_millis(300) {
                <<P as ConnectionProvider>::RuntimeProvider as RuntimeProvider>::Timer::delay_for(
                    backoff,
                )
                .await;
                conns.extend(busy.drain(..));
                backoff *= 2;
                continue;
            }
            return Err(err);
        }

        let mut requests = par_conns
            .into_iter()
            .map(move |conn| {
                conn.send(request_cont.clone())
                    .first_answer()
                    .map(|result| result.map_err(|e| (conn, e)))
            })
            .collect::<FuturesUnordered<_>>();

        while let Some(result) = requests.next().await {
            let (conn, e) = match result {
                Ok(sent) => return Ok(sent),
                Err((conn, e)) => (conn, e),
            };

            match e.kind() {
                ResolveErrorKind::NoRecordsFound { trusted, .. } if *trusted => {
                    return Err(e);
                }
                ResolveErrorKind::Proto(e) if e.is_busy() => {
                    busy.push(conn);
                }
                _ if err.cmp_specificity(&e) == Ordering::Less => {
                    err = e;
                }
                _ => {}
            }
        }
    }
}

#[cfg(feature = "mdns")]
mod mdns {
    use super::*;

    use proto::rr::domain::usage;
    use proto::DnsHandle;

    /// Returns true
    pub(crate) fn maybe_local<C, P>(
        name_server: &mut NameServer<C, P>,
        request: DnsRequest,
    ) -> Local
    where
        C: DnsHandle<Error = ResolveError> + 'static,
        P: ConnectionProvider<Conn = C> + 'static,
        P: ConnectionProvider,
    {
        if request
            .queries()
            .iter()
            .any(|query| usage::LOCAL.name().zone_of(query.name()))
        {
            Local::ResolveStream(name_server.send(request))
        } else {
            Local::NotMdns(request)
        }
    }
}

#[allow(clippy::large_enum_variant)]
pub(crate) enum Local {
    #[allow(dead_code)]
    ResolveStream(Pin<Box<dyn Stream<Item = Result<DnsResponse, ResolveError>> + Send>>),
    NotMdns(DnsRequest),
}

impl Local {
    fn is_local(&self) -> bool {
        matches!(*self, Self::ResolveStream(..))
    }

    /// Takes the stream
    ///
    /// # Panics
    ///
    /// Panics if this is in fact a Local::NotMdns
    fn take_stream(self) -> Pin<Box<dyn Stream<Item = Result<DnsResponse, ResolveError>> + Send>> {
        match self {
            Self::ResolveStream(future) => future,
            _ => panic!("non Local queries have no future, see take_message()"),
        }
    }

    /// Takes the message
    ///
    /// # Panics
    ///
    /// Panics if this is in fact a Local::ResolveStream
    fn take_request(self) -> DnsRequest {
        match self {
            Self::NotMdns(request) => request,
            _ => panic!("Local queries must be polled, see take_future()"),
        }
    }
}

impl Stream for Local {
    type Item = Result<DnsResponse, ResolveError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match *self {
            Self::ResolveStream(ref mut ns) => ns.as_mut().poll_next(cx),
            // TODO: making this a panic for now
            Self::NotMdns(..) => panic!("Local queries that are not mDNS should not be polled"), //Local::NotMdns(message) => return Err(ResolveErrorKind::Message("not mDNS")),
        }
    }
}

#[cfg(test)]
#[cfg(feature = "tokio-runtime")]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
    use std::str::FromStr;

    use tokio::runtime::Runtime;

    use hickory_proto::rr::RData;
    use proto::op::Query;
    use proto::rr::{Name, RecordType};
    use proto::xfer::{DnsHandle, DnsRequestOptions};

    use super::*;
    use crate::config::NameServerConfig;
    use crate::config::Protocol;
    use crate::name_server::TokioRuntimeProvider;
    use crate::name_server::{GenericNameServer, TokioConnectionProvider};

    #[ignore]
    // because of there is a real connection that needs a reasonable timeout
    #[test]
    #[allow(clippy::uninlined_format_args)]
    fn test_failed_then_success_pool() {
        let config1 = NameServerConfig {
            socket_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 252)), 253),
            protocol: Protocol::Udp,
            tls_dns_name: None,
            trust_negative_responses: false,
            #[cfg(feature = "dns-over-rustls")]
            tls_config: None,
            bind_addr: None,
        };

        let config2 = NameServerConfig {
            socket_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)), 53),
            protocol: Protocol::Udp,
            tls_dns_name: None,
            trust_negative_responses: false,
            #[cfg(feature = "dns-over-rustls")]
            tls_config: None,
            bind_addr: None,
        };

        let mut resolver_config = ResolverConfig::new();
        resolver_config.add_name_server(config1);
        resolver_config.add_name_server(config2);

        let io_loop = Runtime::new().unwrap();
        let pool = GenericNameServerPool::tokio_from_config(
            &resolver_config,
            ResolverOpts::default(),
            TokioRuntimeProvider::new(),
        );

        let name = Name::parse("www.example.com.", None).unwrap();

        // TODO: it's not clear why there are two failures before the success
        for i in 0..2 {
            assert!(
                io_loop
                    .block_on(
                        pool.lookup(
                            Query::query(name.clone(), RecordType::A),
                            DnsRequestOptions::default()
                        )
                        .first_answer()
                    )
                    .is_err(),
                "iter: {}",
                i
            );
        }

        for i in 0..10 {
            assert!(
                io_loop
                    .block_on(
                        pool.lookup(
                            Query::query(name.clone(), RecordType::A),
                            DnsRequestOptions::default()
                        )
                        .first_answer()
                    )
                    .is_ok(),
                "iter: {}",
                i
            );
        }
    }

    #[test]
    fn test_multi_use_conns() {
        let io_loop = Runtime::new().unwrap();
        let conn_provider = TokioConnectionProvider::default();

        let tcp = NameServerConfig {
            socket_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)), 53),
            protocol: Protocol::Tcp,
            tls_dns_name: None,
            trust_negative_responses: false,
            #[cfg(feature = "dns-over-rustls")]
            tls_config: None,
            bind_addr: None,
        };

        let opts = ResolverOpts {
            try_tcp_on_error: true,
            ..ResolverOpts::default()
        };
        let ns_config = { tcp };
        let name_server = GenericNameServer::new(ns_config, opts.clone(), conn_provider);
        let name_servers: Arc<[_]> = Arc::from([name_server]);

        #[cfg(not(feature = "mdns"))]
        let pool = GenericNameServerPool::from_nameservers_test(
            opts,
            Arc::from([]),
            Arc::clone(&name_servers),
        );
        #[cfg(feature = "mdns")]
        let mut pool = GenericNameServerPool::from_nameservers_test(
            &opts,
            Arc::from([]),
            Arc::clone(&name_servers),
            name_server::mdns_nameserver(opts, TokioConnectionProvider::default(), false),
        );

        let name = Name::from_str("www.example.com.").unwrap();

        // first lookup
        let response = io_loop
            .block_on(
                pool.lookup(
                    Query::query(name.clone(), RecordType::A),
                    DnsRequestOptions::default(),
                )
                .first_answer(),
            )
            .expect("lookup failed");

        assert_eq!(
            *response.answers()[0]
                .data()
                .and_then(RData::as_a)
                .expect("no a record available"),
            Ipv4Addr::new(93, 184, 215, 14).into()
        );

        assert!(
            name_servers[0].is_connected(),
            "if this is failing then the NameServers aren't being properly shared."
        );

        // first lookup
        let response = io_loop
            .block_on(
                pool.lookup(
                    Query::query(name, RecordType::AAAA),
                    DnsRequestOptions::default(),
                )
                .first_answer(),
            )
            .expect("lookup failed");

        assert_eq!(
            *response.answers()[0]
                .data()
                .and_then(RData::as_aaaa)
                .expect("no aaaa record available"),
            Ipv6Addr::new(0x2606, 0x2800, 0x021f, 0xcb07, 0x6820, 0x80da, 0xaf6b, 0x8b2c).into()
        );

        assert!(
            name_servers[0].is_connected(),
            "if this is failing then the NameServers aren't being properly shared."
        );
    }
}
