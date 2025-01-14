//! Provides the builder and implementation of [`GrpcService`] that enables
//! periodic service discovery.

use crate::{
    service_probe::{GrpcServiceProbe, GrpcServiceProbeConfig},
    DnsResolver, LookupService, ServiceDefinition,
};
use http::Request;
use std::task::{Context, Poll};
use tokio::time::Duration;
use tonic::client::GrpcService;
use tonic::transport::channel::Channel;
use tonic::{body::BoxBody, transport::ClientTlsConfig};
use tower::Service;

// Determines the channel size of the channel we use
// to report endpoint changes to tonic.
// This is effectively how many changes we can report in one go.
// We set the number high to avoid any blocking on our side.
static GRPC_REPORT_ENDPOINTS_CHANNEL_SIZE: usize = 1024;

/// Implements tonic [`GrpcService`] for a client-side load balanced [`Channel`] (using `The Power of
/// Two Choices`).
///
/// [`GrpcService`](tonic::client::GrpcService)
///
/// ```rust
/// #[tokio::main]
/// async fn main() {
///     use ginepro::LoadBalancedChannel;
///     use shared_proto::pb::tester_client::TesterClient;
///
///     let load_balanced_channel = LoadBalancedChannel::builder(("my_hostname", 5000))
///         .await
///         .expect("failed to read system conf")
///         .channel();
///
///     let client = TesterClient::new(load_balanced_channel);
/// }
/// ```
///
#[derive(Debug, Clone)]
pub struct LoadBalancedChannel(Channel);

impl From<LoadBalancedChannel> for Channel {
    fn from(channel: LoadBalancedChannel) -> Self {
        channel.0
    }
}

impl LoadBalancedChannel {
    /// Start configuring a `LoadBalancedChannel` by passing in the [`ServiceDefinition`]
    /// for the gRPC server service you want to call -  e.g. `my.service.uri` and `5000`.
    ///
    /// All the service endpoints of a [`ServiceDefinition`] will be
    /// constructed by resolving IPs for [`ServiceDefinition::hostname`], and
    /// using the port number [`ServiceDefinition::port`].
    pub async fn builder<H: Into<ServiceDefinition>>(
        service_definition: H,
    ) -> Result<LoadBalancedChannelBuilder<DnsResolver>, anyhow::Error> {
        LoadBalancedChannelBuilder::new_with_service(service_definition).await
    }
}

impl Service<http::Request<BoxBody>> for LoadBalancedChannel {
    type Response = http::Response<<Channel as GrpcService<BoxBody>>::ResponseBody>;
    type Error = <Channel as GrpcService<BoxBody>>::Error;
    type Future = <Channel as GrpcService<BoxBody>>::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        GrpcService::poll_ready(&mut self.0, cx)
    }

    fn call(&mut self, request: Request<BoxBody>) -> Self::Future {
        GrpcService::call(&mut self.0, request)
    }
}

/// Builder to configure and create a [`LoadBalancedChannel`].
pub struct LoadBalancedChannelBuilder<T> {
    service_definition: ServiceDefinition,
    probe_interval: Option<Duration>,
    timeout: Option<Duration>,
    tls_config: Option<ClientTlsConfig>,
    lookup_service: T,
}

impl LoadBalancedChannelBuilder<DnsResolver> {
    /// Set the [`ServiceDefinition`] of the gRPC server service
    /// -  e.g. `my.service.uri` and `5000`.
    ///
    /// All the service endpoints of a [`ServiceDefinition`] will be
    /// constructed by resolving all ips from [`ServiceDefinition::hostname`], and
    /// using the portnumber [`ServiceDefinition::port`].
    pub async fn new_with_service<H: Into<ServiceDefinition>>(
        service_definition: H,
    ) -> Result<LoadBalancedChannelBuilder<DnsResolver>, anyhow::Error> {
        Ok(Self {
            service_definition: service_definition.into(),
            probe_interval: None,
            timeout: None,
            tls_config: None,
            lookup_service: DnsResolver::from_system_config().await?,
        })
    }

    /// Set a custom [`LookupService`].
    pub fn lookup_service<T: LookupService + Send + Sync + 'static>(
        self,
        lookup_service: T,
    ) -> LoadBalancedChannelBuilder<T> {
        LoadBalancedChannelBuilder {
            lookup_service,
            service_definition: self.service_definition,
            probe_interval: self.probe_interval,
            tls_config: self.tls_config,
            timeout: self.timeout,
        }
    }
}

impl<T: LookupService + Send + Sync + 'static + Sized> LoadBalancedChannelBuilder<T> {
    /// Returns a `LoadBalancedChannelBuilder` with the [`ServiceDefinition`] and
    /// the customized [`LookupService`].
    pub fn new<H: Into<ServiceDefinition>>(
        service_definition: H,
        lookup_service: T,
    ) -> LoadBalancedChannelBuilder<T> {
        Self {
            service_definition: service_definition.into(),
            probe_interval: None,
            timeout: None,
            tls_config: None,
            lookup_service,
        }
    }

    /// Set the how often, the client should probe for changes to  gRPC server endpoints.
    /// Default interval in seconds is 10.
    pub fn dns_probe_interval(self, interval: Duration) -> LoadBalancedChannelBuilder<T> {
        Self {
            probe_interval: Some(interval),
            ..self
        }
    }

    /// Set a timeout that will be applied to every new `Endpoint`.
    pub fn timeout(self, timeout: Duration) -> LoadBalancedChannelBuilder<T> {
        Self {
            timeout: Some(timeout),
            ..self
        }
    }

    /// Configure the channel to use tls.
    /// A `tls_config` MUST be specified to use the `HTTPS` scheme.
    pub fn with_tls(self, mut tls_config: ClientTlsConfig) -> LoadBalancedChannelBuilder<T> {
        // Since we resolve the hostname to an IP, which is not a valid DNS name,
        // we have to set the hostname explicitly on the tls config,
        // otherwise the IP will be set as the domain name and tls handshake will fail.
        tls_config = tls_config.domain_name(self.service_definition.hostname.clone());

        Self {
            tls_config: Some(tls_config),
            ..self
        }
    }

    /// Construct a [`LoadBalancedChannel`] from the [`LoadBalancedChannelBuilder`] instance.
    pub fn channel(self) -> LoadBalancedChannel {
        let (channel, sender) = Channel::balance_channel(GRPC_REPORT_ENDPOINTS_CHANNEL_SIZE);

        let config = GrpcServiceProbeConfig {
            service_definition: self.service_definition,
            dns_lookup: self.lookup_service,
            endpoint_timeout: self.timeout,
            probe_interval: self
                .probe_interval
                .unwrap_or_else(|| Duration::from_secs(10)),
        };
        let mut service_probe = GrpcServiceProbe::new_with_reporter(config, sender);

        if let Some(tls_config) = self.tls_config {
            service_probe = service_probe.with_tls(tls_config);
        }

        tokio::spawn(service_probe.probe());

        LoadBalancedChannel(channel)
    }
}
