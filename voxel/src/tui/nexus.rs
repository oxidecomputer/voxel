// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Nexus access for monitoring. Finds the rack's Nexus by name through the
//! rack's external DNS, logs into the recovery silo, and runs OxQL timeseries
//! queries. Requests, responses and name resolution use omicron's
//! `oxide-client`.

use std::{future::Future, net::SocketAddr, sync::Arc, time::Duration};

use anyhow::{Context, anyhow, ensure};
use chrono::{DateTime, Utc};
pub(crate) use oxide_client::types::{MetricType, OxqlQueryResult};
use oxide_client::{
    ClientLoginExt, ClientSystemMetricsExt, ClientSystemStatusExt,
    CustomDnsResolver, ResponseValue,
    types::{
        FieldValue, Name, Timeseries, TimeseriesQuery,
        UsernamePasswordCredentials, ValueArray,
    },
};
use reqwest::{StatusCode, Url};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use voxel_config::{RecoverySiloCfg, VoxelConfig};

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

/// Bounds the TCP connect separately from the request timeout. The resolver
/// can return several Nexus addresses, and the connector splits this budget
/// across them, skipping any that refuse or do not answer in time.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// The name a rack's external DNS publishes for the recovery silo's Nexus
/// once rack setup has completed, as omicron's end-to-end tests address it.
pub(crate) fn nexus_name(config: &VoxelConfig, rack: usize) -> String {
    format!(
        "{}.sys.{}",
        config.recovery_silo.silo_name,
        config.network.for_rack(rack).dns_zone
    )
}

pub(crate) struct NexusClient {
    http: reqwest::Client,
    /// Base URLs to try in order, until one answers a ping.
    endpoints: Vec<Url>,
    /// The endpoint that answered, and whether this client is logged in to it.
    session: Mutex<Option<(Url, bool)>>,
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

impl NexusClient {
    /// The rack's Nexus by name, resolved through the rack's external DNS.
    /// Nexus serves HTTPS when rack setup installed a certificate and plain
    /// HTTP otherwise, so both are candidates.
    pub(crate) fn for_rack(
        config: &VoxelConfig,
        rack: usize,
        login: RecoveryLogin,
        timeout: Duration,
    ) -> anyhow::Result<Self> {
        let network = config.network.for_rack(rack);
        let dns = network
            .external_dns_ips
            .first()
            .context("rack has no external DNS address")?;
        let dns = SocketAddr::new(
            dns.parse().with_context(|| format!("external DNS {dns}"))?,
            53,
        );
        let name = nexus_name(config, rack);
        let endpoints = ["http", "https"]
            .into_iter()
            .map(|scheme| Url::parse(&format!("{scheme}://{name}")))
            .collect::<Result<_, _>>()
            .context("build Nexus endpoint")?;
        Self::new(
            endpoints,
            Some(Arc::new(CustomDnsResolver::new(dns)?)),
            login,
            timeout,
        )
    }

    /// A client for `endpoints`, tried in order, resolving their names
    /// through `resolver` when one is given.
    pub(crate) fn new(
        endpoints: Vec<Url>,
        resolver: Option<Arc<CustomDnsResolver>>,
        login: RecoveryLogin,
        timeout: Duration,
    ) -> anyhow::Result<Self> {
        ensure!(!endpoints.is_empty(), "Nexus endpoint list is empty");
        ensure!(!timeout.is_zero(), "Nexus request timeout must be nonzero");
        let mut http = reqwest::Client::builder()
            .cookie_store(true)
            // Voxel's wicket setup intentionally installs a self-signed
            // development certificate. This client is private to virtual racks.
            .danger_accept_invalid_certs(true)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT.min(timeout));
        if let Some(resolver) = resolver {
            http = http.dns_resolver(resolver);
        }
        Ok(Self {
            http: http.build().context("build Voxel Nexus client")?,
            endpoints,
            session: Mutex::new(None),
            login,
            timeout,
        })
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

    async fn log_in(
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
        self.call(
            self.api(endpoint)
                .login_local()
                .silo_name(silo)
                .body(credentials)
                .send(),
            cancel,
        )
        .await?
        .map(|_| ())
        .map_err(|error| anyhow!("Nexus login {}", describe(&error)))
    }

    /// The endpoint to use, logged in: the one already in use, or the first
    /// candidate that answers a ping.
    async fn session(&self, cancel: &CancellationToken) -> anyhow::Result<Url> {
        let mut session = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(anyhow!("cancelled")),
            session = self.session.lock() => session,
        };
        if let Some((endpoint, true)) = session.as_ref() {
            return Ok(endpoint.clone());
        }
        let endpoint = match session.as_ref() {
            Some((endpoint, _)) => endpoint.clone(),
            None => {
                let mut errors = Vec::new();
                let mut found = None;
                for endpoint in &self.endpoints {
                    match self
                        .call(self.api(endpoint).ping().send(), cancel)
                        .await
                    {
                        Ok(Ok(_)) => {
                            found = Some(endpoint.clone());
                            break;
                        }
                        Ok(Err(error)) => errors
                            .push(format!("{endpoint} {}", describe(&error))),
                        Err(error) if cancel.is_cancelled() => {
                            return Err(error);
                        }
                        Err(error) => {
                            errors.push(format!("{endpoint}: {error:#}"))
                        }
                    }
                }
                found.ok_or_else(|| {
                    anyhow!("Nexus is unavailable: {}", errors.join("; "))
                })?
            }
        };
        *session = Some((endpoint.clone(), false));
        self.log_in(&endpoint, cancel).await?;
        *session = Some((endpoint.clone(), true));
        Ok(endpoint)
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

    pub(crate) async fn ready(
        &self,
        cancel: &CancellationToken,
    ) -> anyhow::Result<()> {
        self.session(cancel).await.map(|_| ())
    }

    /// Runs an OxQL query, logging in again once if the session has expired
    /// (Nexus ends sessions after 8 hours idle and 24 hours in all). Any other
    /// failure forgets the endpoint, so the next call looks for Nexus anew.
    pub(crate) async fn query(
        &self,
        query: &str,
        cancel: &CancellationToken,
    ) -> anyhow::Result<OxqlQueryResult> {
        let endpoint = self.session(cancel).await?;
        let mut response = self.query_once(&endpoint, query, cancel).await;
        if matches!(&response, Ok(Err(error)) if error.status() == Some(StatusCode::UNAUTHORIZED))
        {
            if let Some(session) = self.session.lock().await.as_mut() {
                session.1 = false;
            }
            let endpoint = self.session(cancel).await?;
            response = self.query_once(&endpoint, query, cancel).await;
        }
        let result = response.and_then(|response| {
            response.map_err(|error| {
                anyhow!("Nexus timeseries query {}", describe(&error))
            })
        });
        if result.is_err() && !cancel.is_cancelled() {
            *self.session.lock().await = None;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::test_support::recovery;
    use httptest::matchers::{contains, eq, json_decoded, request};
    use httptest::responders::{Responder, json_encoded, status_code};
    use httptest::{Expectation, ExpectationBuilder, Server, all_of, cycle};
    use serde_json::json;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;
    use voxel_config::VoxelConfig;

    const LOGIN: &str = "/v1/login/recovery/local";
    const QUERY: &str = "/v1/system/timeseries/query";

    fn endpoint(server: &Server) -> Url {
        Url::parse(&server.url_str("/")).unwrap()
    }

    fn ping() -> ExpectationBuilder {
        Expectation::matching(request::method_path("GET", "/v1/ping"))
    }

    fn pong() -> impl Responder {
        json_encoded(json!({"status": "ok"}))
    }

    fn login() -> ExpectationBuilder {
        Expectation::matching(request::method_path("POST", LOGIN))
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

    /// Nexus answers every error with a JSON body naming the request.
    fn nexus_error(status: u16, code: &str) -> impl Responder + use<> {
        status_code(status)
            .insert_header("Content-Type", "application/json")
            .body(
            json!({"request_id": "test", "error_code": code, "message": code})
                .to_string(),
        )
    }

    fn empty_result() -> impl Responder {
        json_encoded(json!({"tables": [], "query_summaries": null}))
    }

    fn client(endpoints: Vec<Url>) -> NexusClient {
        NexusClient::new(endpoints, None, recovery(), Duration::from_secs(2))
            .unwrap()
    }

    /// Each step runs its own httptest servers in its own block: a server
    /// verifies its expectations when dropped, so an unmet expectation is
    /// reported at the step that set it.
    #[tokio::test]
    async fn nexus_client_logs_in_recovers_and_fails_fast() {
        const BYTES_SENT: &str = "get sled_data_link:bytes_sent";
        let cancel = CancellationToken::new();

        // Automatic login requires the default recovery identity.
        let mut config = VoxelConfig::default();
        assert_eq!(RecoveryLogin::from_config(&config), Some(recovery()));
        config.recovery_silo.user_name = "operator".into();
        assert_eq!(RecoveryLogin::from_config(&config), None);

        // Readiness authenticates once, without querying timeseries.
        {
            let server = Server::run();
            server.expect(ping().respond_with(pong()));
            server.expect(login().respond_with(session("ready")));
            let client = client(vec![endpoint(&server)]);
            client.ready(&cancel).await.unwrap();
            client.ready(&cancel).await.unwrap();
        }

        // A query logs in, and logs in again once after Unauthorized.
        {
            let server = Server::run();
            server.expect(ping().respond_with(pong()));
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
                query_with_cookie("session=first")
                    .respond_with(nexus_error(401, "Unauthorized")),
            );
            server.expect(
                Expectation::matching(all_of![
                    request::method_path("POST", QUERY),
                    request::headers(contains(("cookie", "session=second"))),
                    request::body(json_decoded(eq(json!({
                        "query": BYTES_SENT,
                    })))),
                ])
                .respond_with(empty_result()),
            );
            client(vec![endpoint(&server)])
                .query(BYTES_SENT, &cancel)
                .await
                .unwrap();
        }

        // Nexus listens on only one of HTTP and HTTPS, so the client settles
        // on the first candidate that answers.
        {
            let other_scheme = Server::run();
            other_scheme.expect(ping().respond_with(status_code(503)));
            let nexus = Server::run();
            nexus.expect(ping().respond_with(pong()));
            nexus.expect(login().respond_with(session("nexus")));
            nexus.expect(
                Expectation::matching(request::method_path("POST", QUERY))
                    .respond_with(empty_result()),
            );
            client(vec![endpoint(&other_scheme), endpoint(&nexus)])
                .query(BYTES_SENT, &cancel)
                .await
                .unwrap();
        }

        // A failed query reports what Nexus answered, and the next query finds
        // and logs in to Nexus again rather than reusing a session that failed.
        {
            let server = Server::run();
            server.expect(ping().times(2).respond_with(pong()));
            server.expect(
                login()
                    .times(2)
                    .respond_with(cycle![session("first"), session("second")]),
            );
            server.expect(
                query_with_cookie("session=first")
                    .respond_with(nexus_error(500, "Internal")),
            );
            server.expect(
                query_with_cookie("session=second")
                    .respond_with(empty_result()),
            );
            let client = client(vec![endpoint(&server)]);
            let error = client.query(BYTES_SENT, &cancel).await.unwrap_err();
            assert!(
                format!("{error:#}")
                    .contains("Nexus timeseries query returned 500"),
                "{error:#}"
            );
            client.query(BYTES_SENT, &cancel).await.unwrap();
        }

        // Unanswering addresses fail at the connect timeout. TEST-NET-1 is
        // never routed, so the connect either fails at once or hangs. Neither
        // case may wait for the long request timeout.
        let client = NexusClient::new(
            vec![Url::parse("http://192.0.2.1/").unwrap()],
            None,
            recovery(),
            Duration::from_secs(60),
        )
        .unwrap();
        let started = std::time::Instant::now();
        let error = client.ready(&cancel).await.unwrap_err();
        assert!(
            started.elapsed() < CONNECT_TIMEOUT + Duration::from_secs(2),
            "failed after {:?}: {error:#}",
            started.elapsed()
        );
        assert!(
            format!("{error:#}").contains("Nexus is unavailable"),
            "{error:#}"
        );
    }

    /// A control-plane image built from omicron older than Voxel's pin rejects
    /// the client's API version. The error names the image as the cause, so
    /// the operator sees more than a bare 400.
    #[tokio::test]
    async fn an_older_nexus_is_reported_as_an_image_mismatch() {
        let server = Server::run();
        server.expect(ping().respond_with(
            status_code(400)
                .insert_header("Content-Type", "application/json")
                .body(
                    json!({
                        "request_id": "test",
                        "message": "server does not support this API version: 2026081400.0.0",
                    })
                    .to_string(),
                ),
        ));

        let error = client(vec![endpoint(&server)])
            .ready(&CancellationToken::new())
            .await
            .unwrap_err();

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
}
