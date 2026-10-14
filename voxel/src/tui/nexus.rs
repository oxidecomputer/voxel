// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Nexus access for monitoring. Finds the rack's Nexus among the configured
//! service-pool addresses or through external DNS, logs into the recovery
//! silo, and runs OxQL timeseries queries, failing over between endpoints.
//! Requests and responses use omicron's generated `oxide-client`.

use std::{future::Future, net::Ipv4Addr, time::Duration};

use anyhow::{Context, anyhow, ensure};
use chrono::{DateTime, Utc};
use futures::{StreamExt, stream::FuturesUnordered};
pub(crate) use oxide_client::types::{MetricType, OxqlQueryResult};
use oxide_client::{
    ClientLoginExt, ClientSystemMetricsExt, ClientSystemStatusExt,
    ResponseValue,
    types::{
        FieldValue, Name, Timeseries, TimeseriesQuery,
        UsernamePasswordCredentials, ValueArray,
    },
};
use reqwest::{StatusCode, Url};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use voxel_config::{Network, RecoverySiloCfg, VoxelConfig};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RecoveryLogin {
    pub(crate) silo: String,
    pub(crate) username: String,
    pub(crate) password: String,
}

impl RecoveryLogin {
    pub(crate) fn from_config(config: &VoxelConfig) -> Option<Self> {
        (config.recovery_silo == RecoverySiloCfg::default()).then(|| Self {
            silo: config.recovery_silo.silo_name.clone(),
            username: config.recovery_silo.user_name.clone(),
            password: "oxide".into(),
        })
    }
}

fn api_candidates(network: &Network) -> Vec<Ipv4Addr> {
    let Ok(first) = network.service_pool_first.parse::<Ipv4Addr>() else {
        return Vec::new();
    };
    let last = network.service_pool_last.parse::<Ipv4Addr>().unwrap_or(first);
    let lo = u32::from(first);
    let hi = u32::from(last).max(lo).min(lo.saturating_add(31));
    let dns: Vec<&str> =
        network.external_dns_ips.iter().map(String::as_str).collect();
    let (rest, dns_members): (Vec<Ipv4Addr>, Vec<Ipv4Addr>) = (lo..=hi)
        .map(Ipv4Addr::from)
        .partition(|addr| !dns.contains(&addr.to_string().as_str()));
    rest.into_iter().chain(dns_members).collect()
}

pub(crate) fn rack_endpoints(
    config: &VoxelConfig,
    rack: usize,
) -> anyhow::Result<Vec<Url>> {
    ensure!(rack < config.topology.racks(), "unknown rack {}", rack + 1);
    let network = config.network.for_rack(rack);
    let candidates = api_candidates(&network);
    ["http", "https"]
        .into_iter()
        .flat_map(|scheme| {
            candidates.iter().map(move |address| (scheme, address))
        })
        .map(|(scheme, address)| {
            Url::parse(&format!("{scheme}://{address}"))
                .context("build Nexus endpoint candidate")
        })
        .collect()
}

type Point<T> = (Option<DateTime<Utc>>, DateTime<Utc>, Option<T>);
pub(crate) type IntegerPoint = Point<i64>;

/// Reads string fields and the single value column of a generated OxQL
/// timeseries.
pub(crate) trait TimeseriesExt {
    fn field(&self, name: &str) -> Option<&str>;
    fn integer_points(&self) -> anyhow::Result<Vec<IntegerPoint>>;
    fn numeric_points(&self) -> anyhow::Result<Vec<Point<f64>>>;
}

impl TimeseriesExt for Timeseries {
    fn field(&self, name: &str) -> Option<&str> {
        match self.fields.get(name)? {
            FieldValue::String(value) => Some(value),
            _ => None,
        }
    }

    fn integer_points(&self) -> anyhow::Result<Vec<IntegerPoint>> {
        let ValueArray::Integer(values) = value_column(self)? else {
            return Err(anyhow!("expected integer OxQL values"));
        };
        points(self, values.clone())
    }

    fn numeric_points(&self) -> anyhow::Result<Vec<Point<f64>>> {
        let values = match value_column(self)? {
            ValueArray::Integer(values) => values
                .iter()
                .map(|value| value.map(|value| value as f64))
                .collect(),
            ValueArray::Double(values) => values.clone(),
            _ => return Err(anyhow!("expected numeric OxQL values")),
        };
        points(self, values)
    }
}

fn value_column(series: &Timeseries) -> anyhow::Result<&ValueArray> {
    ensure!(series.points.values.len() == 1, "expected one OxQL value column");
    Ok(&series.points.values[0].values)
}

/// Pairs each value with its timestamp and, when the series has intervals,
/// its start time.
fn points<T>(
    series: &Timeseries,
    values: Vec<Option<T>>,
) -> anyhow::Result<Vec<Point<T>>> {
    let points = &series.points;
    ensure!(
        values.len() == points.timestamps.len(),
        "OxQL value and timestamp lengths differ"
    );
    if let Some(starts) = &points.start_times {
        ensure!(starts.len() == values.len(), "OxQL interval lengths differ");
    }
    Ok(values
        .into_iter()
        .enumerate()
        .map(|(index, value)| {
            (
                points.start_times.as_ref().map(|starts| starts[index]),
                points.timestamps[index],
                value,
            )
        })
        .collect())
}

/// Bounds the TCP connect separately from the request timeout. Most
/// service-pool addresses are not Nexus and silently drop connections, and a
/// probe round can only conclude once its slowest probe has.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
// Kept short: the lookup runs inside the 10 s Nexus readiness budget.
const DNS_LOOKUP_TIMEOUT: Duration = Duration::from_secs(2);

/// Where a rack publishes its Nexus addresses once rack setup has completed.
#[derive(Clone, Debug, Eq, PartialEq)]
struct NexusDns {
    servers: Vec<String>,
    name: String,
}

pub(crate) struct NexusClient {
    http: reqwest::Client,
    endpoints: Vec<Url>,
    dns: Option<NexusDns>,
    endpoint: Mutex<Option<Url>>,
    authenticated: Mutex<Option<Url>>,
    operation: Mutex<()>,
    login: RecoveryLogin,
    timeout: Duration,
}

type NexusError = oxide_client::Error<oxide_client::types::Error>;

/// Summarizes a client error as the status and message Nexus returned, if
/// any.
fn describe(error: &NexusError) -> String {
    let oxide_client::Error::ErrorResponse(response) = error else {
        return match error.status() {
            Some(status) => format!("returned {status}"),
            None => format!("failed: {error}"),
        };
    };
    let described =
        format!("returned {}: {}", response.status(), response.message);
    // The generated client sends the Nexus API version of the omicron Voxel
    // is pinned to, which a control plane built from older omicron rejects.
    if response.message.contains("does not support this API version") {
        format!(
            "{described} (monitoring needs a control-plane image built at or \
             after omicron {}, which Voxel is pinned to)",
            env!("RACK_INIT_CONFIG_OMICRON_REV")
        )
    } else {
        described
    }
}

/// The A records in `dig +short` output; CNAME targets and noise are skipped.
fn parse_dig_addresses(output: &str) -> Vec<Ipv4Addr> {
    output.lines().filter_map(|line| line.trim().parse().ok()).collect()
}

/// Narrows the configured candidates to the addresses DNS names, keeping
/// their scheme order. Addresses outside the configured pool are still
/// trusted, since DNS is authoritative once it answers.
fn dns_endpoints(configured: &[Url], resolved: &[Ipv4Addr]) -> Vec<Url> {
    let mut schemes = Vec::<&str>::new();
    for endpoint in configured {
        if !schemes.contains(&endpoint.scheme()) {
            schemes.push(endpoint.scheme());
        }
    }
    schemes
        .into_iter()
        .flat_map(|scheme| {
            resolved.iter().filter_map(move |address| {
                Url::parse(&format!("{scheme}://{address}/")).ok()
            })
        })
        .collect()
}

impl NexusClient {
    pub(crate) fn new(
        endpoints: Vec<Url>,
        login: RecoveryLogin,
        timeout: Duration,
    ) -> anyhow::Result<Self> {
        ensure!(!endpoints.is_empty(), "Nexus endpoint list is empty");
        ensure!(!timeout.is_zero(), "Nexus request timeout must be nonzero");
        let http = reqwest::Client::builder()
            .cookie_store(true)
            // Voxel's wicket setup intentionally installs a self-signed
            // development certificate. This client is private to virtual racks.
            .danger_accept_invalid_certs(true)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT.min(timeout))
            .build()
            .context("build Voxel Nexus client")?;
        Ok(Self {
            http,
            endpoints,
            dns: None,
            endpoint: Mutex::new(None),
            authenticated: Mutex::new(None),
            operation: Mutex::new(()),
            login,
            timeout,
        })
    }

    /// Prefers the Nexus addresses `servers` publish for the recovery silo in
    /// `zone` over the configured candidates whenever they answer.
    pub(crate) fn with_dns(mut self, servers: Vec<String>, zone: &str) -> Self {
        self.dns = Some(NexusDns {
            servers,
            name: format!("{}.sys.{zone}", self.login.silo),
        });
        self
    }

    /// Nexus addresses from the rack's external DNS, or none when DNS is not
    /// serving yet (before rack setup) or `dig` is unavailable.
    async fn resolve(&self, cancel: &CancellationToken) -> Vec<Ipv4Addr> {
        let Some(dns) = &self.dns else {
            return Vec::new();
        };
        for server in &dns.servers {
            let mut command = tokio::process::Command::new("dig");
            command
                .args(["+short", "+time=1", "+tries=1"])
                .arg(format!("@{server}"))
                .args([dns.name.as_str(), "A"])
                .kill_on_drop(true);
            let output = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Vec::new(),
                output = tokio::time::timeout(DNS_LOOKUP_TIMEOUT, command.output()) => output,
            };
            if let Ok(Ok(output)) = output
                && output.status.success()
            {
                let addresses = parse_dig_addresses(&String::from_utf8_lossy(
                    &output.stdout,
                ));
                if !addresses.is_empty() {
                    return addresses;
                }
            }
        }
        Vec::new()
    }

    /// The generated client for `endpoint`, sharing the session cookies of
    /// `http`.
    fn api(&self, endpoint: &Url) -> oxide_client::Client {
        oxide_client::Client::new_with_client(
            endpoint.as_str().trim_end_matches('/'),
            self.http.clone(),
        )
    }

    /// Runs one Nexus call, including reading the response body, within the
    /// request timeout. The outer error is cancellation or timeout. The inner
    /// result is the Nexus response.
    async fn call<T>(
        &self,
        call: impl Future<Output = Result<ResponseValue<T>, NexusError>>,
        cancel: &CancellationToken,
    ) -> anyhow::Result<Result<T, NexusError>> {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(anyhow!("cancelled")),
            result = tokio::time::timeout(self.timeout, call) => {
                let result = result.map_err(|_| {
                    anyhow!("Nexus request timed out after {:?}", self.timeout)
                })?;
                Ok(result.map(ResponseValue::into_inner))
            }
        }
    }

    async fn endpoint(
        &self,
        cancel: &CancellationToken,
        excluded: &[Url],
    ) -> anyhow::Result<Url> {
        if let Some(endpoint) = self.endpoint.lock().await.clone()
            && !excluded.contains(&endpoint)
        {
            return Ok(endpoint);
        }
        let resolved = self.resolve(cancel).await;
        let candidates = if resolved.is_empty() {
            self.endpoints.clone()
        } else {
            dns_endpoints(&self.endpoints, &resolved)
        };
        let mut probes = FuturesUnordered::new();
        for endpoint in
            candidates.iter().filter(|endpoint| !excluded.contains(endpoint))
        {
            let endpoint = endpoint.clone();
            probes.push(async move {
                let api = self.api(&endpoint);
                let response = self.call(api.ping().send(), cancel).await;
                (endpoint, response)
            });
        }
        let mut errors = Vec::new();
        while let Some((endpoint, response)) = probes.next().await {
            match response {
                Ok(Ok(_)) => {
                    *self.endpoint.lock().await = Some(endpoint.clone());
                    return Ok(endpoint);
                }
                Ok(Err(error)) => {
                    errors.push(format!("{endpoint} {}", describe(&error)))
                }
                Err(error) if cancel.is_cancelled() => return Err(error),
                Err(error) => errors.push(format!("{endpoint}: {error:#}")),
            }
        }
        Err(anyhow!("Nexus is unavailable: {}", errors.join("; ")))
    }

    async fn login(
        &self,
        endpoint: &Url,
        cancel: &CancellationToken,
    ) -> anyhow::Result<()> {
        let silo = Name::try_from(self.login.silo.as_str())
            .context("recovery silo name")?;
        let credentials = UsernamePasswordCredentials {
            username: self
                .login
                .username
                .parse()
                .context("recovery user name")?,
            password: self
                .login
                .password
                .parse()
                .context("recovery password")?,
        };
        let api = self.api(endpoint);
        self.call(
            api.login_local().silo_name(silo).body(credentials).send(),
            cancel,
        )
        .await?
        .map_err(|error| anyhow!("Nexus login {}", describe(&error)))?;
        *self.authenticated.lock().await = Some(endpoint.clone());
        Ok(())
    }

    async fn query_once(
        &self,
        endpoint: &Url,
        query: &str,
        cancel: &CancellationToken,
    ) -> anyhow::Result<Result<OxqlQueryResult, NexusError>> {
        let api = self.api(endpoint);
        let body = TimeseriesQuery { query: query.into() };
        self.call(api.system_timeseries_query().body(body).send(), cancel).await
    }

    async fn query_from(
        &self,
        endpoint: &Url,
        query: &str,
        cancel: &CancellationToken,
    ) -> anyhow::Result<OxqlQueryResult> {
        let mut response = self.query_once(endpoint, query, cancel).await?;
        if response.as_ref().is_err_and(|error| {
            error.status() == Some(StatusCode::UNAUTHORIZED)
        }) {
            let _operation = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(anyhow!("cancelled")),
                operation = self.operation.lock() => operation,
            };
            *self.authenticated.lock().await = None;
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(anyhow!("cancelled")),
                _ = tokio::time::sleep(Duration::from_millis(100)) => {}
            }
            self.login(endpoint, cancel).await?;
            response = self.query_once(endpoint, query, cancel).await?;
        }
        response.map_err(|error| {
            anyhow!("Nexus timeseries query {}", describe(&error))
        })
    }

    pub(crate) async fn ready(
        &self,
        cancel: &CancellationToken,
    ) -> anyhow::Result<()> {
        let _operation = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(anyhow!("cancelled")),
            operation = self.operation.lock() => operation,
        };
        let endpoint = self.endpoint(cancel, &[]).await?;
        if self.authenticated.lock().await.as_ref() != Some(&endpoint) {
            self.login(&endpoint, cancel).await?;
        }
        Ok(())
    }

    pub(crate) async fn query(
        &self,
        query: &str,
        cancel: &CancellationToken,
    ) -> anyhow::Result<OxqlQueryResult> {
        let mut excluded = Vec::new();
        let mut first_error = None;
        for _ in 0..self.endpoints.len() {
            let endpoint = {
                let _operation = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(anyhow!("cancelled")),
                    operation = self.operation.lock() => operation,
                };
                let endpoint = self.endpoint(cancel, &excluded).await?;
                if self.authenticated.lock().await.as_ref() != Some(&endpoint) {
                    self.login(&endpoint, cancel).await?;
                }
                endpoint
            };
            match self.query_from(&endpoint, query, cancel).await {
                Ok(result) => return Ok(result),
                Err(error) if cancel.is_cancelled() => return Err(error),
                Err(error) => {
                    first_error.get_or_insert(error);
                    let _operation = self.operation.lock().await;
                    if self.endpoint.lock().await.as_ref() == Some(&endpoint) {
                        *self.endpoint.lock().await = None;
                    }
                    if self.authenticated.lock().await.as_ref()
                        == Some(&endpoint)
                    {
                        *self.authenticated.lock().await = None;
                    }
                    excluded.push(endpoint);
                }
            }
        }
        Err(first_error.expect("query attempt failed without an error"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::test_support::recovery;
    use httptest::matchers::{contains, eq, json_decoded, request};
    use httptest::responders::{
        Responder, delay_and_then, json_encoded, status_code,
    };
    use httptest::{Expectation, ExpectationBuilder, Server, all_of, cycle};
    use serde_json::json;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;
    use voxel_config::VoxelConfig;

    const PING_PATH: &str = "/v1/ping";
    const LOGIN: &str = "/v1/login/recovery/local";
    const QUERY: &str = "/v1/system/timeseries/query";
    // Nexus answers every error with a JSON body naming the request.
    const UNAUTHORIZED: &str = r#"{"request_id":"test","error_code":"Unauthorized","message":"credentials missing or invalid"}"#;
    const UNSUPPORTED_VERSION: &str = r#"{"request_id":"test","message":"server does not support this API version: 2026081400.0.0"}"#;

    fn endpoint(server: &Server) -> Url {
        Url::parse(&server.url_str("/")).unwrap()
    }

    fn pong() -> impl Responder {
        json_encoded(json!({"status": "ok"}))
    }

    fn ping() -> Expectation {
        Expectation::matching(request::method_path("GET", PING_PATH))
            .respond_with(pong())
    }

    fn login() -> ExpectationBuilder {
        Expectation::matching(request::method_path("POST", LOGIN))
    }

    fn query() -> ExpectationBuilder {
        Expectation::matching(request::method_path("POST", QUERY))
    }

    fn query_with_cookie(cookie: &'static str) -> ExpectationBuilder {
        Expectation::matching(all_of![
            request::method_path("POST", QUERY),
            request::headers(contains(("cookie", cookie))),
        ])
    }

    /// A successful login, which sets the session cookie Nexus uses.
    fn session(name: &str) -> impl Responder + use<> {
        status_code(204).append_header(
            "Set-Cookie",
            format!("session={name}; Path=/; HttpOnly"),
        )
    }

    /// An answer that arrives well after any client timeout in these tests.
    fn stall() -> impl Responder {
        delay_and_then(Duration::from_millis(200), json_encoded(empty_result()))
    }

    fn empty_result() -> serde_json::Value {
        json!({"tables": [], "query_summaries": null})
    }

    #[test]
    fn rack_endpoints_follow_service_pool_probe_order() {
        let config = VoxelConfig::default();

        let endpoints = rack_endpoints(&config, 0).unwrap();

        assert_eq!(endpoints.len(), 20);
        assert_eq!(endpoints[0].as_str(), "http://198.51.100.22/");
        assert_eq!(endpoints[8].as_str(), "http://198.51.100.20/");
        assert_eq!(endpoints[10].as_str(), "https://198.51.100.22/");
    }

    #[test]
    fn dns_answers_narrow_the_candidates_and_keep_scheme_order() {
        let configured = rack_endpoints(&VoxelConfig::default(), 0).unwrap();
        let resolved = parse_dig_addresses(
            "198.51.100.22\nnexus.alias.example.\n198.51.100.24\n\n",
        );
        let narrowed = dns_endpoints(&configured, &resolved)
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>();
        assert_eq!(
            narrowed,
            [
                "http://198.51.100.22/",
                "http://198.51.100.24/",
                "https://198.51.100.22/",
                "https://198.51.100.24/",
            ]
        );
    }

    #[tokio::test]
    async fn unanswering_addresses_fail_at_the_connect_timeout() {
        // TEST-NET-1 is never routed, so the connect either fails at once or
        // hangs; either way the long request timeout must not apply.
        let client = NexusClient::new(
            vec![Url::parse("http://192.0.2.1/").unwrap()],
            recovery(),
            Duration::from_secs(60),
        )
        .unwrap()
        .with_dns(vec![], "rack1.oxide.test");
        let started = std::time::Instant::now();
        let error = client.ready(&CancellationToken::new()).await.unwrap_err();
        assert!(
            started.elapsed() < CONNECT_TIMEOUT + Duration::from_secs(2),
            "{:?}: {error:#}",
            started.elapsed()
        );
        assert!(format!("{error:#}").contains("Nexus is unavailable"));
    }

    #[test]
    fn automatic_login_requires_the_default_recovery_identity() {
        let mut config = VoxelConfig::default();
        assert_eq!(RecoveryLogin::from_config(&config), Some(recovery()));

        config.recovery_silo.user_name = "operator".into();
        assert_eq!(RecoveryLogin::from_config(&config), None);
    }

    /// A control-plane image built from omicron older than Voxel's pin rejects
    /// the client's API version. The error names the image as the cause, so
    /// the operator sees more than a bare 400.
    #[tokio::test]
    async fn an_older_nexus_is_reported_as_an_image_mismatch() {
        let server = Server::run();
        server.expect(
            Expectation::matching(request::method_path("GET", PING_PATH))
                .respond_with(
                    status_code(400)
                        .append_header("Content-Type", "application/json")
                        .body(UNSUPPORTED_VERSION),
                ),
        );
        let client = NexusClient::new(
            vec![endpoint(&server)],
            recovery(),
            Duration::from_secs(2),
        )
        .unwrap();

        let error = client.ready(&CancellationToken::new()).await.unwrap_err();

        let error = format!("{error:#}");
        assert!(
            error.contains("does not support this API version")
                && error.contains(&format!(
                    "built at or after omicron {}",
                    env!("RACK_INIT_CONFIG_OMICRON_REV")
                )),
            "{error}"
        );
    }

    #[tokio::test]
    async fn readiness_authenticates_once_without_querying_timeseries() {
        let server = Server::run();
        server.expect(ping());
        server.expect(login().respond_with(session("ready")));
        let client = NexusClient::new(
            vec![endpoint(&server)],
            recovery(),
            Duration::from_secs(2),
        )
        .unwrap();
        let cancel = CancellationToken::new();

        client.ready(&cancel).await.unwrap();
        client.ready(&cancel).await.unwrap();
    }

    #[tokio::test]
    async fn query_logs_in_and_reauthenticates_once_after_unauthorized() {
        let server = Server::run();
        server.expect(ping());
        server.expect(
            Expectation::matching(all_of![
                request::method_path("POST", LOGIN),
                request::body(json_decoded(eq(json!({
                    "username": "recovery",
                    "password": "oxide",
                })))),
            ])
            .times(2)
            .respond_with(cycle![session("first"), session("second")]),
        );
        server.expect(
            query_with_cookie("session=first").respond_with(
                status_code(401)
                    .insert_header("Content-Type", "application/json")
                    .body(UNAUTHORIZED),
            ),
        );
        server.expect(
            Expectation::matching(all_of![
                request::method_path("POST", QUERY),
                request::headers(contains(("cookie", "session=second"))),
                request::body(json_decoded(eq(json!({
                    "query": "get sled_data_link:bytes_sent",
                })))),
            ])
            .respond_with(json_encoded(empty_result())),
        );
        let client = NexusClient::new(
            vec![endpoint(&server)],
            recovery(),
            Duration::from_secs(2),
        )
        .unwrap();

        let result = client
            .query("get sled_data_link:bytes_sent", &CancellationToken::new())
            .await
            .unwrap();

        assert!(result.tables.is_empty());
    }

    /// Fails over past every endpoint that stalls or refuses and answers from
    /// the last one, whether the client found the first by probing or had
    /// already settled on it.
    #[tokio::test]
    async fn query_succeeds_on_the_last_endpoint_after_earlier_ones_stall_or_fail()
     {
        type Servers = fn() -> Vec<Server>;
        // (case, servers in candidate order, whether the client starts
        // settled on the first)
        let cases: [(&str, Servers, bool); 2] = [
            (
                "first chosen by probing",
                || {
                    let stalled = Server::run();
                    stalled.expect(ping());
                    stalled.expect(login().respond_with(session("stalled")));
                    stalled.expect(query().respond_with(stall()));
                    let healthy = Server::run();
                    // The healthy endpoint answers its first ping late, so
                    // the client settles on the stalled one first.
                    healthy.expect(
                        Expectation::matching(request::method_path(
                            "GET", PING_PATH,
                        ))
                        .times(2)
                        .respond_with(cycle![
                            delay_and_then(Duration::from_millis(20), pong()),
                            pong(),
                        ]),
                    );
                    healthy.expect(login().respond_with(session("healthy")));
                    healthy.expect(
                        query().respond_with(json_encoded(empty_result())),
                    );
                    vec![stalled, healthy]
                },
                false,
            ),
            (
                "first already settled",
                || {
                    let first = Server::run();
                    first.expect(login().respond_with(session("first")));
                    first.expect(query().respond_with(stall()));
                    let second = Server::run();
                    second.expect(ping());
                    second.expect(login().respond_with(session("second")));
                    second.expect(query().respond_with(stall()));
                    let third = Server::run();
                    third.expect(
                        Expectation::matching(request::method_path(
                            "GET", PING_PATH,
                        ))
                        .times(2)
                        .respond_with(cycle![status_code(503), pong()]),
                    );
                    third.expect(login().respond_with(session("third")));
                    third.expect(
                        query().respond_with(json_encoded(empty_result())),
                    );
                    vec![first, second, third]
                },
                true,
            ),
        ];
        for (case, servers, settled) in cases {
            // Each server checks its expectations when dropped at the end of
            // the case.
            let servers = servers();
            let client = NexusClient::new(
                servers.iter().map(endpoint).collect(),
                recovery(),
                Duration::from_millis(50),
            )
            .unwrap();
            if settled {
                *client.endpoint.lock().await = Some(endpoint(&servers[0]));
            }

            let result = client
                .query(
                    "get sled_data_link:bytes_sent",
                    &CancellationToken::new(),
                )
                .await;

            assert!(
                result.expect(case).tables.is_empty(),
                "{case}: the last endpoint's answer"
            );
        }
    }

    #[tokio::test]
    async fn query_fails_with_the_first_error_once_every_endpoint_fails() {
        let servers = [Server::run(), Server::run()];
        for (server, name) in servers.iter().zip(["first", "second"]) {
            // Readiness may probe every endpoint, so pings are not counted.
            server.expect(
                Expectation::matching(request::method_path("GET", PING_PATH))
                    .times(1..)
                    .respond_with(pong()),
            );
            server.expect(login().respond_with(session(name)));
            server.expect(query().respond_with(status_code(500)));
        }
        let client = NexusClient::new(
            servers.iter().map(endpoint).collect(),
            recovery(),
            Duration::from_secs(2),
        )
        .unwrap();

        let error = client
            .query("get sled_data_link:bytes_sent", &CancellationToken::new())
            .await
            .unwrap_err();

        assert!(
            format!("{error:#}").contains("Nexus timeseries query"),
            "{error:#}"
        );
    }
}
