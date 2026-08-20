use std::time::Duration;

use anyhow::{Context, anyhow, ensure};
use chrono::{DateTime, Utc};
use futures::{StreamExt, stream::FuturesUnordered};
use reqwest::{RequestBuilder, Response, StatusCode, Url};
use serde::{Deserialize, Serialize};
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

pub(crate) fn rack_endpoints(
    config: &VoxelConfig,
    rack: usize,
) -> anyhow::Result<Vec<Url>> {
    ensure!(rack < config.topology.racks(), "unknown rack {}", rack + 1);
    let network = config.network.for_rack(rack);
    let candidates = crate::commtest::api_candidates(&network);
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

#[derive(Debug, Deserialize)]
pub(crate) struct OxqlQueryResult {
    pub(crate) tables: Vec<OxqlTable>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OxqlTable {
    pub(crate) name: String,
    pub(crate) timeseries: Vec<OxqlTimeseries>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OxqlTimeseries {
    pub(crate) fields: std::collections::BTreeMap<String, FieldValue>,
    pub(crate) points: OxqlPoints,
}

#[derive(Debug, Deserialize)]
pub(crate) struct FieldValue {
    #[serde(rename = "type")]
    kind: String,
    value: serde_json::Value,
}

impl FieldValue {
    pub(crate) fn as_str(&self) -> Option<&str> {
        match self.kind.as_str() {
            "string" | "uuid" => self.value.as_str(),
            _ => None,
        }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct OxqlPoints {
    #[serde(default)]
    pub(crate) start_times: Option<Vec<DateTime<Utc>>>,
    pub(crate) timestamps: Vec<DateTime<Utc>>,
    pub(crate) values: Vec<OxqlValues>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OxqlValues {
    pub(crate) values: ValueArray,
    pub(crate) metric_type: MetricType,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MetricType {
    Gauge,
    Cumulative,
    Delta,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", content = "values", rename_all = "snake_case")]
pub(crate) enum ValueArray {
    Integer(Vec<Option<i64>>),
    Double(Vec<Option<f64>>),
}

pub(crate) type IntegerPoint =
    (Option<DateTime<Utc>>, DateTime<Utc>, Option<i64>);

impl OxqlTimeseries {
    pub(crate) fn field(&self, name: &str) -> Option<&str> {
        self.fields.get(name).and_then(FieldValue::as_str)
    }

    pub(crate) fn integer_points(&self) -> anyhow::Result<Vec<IntegerPoint>> {
        ensure!(
            self.points.values.len() == 1,
            "expected one OxQL value column"
        );
        let ValueArray::Integer(values) = &self.points.values[0].values else {
            return Err(anyhow!("expected integer OxQL values"));
        };
        ensure!(
            values.len() == self.points.timestamps.len(),
            "OxQL value and timestamp lengths differ"
        );
        if let Some(starts) = &self.points.start_times {
            ensure!(
                starts.len() == values.len(),
                "OxQL interval lengths differ"
            );
        }
        Ok(values
            .iter()
            .enumerate()
            .map(|(index, value)| {
                (
                    self.points
                        .start_times
                        .as_ref()
                        .map(|starts| starts[index]),
                    self.points.timestamps[index],
                    *value,
                )
            })
            .collect())
    }

    pub(crate) fn numeric_points(
        &self,
    ) -> anyhow::Result<Vec<(Option<DateTime<Utc>>, DateTime<Utc>, Option<f64>)>>
    {
        ensure!(
            self.points.values.len() == 1,
            "expected one OxQL value column"
        );
        let values = match &self.points.values[0].values {
            ValueArray::Integer(values) => values
                .iter()
                .map(|value| value.map(|value| value as f64))
                .collect(),
            ValueArray::Double(values) => values.clone(),
        };
        ensure!(
            values.len() == self.points.timestamps.len(),
            "OxQL value and timestamp lengths differ"
        );
        if let Some(starts) = &self.points.start_times {
            ensure!(
                starts.len() == values.len(),
                "OxQL interval lengths differ"
            );
        }
        Ok(values
            .into_iter()
            .enumerate()
            .map(|(index, value)| {
                (
                    self.points
                        .start_times
                        .as_ref()
                        .map(|starts| starts[index]),
                    self.points.timestamps[index],
                    value,
                )
            })
            .collect())
    }
}

#[derive(Serialize)]
struct LoginRequest<'a> {
    username: &'a str,
    password: &'a str,
}

#[derive(Serialize)]
struct QueryRequest<'a> {
    query: &'a str,
    include_summaries: bool,
}

pub(crate) struct NexusClient {
    http: reqwest::Client,
    endpoints: Vec<Url>,
    endpoint: Mutex<Option<Url>>,
    authenticated: Mutex<Option<Url>>,
    operation: Mutex<()>,
    login: RecoveryLogin,
    timeout: Duration,
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
            .build()
            .context("build Voxel Nexus client")?;
        Ok(Self {
            http,
            endpoints,
            endpoint: Mutex::new(None),
            authenticated: Mutex::new(None),
            operation: Mutex::new(()),
            login,
            timeout,
        })
    }

    async fn send(
        &self,
        request: RequestBuilder,
        cancel: &CancellationToken,
    ) -> anyhow::Result<Response> {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(anyhow!("cancelled")),
            result = tokio::time::timeout(self.timeout, request.send()) => {
                result
                    .map_err(|_| anyhow!("Nexus request timed out after {:?}", self.timeout))?
                    .context("send Nexus request")
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
        let mut probes = FuturesUnordered::new();
        for endpoint in self
            .endpoints
            .iter()
            .filter(|endpoint| !excluded.contains(endpoint))
        {
            let endpoint = endpoint.clone();
            probes.push(async move {
                let ping =
                    endpoint.join("v1/ping").context("build Nexus ping URL")?;
                Ok::<_, anyhow::Error>((
                    endpoint,
                    self.send(self.http.get(ping), cancel).await,
                ))
            });
        }
        let mut errors = Vec::new();
        while let Some(probe) = probes.next().await {
            let (endpoint, response) = probe?;
            match response {
                Ok(response) if response.status().is_success() => {
                    *self.endpoint.lock().await = Some(endpoint.clone());
                    return Ok(endpoint);
                }
                Ok(response) => errors.push(format!(
                    "{} returned {}",
                    endpoint,
                    response.status()
                )),
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
        let url = endpoint
            .join(&format!("v1/login/{}/local", self.login.silo))
            .context("build Nexus login URL")?;
        let response = self
            .send(
                self.http.post(url).json(&LoginRequest {
                    username: &self.login.username,
                    password: &self.login.password,
                }),
                cancel,
            )
            .await?;
        if !response.status().is_success() {
            return Err(anyhow!("Nexus login returned {}", response.status()));
        }
        *self.authenticated.lock().await = Some(endpoint.clone());
        Ok(())
    }

    async fn query_once(
        &self,
        endpoint: &Url,
        query: &str,
        cancel: &CancellationToken,
    ) -> anyhow::Result<Response> {
        let url = endpoint
            .join("v1/system/timeseries/query")
            .context("build Nexus timeseries URL")?;
        self.send(
            self.http
                .post(url)
                .json(&QueryRequest { query, include_summaries: false }),
            cancel,
        )
        .await
    }

    async fn query_from(
        &self,
        endpoint: &Url,
        query: &str,
        cancel: &CancellationToken,
    ) -> anyhow::Result<OxqlQueryResult> {
        let mut response = self.query_once(endpoint, query, cancel).await?;
        if response.status() == StatusCode::UNAUTHORIZED {
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
        if !response.status().is_success() {
            return Err(anyhow!(
                "Nexus timeseries query returned {}",
                response.status()
            ));
        }
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(anyhow!("cancelled")),
            result = tokio::time::timeout(self.timeout, response.json()) => {
                result
                    .map_err(|_| anyhow!("Nexus response timed out after {:?}", self.timeout))?
                    .context("decode Nexus timeseries response")
            }
        }
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
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_util::sync::CancellationToken;
    use voxel_config::VoxelConfig;

    struct TestResponse {
        status: &'static str,
        headers: &'static str,
        body: &'static str,
        delay: Duration,
    }

    async fn test_server(
        responses: Vec<TestResponse>,
    ) -> (Url, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let task = tokio::spawn(async move {
            let mut responses = VecDeque::from(responses);
            while let Some(response) = responses.pop_front() {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let mut buffer = [0; 4096];
                loop {
                    let count = stream.read(&mut buffer).await.unwrap();
                    if count == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&buffer[..count]);
                    let Some(headers_end) = bytes
                        .windows(4)
                        .position(|window| window == b"\r\n\r\n")
                    else {
                        continue;
                    };
                    let headers =
                        String::from_utf8_lossy(&bytes[..headers_end]);
                    let content_length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|value| value.trim().parse().ok())
                        })
                        .unwrap_or(0);
                    if bytes.len() >= headers_end + 4 + content_length {
                        break;
                    }
                }
                recorded
                    .lock()
                    .unwrap()
                    .push(String::from_utf8(bytes).unwrap());
                let reply = format!(
                    "HTTP/1.1 {}\r\nContent-Length: {}\r\nConnection: close\r\n{}\r\n",
                    response.status,
                    response.body.len(),
                    response.headers,
                );
                stream.write_all(reply.as_bytes()).await.unwrap();
                tokio::time::sleep(response.delay).await;
                stream.write_all(response.body.as_bytes()).await.unwrap();
            }
        });
        (Url::parse(&format!("http://{address}/")).unwrap(), requests, task)
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
    fn automatic_login_requires_the_default_recovery_identity() {
        let mut config = VoxelConfig::default();
        assert_eq!(
            RecoveryLogin::from_config(&config),
            Some(RecoveryLogin {
                silo: "recovery".into(),
                username: "recovery".into(),
                password: "oxide".into(),
            })
        );

        config.recovery_silo.user_name = "operator".into();
        assert_eq!(RecoveryLogin::from_config(&config), None);
    }

    #[test]
    fn oxql_fields_decode_every_api_field_type() {
        let result: OxqlQueryResult = serde_json::from_value(serde_json::json!({
            "tables": [{
                "name": "test:metric",
                "timeseries": [{
                    "fields": {
                        "string": {"type": "string", "value": "value"},
                        "i8": {"type": "i8", "value": -1},
                        "u8": {"type": "u8", "value": 1},
                        "i16": {"type": "i16", "value": -2},
                        "u16": {"type": "u16", "value": 2},
                        "i32": {"type": "i32", "value": -3},
                        "u32": {"type": "u32", "value": 3},
                        "i64": {"type": "i64", "value": -4},
                        "u64": {"type": "u64", "value": 4},
                        "ip_addr": {"type": "ip_addr", "value": "192.0.2.1"},
                        "uuid": {
                            "type": "uuid",
                            "value": "00000000-0000-0000-0000-000000000001"
                        },
                        "bool": {"type": "bool", "value": true}
                    },
                    "points": {
                        "timestamps": [],
                        "values": []
                    }
                }]
            }]
        }))
        .unwrap();
        let fields = &result.tables[0].timeseries[0].fields;

        assert_eq!(fields["string"].as_str(), Some("value"));
        assert_eq!(
            fields["uuid"].as_str(),
            Some("00000000-0000-0000-0000-000000000001")
        );
        assert_eq!(fields["ip_addr"].as_str(), None);
    }

    #[tokio::test]
    async fn query_logs_in_and_reauthenticates_once_after_unauthorized() {
        let (endpoint, requests, server) = test_server(vec![
            TestResponse {
                status: "200 OK",
                headers: "",
                body: "pong",
                delay: Duration::ZERO,
            },
            TestResponse {
                status: "204 No Content",
                headers: "Set-Cookie: session=first; Path=/; HttpOnly\r\n",
                body: "",
                delay: Duration::ZERO,
            },
            TestResponse {
                status: "401 Unauthorized",
                headers: "",
                body: "",
                delay: Duration::ZERO,
            },
            TestResponse {
                status: "204 No Content",
                headers: "Set-Cookie: session=second; Path=/; HttpOnly\r\n",
                body: "",
                delay: Duration::ZERO,
            },
            TestResponse {
                status: "200 OK",
                headers: "Content-Type: application/json\r\n",
                body: r#"{"tables":[],"query_summaries":null}"#,
                delay: Duration::ZERO,
            },
        ])
        .await;
        let client = NexusClient::new(
            vec![endpoint],
            RecoveryLogin {
                silo: "recovery".into(),
                username: "recovery".into(),
                password: "oxide".into(),
            },
            Duration::from_secs(2),
        )
        .unwrap();

        let result = client
            .query("get sled_data_link:bytes_sent", &CancellationToken::new())
            .await
            .unwrap();

        assert!(result.tables.is_empty());
        server.await.unwrap();
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 5);
        assert!(requests[0].starts_with("GET /v1/ping "));
        assert!(requests[1].starts_with("POST /v1/login/recovery/local "));
        assert!(
            requests[1]
                .contains(r#"{"username":"recovery","password":"oxide"}"#)
        );
        assert!(requests[2].contains("cookie: session=first"));
        assert!(requests[3].starts_with("POST /v1/login/recovery/local "));
        assert!(requests[4].contains("cookie: session=second"));
        assert!(requests[4].contains(r#""include_summaries":false"#));
    }

    #[tokio::test]
    async fn query_retries_another_endpoint_when_a_response_stalls() {
        let (stalled, stalled_requests, stalled_server) = test_server(vec![
            TestResponse {
                status: "200 OK",
                headers: "",
                body: "pong",
                delay: Duration::ZERO,
            },
            TestResponse {
                status: "204 No Content",
                headers: "Set-Cookie: session=stalled; Path=/; HttpOnly\r\n",
                body: "",
                delay: Duration::ZERO,
            },
            TestResponse {
                status: "200 OK",
                headers: "Content-Type: application/json\r\n",
                body: r#"{"tables":[],"query_summaries":null}"#,
                delay: Duration::from_millis(200),
            },
        ])
        .await;
        let (healthy, healthy_requests, healthy_server) = test_server(vec![
            TestResponse {
                status: "200 OK",
                headers: "",
                body: "pong",
                delay: Duration::from_millis(20),
            },
            TestResponse {
                status: "200 OK",
                headers: "",
                body: "pong",
                delay: Duration::ZERO,
            },
            TestResponse {
                status: "204 No Content",
                headers: "Set-Cookie: session=healthy; Path=/; HttpOnly\r\n",
                body: "",
                delay: Duration::ZERO,
            },
            TestResponse {
                status: "200 OK",
                headers: "Content-Type: application/json\r\n",
                body: r#"{"tables":[],"query_summaries":null}"#,
                delay: Duration::ZERO,
            },
        ])
        .await;
        let client = NexusClient::new(
            vec![stalled, healthy],
            RecoveryLogin {
                silo: "recovery".into(),
                username: "recovery".into(),
                password: "oxide".into(),
            },
            Duration::from_millis(50),
        )
        .unwrap();

        let result = client
            .query("get sled_data_link:bytes_sent", &CancellationToken::new())
            .await
            .unwrap();

        assert!(result.tables.is_empty());
        stalled_server.await.unwrap();
        healthy_server.await.unwrap();
        assert_eq!(stalled_requests.lock().unwrap().len(), 3);
        assert_eq!(healthy_requests.lock().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn query_tries_every_endpoint_before_failing() {
        let stalled_query = TestResponse {
            status: "200 OK",
            headers: "Content-Type: application/json\r\n",
            body: r#"{"tables":[],"query_summaries":null}"#,
            delay: Duration::from_millis(200),
        };
        let (first, _, first_server) = test_server(vec![
            TestResponse {
                status: "204 No Content",
                headers: "Set-Cookie: session=first; Path=/; HttpOnly\r\n",
                body: "",
                delay: Duration::ZERO,
            },
            stalled_query,
        ])
        .await;
        let (second, _, second_server) = test_server(vec![
            TestResponse {
                status: "200 OK",
                headers: "",
                body: "pong",
                delay: Duration::ZERO,
            },
            TestResponse {
                status: "204 No Content",
                headers: "Set-Cookie: session=second; Path=/; HttpOnly\r\n",
                body: "",
                delay: Duration::ZERO,
            },
            TestResponse {
                status: "200 OK",
                headers: "Content-Type: application/json\r\n",
                body: r#"{"tables":[],"query_summaries":null}"#,
                delay: Duration::from_millis(200),
            },
        ])
        .await;
        let (third, third_requests, third_server) = test_server(vec![
            TestResponse {
                status: "503 Service Unavailable",
                headers: "",
                body: "",
                delay: Duration::ZERO,
            },
            TestResponse {
                status: "200 OK",
                headers: "",
                body: "pong",
                delay: Duration::ZERO,
            },
            TestResponse {
                status: "204 No Content",
                headers: "Set-Cookie: session=third; Path=/; HttpOnly\r\n",
                body: "",
                delay: Duration::ZERO,
            },
            TestResponse {
                status: "200 OK",
                headers: "Content-Type: application/json\r\n",
                body: r#"{"tables":[],"query_summaries":null}"#,
                delay: Duration::ZERO,
            },
        ])
        .await;
        let client = NexusClient::new(
            vec![first.clone(), second, third],
            RecoveryLogin {
                silo: "recovery".into(),
                username: "recovery".into(),
                password: "oxide".into(),
            },
            Duration::from_millis(50),
        )
        .unwrap();
        *client.endpoint.lock().await = Some(first);

        let result = client
            .query("get sled_data_link:bytes_sent", &CancellationToken::new())
            .await
            .unwrap();

        assert!(result.tables.is_empty());
        first_server.await.unwrap();
        second_server.await.unwrap();
        third_server.await.unwrap();
        assert_eq!(third_requests.lock().unwrap().len(), 4);
    }
}
