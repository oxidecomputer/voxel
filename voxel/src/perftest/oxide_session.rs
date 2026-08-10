use super::{OxideAuthProviderMetadata, OxideSessionMetadata};
use crate::net::command_output_timeout;
use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{Seek, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;
use voxel_config::{RecoverySiloCfg, VoxelConfig};

pub(super) const PROFILE: &str = "voxel-perftest";
const PROFILE_PREFIX: &str = "voxel-perftest-profile-";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const API_COMMAND_TIMEOUT: Duration = Duration::from_secs(310);
const API_TIMEOUT_SECONDS: &str = "300";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const BUILTIN_TIMEOUT: Duration = Duration::from_secs(60);
const RETRY_DELAY: Duration = Duration::from_secs(2);
const HELPER_TIMEOUT: Duration = Duration::from_secs(120);
const RESOLVER_ATTEMPTS: u32 = 3;
const RESOLVER_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const OXIDE_VERSION_ARGS: &[&str] = &[
    "--timeout",
    API_TIMEOUT_SECONDS,
    "--resolve",
    "example.invalid:80:127.0.0.1",
    "version",
];

#[derive(Debug)]
pub(super) enum ProvisionError {
    Permanent(anyhow::Error),
    Transient(anyhow::Error),
    Boundary(anyhow::Error),
}

impl fmt::Display for ProvisionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Permanent(_) => {
                formatter.write_str("permanent Oxide provisioning failure")
            }
            Self::Transient(_) => formatter
                .write_str("transient Oxide provisioning attempts exhausted"),
            Self::Boundary(_) => {
                formatter.write_str("Oxide provisioning cleanup failure")
            }
        }
    }
}

impl std::error::Error for ProvisionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(match self {
            Self::Permanent(error)
            | Self::Transient(error)
            | Self::Boundary(error) => error.as_ref(),
        })
    }
}

#[derive(Clone, Copy)]
struct ProviderTiming {
    deadline: Duration,
    retry_delay: Duration,
    helper_timeout: Duration,
    request_timeout: Duration,
}

const PROVIDER_TIMING: ProviderTiming = ProviderTiming {
    deadline: BUILTIN_TIMEOUT,
    retry_delay: RETRY_DELAY,
    helper_timeout: HELPER_TIMEOUT,
    request_timeout: REQUEST_TIMEOUT,
};

#[derive(Deserialize)]
struct DeviceAuth {
    device_code: String,
    user_code: String,
}

#[derive(Deserialize)]
struct DeviceToken {
    access_token: String,
    token_id: Option<String>,
    time_expires: Option<String>,
}

#[derive(Deserialize)]
struct CurrentUser {
    id: String,
}

struct AcquiredCredentials {
    token: String,
    token_id: Option<String>,
    time_expires: Option<String>,
    user_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct OxideResolve {
    hostname: String,
    port: u16,
    address: Ipv4Addr,
}

impl OxideResolve {
    fn from_host(host: &str, address: Ipv4Addr) -> Result<Self> {
        let url = reqwest::Url::parse(host)
            .context("parse configured Oxide API host")?;
        let hostname = url
            .host_str()
            .ok_or_else(|| {
                anyhow!("configured Oxide API host has no hostname")
            })?
            .to_owned();
        let port = url.port_or_known_default().ok_or_else(|| {
            anyhow!("configured Oxide API host has no known port")
        })?;
        Ok(Self { hostname, port, address })
    }

    fn socket_addr(&self) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(self.address), self.port)
    }
}

impl fmt::Display for OxideResolve {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}:{}", self.hostname, self.port, self.address)
    }
}

pub(super) struct OxideSession {
    directory: Option<tempfile::TempDir>,
    metadata: OxideSessionMetadata,
    resolver: OxideResolve,
}

#[derive(Debug)]
pub(super) struct ApiCommandError {
    pub(super) kind: ApiErrorKind,
    pub(super) status: Option<u16>,
    pub(super) message: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ApiErrorKind {
    ShapeRejected,
    Authentication,
    Retryable,
    Permanent,
}

#[derive(Default)]
struct ApiFailureMetadata {
    status: Option<u16>,
    error_code: Option<String>,
    request_id: Option<String>,
}

#[derive(Deserialize)]
struct ApiErrorEnvelope {
    error_code: Option<String>,
    request_id: Option<String>,
}

fn api_failure_metadata(stdout: &[u8]) -> ApiFailureMetadata {
    const PREFIX: &str = "error; status code: ";
    let Some(text) = std::str::from_utf8(stdout).ok() else {
        return ApiFailureMetadata::default();
    };
    let mut lines = text.lines();
    let Some(first_line) = lines.next() else {
        return ApiFailureMetadata::default();
    };
    let Some(status_text) = first_line.strip_prefix(PREFIX) else {
        return ApiFailureMetadata::default();
    };
    let bytes = status_text.as_bytes();
    if bytes.len() <= 4
        || !bytes[..3].iter().all(u8::is_ascii_digit)
        || bytes[3] != b' '
    {
        return ApiFailureMetadata::default();
    }
    let status = status_text[..3].parse::<u16>().expect("validated digits");
    let envelope = serde_json::from_str::<ApiErrorEnvelope>(
        &lines.collect::<Vec<_>>().join("\n"),
    )
    .ok();
    let error_code = envelope
        .as_ref()
        .and_then(|body| body.error_code.as_deref())
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 64
                && value.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')
                })
        })
        .map(str::to_owned);
    let request_id = envelope
        .as_ref()
        .and_then(|body| body.request_id.as_deref())
        .and_then(|value| uuid::Uuid::parse_str(value).ok())
        .map(|value| value.to_string());
    ApiFailureMetadata { status: Some(status), error_code, request_id }
}

fn classify_api_failure(stdout: &[u8]) -> ApiErrorKind {
    let metadata = api_failure_metadata(stdout);
    if metadata.status == Some(507)
        && metadata.error_code.as_deref() == Some("InsufficientCapacity")
    {
        return ApiErrorKind::Permanent;
    }
    match metadata.status {
        Some(400) => ApiErrorKind::ShapeRejected,
        Some(401 | 403) => ApiErrorKind::Authentication,
        Some(404 | 409 | 500..=599) => ApiErrorKind::Retryable,
        Some(402..=499) => ApiErrorKind::Permanent,
        Some(_) | None => ApiErrorKind::Retryable,
    }
}

impl ApiFailureMetadata {
    fn diagnostic(&self) -> String {
        let mut fields = Vec::new();
        if let Some(status) = self.status {
            fields.push(format!("HTTP {status}"));
        }
        if let Some(error_code) = &self.error_code {
            fields.push(format!("error_code {error_code}"));
        }
        if let Some(request_id) = &self.request_id {
            fields.push(format!("request_id {request_id}"));
        }
        if fields.is_empty() {
            String::new()
        } else {
            format!("; {}", fields.join("; "))
        }
    }
}

fn api_error_kind_label(kind: ApiErrorKind) -> &'static str {
    match kind {
        ApiErrorKind::Authentication => "authentication/authorization",
        ApiErrorKind::ShapeRejected => "bad request",
        ApiErrorKind::Retryable => "retryable API failure",
        ApiErrorKind::Permanent => "permanent API failure",
    }
}

impl fmt::Display for ApiCommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ApiCommandError {}

impl OxideSession {
    fn provision<F>(
        cfg: &VoxelConfig,
        provider: OxideAuthProviderMetadata,
        oxide_cli_version: String,
        resolver: OxideResolve,
        populate: F,
    ) -> Result<Self>
    where
        F: FnOnce(&Path) -> Result<String>,
    {
        Self::provision_in(
            cfg,
            provider,
            oxide_cli_version,
            resolver,
            &std::env::temp_dir(),
            populate,
        )
    }

    fn provision_in<F>(
        cfg: &VoxelConfig,
        provider: OxideAuthProviderMetadata,
        oxide_cli_version: String,
        resolver: OxideResolve,
        temp_root: &Path,
        populate: F,
    ) -> Result<Self>
    where
        F: FnOnce(&Path) -> Result<String>,
    {
        let directory = private_directory(temp_root)?;
        let user_id = match populate(directory.path())
            .context("populate Oxide profile")
        {
            Ok(user_id) => user_id,
            Err(error) => return close_failed_directory(directory, error),
        };
        let host = match derived_host(cfg).and_then(|host| {
            validate_profile(directory.path(), &host, &user_id)?;
            Ok(host)
        }) {
            Ok(host) => host,
            Err(error) => return close_failed_directory(directory, error),
        };
        Ok(OxideSession {
            directory: Some(directory),
            metadata: OxideSessionMetadata {
                profile: PROFILE.into(),
                host,
                provider,
                oxide_cli_version,
            },
            resolver,
        })
    }

    fn directory(&self) -> Result<&Path> {
        self.directory.as_ref().map(tempfile::TempDir::path).ok_or_else(|| {
            anyhow!("Oxide profile directory is no longer available")
        })
    }

    pub(super) fn command(&self) -> Result<Command> {
        let directory = self.directory()?;
        let directory_utf8 = directory.to_str().ok_or_else(|| {
            anyhow!("Oxide profile directory is not valid UTF-8")
        })?;
        let mut command = Command::new("oxide");
        command
            .args(["--config-dir", directory_utf8, "--profile", PROFILE])
            .args(["--timeout", API_TIMEOUT_SECONDS])
            .arg("--resolve")
            .arg(self.resolver.to_string())
            .env_remove("OXIDE_HOST")
            .env_remove("OXIDE_TOKEN")
            .env_remove("OXIDE_PROFILE")
            .env("HOME", directory);
        Ok(command)
    }

    pub(super) fn api_request(
        &self,
        endpoint: &str,
        method: &str,
        body: Option<&str>,
    ) -> std::result::Result<String, ApiCommandError> {
        let mut command = self.command().map_err(|_| ApiCommandError {
            kind: ApiErrorKind::Permanent,
            status: None,
            message: format!(
                "Oxide API {method} {endpoint} command could not be constructed"
            ),
        })?;
        command.args(["api", endpoint, "--method", method]);
        let mut input = body
            .map(|body| {
                let directory = self.directory().map_err(|_| ())?;
                let mut input = tempfile::tempfile_in(directory).map_err(|_| ())?;
                input.write_all(body.as_bytes()).map_err(|_| ())?;
                input.rewind().map_err(|_| ())?;
                command
                    .args(["--header", "content-type:application/json", "--input", "-"])
                    .stdin(Stdio::from(input.try_clone().map_err(|_| ())?));
                Ok::<_, ()>(input)
            })
            .transpose()
            .map_err(|_| ApiCommandError {
                kind: ApiErrorKind::Permanent,
                status: None,
                message: format!(
                    "Oxide API {method} {endpoint} request input could not be prepared"
                ),
            })?;
        let output = command_output_timeout(command, API_COMMAND_TIMEOUT)
            .ok_or_else(|| ApiCommandError {
                kind: ApiErrorKind::Retryable,
                status: None,
                message: format!(
                    "Oxide API {method} {endpoint} failed to run or timed out"
                ),
            })?;
        drop(input.take());
        if !output.status.success() {
            let metadata = api_failure_metadata(&output.stdout);
            let kind = classify_api_failure(&output.stdout);
            return Err(ApiCommandError {
                kind,
                status: metadata.status,
                message: format!(
                    "Oxide API {method} {endpoint} failed with {} ({}{})",
                    output.status,
                    api_error_kind_label(kind),
                    metadata.diagnostic(),
                ),
            });
        }
        String::from_utf8(output.stdout).map_err(|_| ApiCommandError {
            kind: ApiErrorKind::Permanent,
            status: None,
            message: format!(
                "Oxide API {method} {endpoint} output was not UTF-8"
            ),
        })
    }

    pub(super) fn metadata(&self) -> &OxideSessionMetadata {
        &self.metadata
    }

    pub(super) fn close(mut self) -> Result<()> {
        self.directory
            .take()
            .ok_or_else(|| {
                anyhow!("Oxide profile directory is no longer available")
            })?
            .close()
            .context("remove temporary Oxide profile")
    }
}

fn close_failed_directory<T>(
    directory: tempfile::TempDir,
    error: anyhow::Error,
) -> Result<T> {
    match directory.close() {
        Ok(()) => Err(error),
        Err(cleanup) => Err(anyhow!(
            "Oxide session provisioning failed: {error:#}; additionally temporary profile cleanup failed: {cleanup}"
        )),
    }
}

fn combine_provisioning_failure(
    error: ProvisionError,
    cleanup: Result<()>,
) -> ProvisionError {
    let Err(cleanup) = cleanup else {
        return error;
    };
    let error = match error {
        ProvisionError::Permanent(error)
        | ProvisionError::Transient(error)
        | ProvisionError::Boundary(error) => error,
    };
    ProvisionError::Boundary(anyhow!(
        "Oxide session provisioning failed: {error:#}; additionally temporary profile cleanup failed: {cleanup:#}"
    ))
}

fn close_provisioning_failure<T>(
    session: OxideSession,
    error: ProvisionError,
) -> std::result::Result<T, ProvisionError> {
    let cleanup = session.close();
    Err(combine_provisioning_failure(error, cleanup))
}

enum RequestFailure {
    Permanent(anyhow::Error),
    Transient(anyhow::Error),
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ConfirmationState {
    Confirmed,
    Ambiguous,
}

async fn send_with_retry<F>(
    description: &'static str,
    deadline: tokio::time::Instant,
    delay: Duration,
    mut request: F,
) -> std::result::Result<reqwest::Response, ProvisionError>
where
    F: FnMut() -> reqwest::RequestBuilder,
{
    loop {
        let result = request().send().await;
        let classified = match result {
            Err(error) if error.is_timeout() => Err(RequestFailure::Transient(
                anyhow!("{description} timed out"),
            )),
            Err(error) if error.is_connect() => Err(RequestFailure::Transient(
                anyhow!("{description} could not connect"),
            )),
            // Reqwest's timeout/connect predicates vary by platform and error
            // source. These requests are safe to repeat within the deadline, so
            // every otherwise unclassified transport failure remains transient.
            Err(_) => Err(RequestFailure::Transient(anyhow!(
                "{description} transport failed"
            ))),
            Ok(response) if response.status().is_server_error() => {
                Err(RequestFailure::Transient(anyhow!(
                    "{description} returned HTTP {}",
                    response.status().as_u16()
                )))
            }
            Ok(response) if !response.status().is_success() => {
                Err(RequestFailure::Permanent(anyhow!(
                    "{description} returned HTTP {}",
                    response.status().as_u16()
                )))
            }
            Ok(response) => Ok(response),
        };
        match classified {
            Ok(response) => return Ok(response),
            Err(RequestFailure::Permanent(error)) => {
                return Err(ProvisionError::Permanent(error));
            }
            Err(RequestFailure::Transient(error)) => {
                if tokio::time::Instant::now() + delay >= deadline {
                    return Err(ProvisionError::Transient(error));
                }
                tokio::time::sleep(delay).await;
            }
        }
    }
}

async fn confirm_device(
    client: &reqwest::Client,
    host: &str,
    session_cookie: &str,
    user_code: &str,
    deadline: tokio::time::Instant,
    delay: Duration,
    previously_ambiguous: bool,
) -> std::result::Result<ConfirmationState, ProvisionError> {
    loop {
        let result = client
            .post(format!("{host}/device/confirm"))
            .header(reqwest::header::COOKIE, session_cookie)
            .json(&serde_json::json!({ "user_code": user_code }))
            .send()
            .await;
        match result {
            Ok(response) if response.status().is_success() => {
                return Ok(ConfirmationState::Confirmed);
            }
            // Immediately after device/auth, Nexus can briefly fail to find the
            // new code. Once an earlier confirmation was ambiguous, however, a
            // 404 may instead mean that the one-shot code was already consumed;
            // reconcile that state through device/token rather than POST again.
            Ok(response)
                if response.status() == reqwest::StatusCode::NOT_FOUND =>
            {
                if previously_ambiguous {
                    return Ok(ConfirmationState::Ambiguous);
                }
                if tokio::time::Instant::now() + delay >= deadline {
                    return Err(ProvisionError::Transient(anyhow!(
                        "device-confirmation request returned HTTP 404"
                    )));
                }
                tokio::time::sleep(delay).await;
            }
            // Confirmation consumes the code transactionally. A transport or
            // server failure can lose the response after that commit, so token
            // polling must determine whether it succeeded before any retry.
            Ok(response) if response.status().is_server_error() => {
                return Ok(ConfirmationState::Ambiguous);
            }
            Err(error) if error.is_timeout() || error.is_connect() => {
                return Ok(ConfirmationState::Ambiguous);
            }
            Ok(response) => {
                return Err(ProvisionError::Permanent(anyhow!(
                    "device-confirmation request returned HTTP {}",
                    response.status().as_u16()
                )));
            }
            // Once request construction succeeds, any transport error may mean
            // that Nexus committed confirmation but its response was lost.
            Err(_) => return Ok(ConfirmationState::Ambiguous),
        }
    }
}

async fn decode_json<T: for<'de> Deserialize<'de>>(
    response: reqwest::Response,
    description: &'static str,
) -> std::result::Result<T, ProvisionError> {
    response.json().await.map_err(|_| {
        ProvisionError::Permanent(anyhow!("malformed {description} response"))
    })
}

fn http_client(
    timeout: Duration,
    resolver: &OxideResolve,
) -> std::result::Result<reqwest::Client, ProvisionError> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout)
        .resolve(&resolver.hostname, resolver.socket_addr())
        .build()
        .map_err(|_| {
            ProvisionError::Permanent(anyhow!("build Oxide API client"))
        })
}

async fn current_user(
    client: &reqwest::Client,
    host: &str,
    token: &str,
    deadline: tokio::time::Instant,
    delay: Duration,
) -> std::result::Result<String, ProvisionError> {
    let response = send_with_retry(
        "current-user validation request",
        deadline,
        delay,
        || client.get(format!("{host}/v1/me")).bearer_auth(token),
    )
    .await?;
    let user: CurrentUser = decode_json(response, "current-user").await?;
    if user.id.is_empty() {
        return Err(ProvisionError::Permanent(anyhow!(
            "malformed current-user response"
        )));
    }
    Ok(user.id)
}

async fn builtin_credentials(
    cfg: &VoxelConfig,
    host: &str,
    resolver: &OxideResolve,
    timing: ProviderTiming,
) -> std::result::Result<AcquiredCredentials, ProvisionError> {
    let client = http_client(timing.request_timeout, resolver)?;
    let deadline = tokio::time::Instant::now() + timing.deadline;
    let operation = async {
        let login_url =
            format!("{host}/v1/login/{}/local", cfg.recovery_silo.silo_name);
        let login = send_with_retry(
            "local login request",
            deadline,
            timing.retry_delay,
            || {
                client.post(&login_url).json(&serde_json::json!({
                    "username": cfg.recovery_silo.user_name,
                    "password": "oxide",
                }))
            },
        )
        .await?;
        let session_cookie = login
            .headers()
            .get_all(reqwest::header::SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .filter_map(|value| value.split(';').next())
            .find(|cookie| cookie.starts_with("session="))
            .map(str::to_owned)
            .ok_or_else(|| {
                ProvisionError::Permanent(anyhow!(
                    "login response missing session cookie"
                ))
            })?;

        let client_id = uuid::Uuid::new_v4().to_string();
        let auth = send_with_retry(
            "device-authorization request",
            deadline,
            timing.retry_delay,
            || {
                client
                    .post(format!("{host}/device/auth"))
                    .form(&[("client_id", client_id.as_str())])
            },
        )
        .await?;
        let auth: DeviceAuth =
            decode_json(auth, "device-authorization").await?;
        if auth.device_code.is_empty() || auth.user_code.is_empty() {
            return Err(ProvisionError::Permanent(anyhow!(
                "malformed device-authorization response"
            )));
        }

        let mut confirmation = confirm_device(
            &client,
            host,
            &session_cookie,
            &auth.user_code,
            deadline,
            timing.retry_delay,
            false,
        )
        .await?;

        let token = loop {
            let response = client
                .post(format!("{host}/device/token"))
                .form(&[
                    (
                        "grant_type",
                        "urn:ietf:params:oauth:grant-type:device_code",
                    ),
                    ("client_id", client_id.as_str()),
                    ("device_code", auth.device_code.as_str()),
                ])
                .send()
                .await;
            match response {
                Ok(response) if response.status().is_success() => {
                    break decode_json::<DeviceToken>(response, "device-token")
                        .await?;
                }
                Ok(response) if response.status().is_server_error() => {}
                Ok(response) if response.status().is_client_error() => {
                    #[derive(Deserialize)]
                    struct TokenError {
                        error: String,
                    }
                    let pending = response
                        .json::<TokenError>()
                        .await
                        .map(|body| body.error == "authorization_pending")
                        .unwrap_or(false);
                    if !pending {
                        return Err(ProvisionError::Permanent(anyhow!(
                            "device-token request rejected"
                        )));
                    }
                    if confirmation == ConfirmationState::Ambiguous {
                        confirmation = confirm_device(
                            &client,
                            host,
                            &session_cookie,
                            &auth.user_code,
                            deadline,
                            timing.retry_delay,
                            true,
                        )
                        .await?;
                    }
                }
                Ok(_) => {
                    return Err(ProvisionError::Permanent(anyhow!(
                        "device-token request rejected"
                    )));
                }
                Err(error) if error.is_timeout() || error.is_connect() => {}
                Err(_) => {
                    return Err(ProvisionError::Permanent(anyhow!(
                        "device-token request failed"
                    )));
                }
            }
            if tokio::time::Instant::now() + timing.retry_delay >= deadline {
                return Err(ProvisionError::Transient(anyhow!(
                    "device-token attempts exhausted"
                )));
            }
            tokio::time::sleep(timing.retry_delay).await;
        };
        if token.access_token.is_empty() {
            return Err(ProvisionError::Permanent(anyhow!(
                "malformed device-token response"
            )));
        }
        let user_id = current_user(
            &client,
            host,
            &token.access_token,
            deadline,
            timing.retry_delay,
        )
        .await?;
        Ok(AcquiredCredentials {
            token: token.access_token,
            token_id: token.token_id,
            time_expires: token.time_expires,
            user_id,
        })
    };
    tokio::time::timeout_at(deadline, operation).await.unwrap_or_else(|_| {
        Err(ProvisionError::Transient(anyhow!(
            "built-in provisioning deadline exceeded"
        )))
    })
}

pub(super) async fn provision(
    cfg: &VoxelConfig,
    helper: Option<&Path>,
) -> std::result::Result<OxideSession, ProvisionError> {
    provision_with_timing(cfg, helper, PROVIDER_TIMING).await
}

async fn provision_with_timing(
    cfg: &VoxelConfig,
    helper: Option<&Path>,
    timing: ProviderTiming,
) -> std::result::Result<OxideSession, ProvisionError> {
    validate_config(cfg, helper).map_err(ProvisionError::Permanent)?;
    let oxide_cli_version =
        static_preflight(cfg, helper, &std::env::temp_dir())
            .map_err(ProvisionError::Permanent)?;
    let host = derived_host(cfg).map_err(ProvisionError::Permanent)?;
    let resolver = discover_resolve(cfg, &host, timing.retry_delay).await?;

    if let Some(helper) = helper {
        let provider =
            OxideAuthProviderMetadata::Helper { path: helper.to_path_buf() };
        let session = OxideSession::provision(
            cfg,
            provider,
            oxide_cli_version,
            resolver.clone(),
            |directory| {
                run_helper(
                    cfg,
                    helper,
                    directory,
                    &host,
                    &resolver,
                    timing.helper_timeout,
                )?;
                profile_at(directory).map(|profile| profile.user.clone())
            },
        )
        .map_err(ProvisionError::Permanent)?;
        let validation = async {
            let profile = profile_at(
                session.directory().map_err(ProvisionError::Permanent)?,
            )
            .map_err(ProvisionError::Permanent)?;
            let client = http_client(timing.request_timeout, &resolver)?;
            let deadline = tokio::time::Instant::now() + timing.deadline;
            let authenticated = current_user(
                &client,
                &host,
                &profile.token,
                deadline,
                timing.retry_delay,
            )
            .await?;
            if authenticated != profile.user {
                return Err(ProvisionError::Permanent(anyhow!(
                    "Oxide profile user does not match /v1/me"
                )));
            }
            Ok(())
        }
        .await;
        match validation {
            Ok(()) => Ok(session),
            Err(error) => close_provisioning_failure(session, error),
        }
    } else {
        let credentials =
            builtin_credentials(cfg, &host, &resolver, timing).await?;
        let user_id = credentials.user_id.clone();
        OxideSession::provision(
            cfg,
            OxideAuthProviderMetadata::Builtin,
            oxide_cli_version,
            resolver,
            |directory| {
                write_profile_with_metadata(
                    directory,
                    &host,
                    &credentials.token,
                    &user_id,
                    credentials.token_id.as_deref(),
                    credentials.time_expires.as_deref(),
                )?;
                Ok(user_id)
            },
        )
        .map_err(ProvisionError::Permanent)
    }
}

fn run_helper(
    cfg: &VoxelConfig,
    helper: &Path,
    directory: &Path,
    host: &str,
    resolver: &OxideResolve,
    timeout: Duration,
) -> Result<()> {
    let mut command = Command::new(helper);
    command
        .env_remove("OXIDE_HOST")
        .env_remove("OXIDE_TOKEN")
        .env_remove("OXIDE_PROFILE")
        .env("HOME", directory)
        .env("VOXEL_PERFTEST_OXIDE_CONFIG_DIR", directory)
        .env("VOXEL_PERFTEST_OXIDE_PROFILE", PROFILE)
        .env("VOXEL_PERFTEST_OXIDE_HOST", host)
        .env("VOXEL_PERFTEST_OXIDE_RESOLVE", resolver.to_string())
        .env("VOXEL_PERFTEST_RECOVERY_SILO", &cfg.recovery_silo.silo_name)
        .env("VOXEL_PERFTEST_RECOVERY_USER", &cfg.recovery_silo.user_name);
    let output = command_output_timeout(command, timeout).ok_or_else(|| {
        anyhow!("Oxide auth helper failed to run or timed out")
    })?;
    if !output.status.success() {
        bail!("Oxide auth helper exited unsuccessfully");
    }
    Ok(())
}

pub(super) fn static_preflight(
    cfg: &VoxelConfig,
    helper: Option<&Path>,
    temp_root: &Path,
) -> Result<String> {
    validate_config(cfg, helper)?;
    validate_temp_root(temp_root)?;

    let mut dig_version = Command::new("dig");
    dig_version.arg("-v");
    let output = command_output_timeout(dig_version, COMMAND_TIMEOUT)
        .ok_or_else(|| anyhow!("`dig -v` failed to run or timed out"))?;
    if !output.status.success() {
        bail!("`dig -v` failed with {}", output.status);
    }

    let mut version_command = Command::new("oxide");
    version_command.args(OXIDE_VERSION_ARGS);
    let output = command_output_timeout(version_command, COMMAND_TIMEOUT).ok_or_else(|| {
        anyhow!("Oxide CLI resolver compatibility probe failed to run or timed out")
    })?;
    if !output.status.success() {
        bail!(
            "Oxide CLI resolver compatibility probe failed with {}",
            output.status
        );
    }
    let version = String::from_utf8(output.stdout)
        .context("Oxide CLI version output was not UTF-8")?
        .trim()
        .to_owned();
    if version.is_empty() {
        bail!("Oxide CLI version probe returned empty output");
    }

    let probe = private_directory(temp_root)?;
    probe.close().context("remove temporary Oxide profile probe")?;
    Ok(version)
}

fn validate_config(cfg: &VoxelConfig, helper: Option<&Path>) -> Result<()> {
    validate_label("recovery silo", &cfg.recovery_silo.silo_name)?;
    validate_label("recovery user", &cfg.recovery_silo.user_name)?;
    for label in cfg.network.dns_zone.split('.') {
        validate_label("DNS", label)?;
    }
    cfg.network
        .for_rack(0)
        .external_dns_ips
        .first()
        .ok_or_else(|| anyhow!("rack 1 has no authoritative external DNS server configured"))?
        .parse::<IpAddr>()
        .map_err(|_| {
            anyhow!("rack 1 authoritative external DNS server is not a valid IP address")
        })?;
    if let Some(helper) = helper {
        if !helper.is_absolute() {
            bail!("Oxide auth helper path must be absolute");
        }
        if !helper.is_file() {
            bail!("Oxide auth helper does not exist or is not a file");
        }
    } else if cfg.recovery_silo.user_password_hash
        != RecoverySiloCfg::default().user_password_hash
    {
        bail!("custom recovery password hash requires --oxide-auth-helper");
    }
    Ok(())
}

fn validate_label(kind: &str, label: &str) -> Result<()> {
    let valid = !label.is_empty()
        && label.len() <= 63
        && !label.starts_with('-')
        && !label.ends_with('-')
        && label
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-');
    if !valid {
        bail!("invalid {kind} label");
    }
    Ok(())
}

fn validate_temp_root(temp_root: &Path) -> Result<()> {
    for entry in fs::read_dir(temp_root).context("read temporary directory")? {
        let entry = entry.context("read temporary directory entry")?;
        if entry.file_name().to_string_lossy().starts_with(PROFILE_PREFIX) {
            bail!("stale Oxide perftest profile exists in temporary directory");
        }
    }
    Ok(())
}

fn derived_host(cfg: &VoxelConfig) -> Result<String> {
    validate_label("recovery silo", &cfg.recovery_silo.silo_name)?;
    let dns_zone = cfg.network.for_rack(0).dns_zone;
    for label in dns_zone.split('.') {
        validate_label("DNS", label)?;
    }
    Ok(format!("http://{}.sys.{}", cfg.recovery_silo.silo_name, dns_zone))
}

async fn discover_resolve(
    cfg: &VoxelConfig,
    host: &str,
    retry_delay: Duration,
) -> std::result::Result<OxideResolve, ProvisionError> {
    let mut resolver = OxideResolve::from_host(host, Ipv4Addr::UNSPECIFIED)
        .map_err(ProvisionError::Permanent)?;
    let rack_network = cfg.network.for_rack(0);
    let dns_server =
        rack_network.external_dns_ips.first().cloned().ok_or_else(|| {
            ProvisionError::Permanent(anyhow!(
                "rack 1 has no authoritative external DNS server configured"
            ))
        })?;
    for attempt in 1..=RESOLVER_ATTEMPTS {
        match query_resolve(&dns_server, &resolver.hostname) {
            Ok(address) => {
                resolver.address = address;
                return Ok(resolver);
            }
            Err(error) if attempt == RESOLVER_ATTEMPTS => {
                return Err(ProvisionError::Transient(error));
            }
            Err(_) => tokio::time::sleep(retry_delay).await,
        }
    }
    unreachable!("resolver attempt loop always returns")
}

fn query_resolve(dns_server: &str, hostname: &str) -> Result<Ipv4Addr> {
    let mut command = Command::new("dig");
    command.args([
        "+short",
        "+timeout=3",
        "+tries=1",
        &format!("@{dns_server}"),
        &hostname,
        "A",
    ]);
    let output = command_output_timeout(command, RESOLVER_COMMAND_TIMEOUT).ok_or_else(|| {
        anyhow!("authoritative DNS {dns_server} query for {hostname} failed to run or timed out")
    })?;
    if !output.status.success() {
        bail!(
            "authoritative DNS {dns_server} query for {hostname} failed with {}",
            output.status
        );
    }
    std::str::from_utf8(&output.stdout)
        .ok()
        .and_then(|stdout| {
            stdout
                .lines()
                .find_map(|line| line.trim().parse::<Ipv4Addr>().ok())
        })
        .ok_or_else(|| {
            anyhow!("authoritative DNS {dns_server} returned no valid A record for {hostname}")
        })
}

fn private_directory(temp_root: &Path) -> Result<tempfile::TempDir> {
    let directory = tempfile::Builder::new()
        .prefix(PROFILE_PREFIX)
        .tempdir_in(temp_root)
        .context("create temporary Oxide profile")?;
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
        .context("secure temporary Oxide profile")?;
    Ok(directory)
}

#[cfg(test)]
fn write_profile(
    directory: &Path,
    host: &str,
    token: &str,
    user: &str,
) -> Result<()> {
    write_profile_with_metadata(directory, host, token, user, None, None)
}

fn write_profile_with_metadata(
    directory: &Path,
    host: &str,
    token: &str,
    user: &str,
    token_id: Option<&str>,
    time_expires: Option<&str>,
) -> Result<()> {
    write_private(
        &directory.join("config.toml"),
        "Oxide config",
        format!("default-profile = {PROFILE:?}\n").as_bytes(),
    )?;
    let mut credentials = format!(
        "[profile.{PROFILE}]\nhost = {host:?}\ntoken = {token:?}\nuser = {user:?}\n"
    );
    if let Some(token_id) = token_id {
        credentials.push_str(&format!("token_id = {token_id:?}\n"));
    }
    if let Some(time_expires) = time_expires {
        credentials.push_str(&format!("time_expires = {time_expires:?}\n"));
    }
    write_private(
        &directory.join("credentials.toml"),
        "Oxide credentials",
        credentials.as_bytes(),
    )
}

fn write_private(
    path: &Path,
    description: &str,
    contents: &[u8],
) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("create {description}"))?;
    file.write_all(contents).with_context(|| format!("write {description}"))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct OxideConfig {
    default_profile: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OxideCredentials {
    profile: BTreeMap<String, OxideProfile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OxideProfile {
    host: String,
    token: String,
    user: String,
    #[allow(dead_code)]
    token_id: Option<String>,
    #[allow(dead_code)]
    time_expires: Option<String>,
}

fn profile_at(directory: &Path) -> Result<OxideProfile> {
    require_private_file(&directory.join("config.toml"), "Oxide config")?;
    require_private_file(
        &directory.join("credentials.toml"),
        "Oxide credentials",
    )?;
    let contents = fs::read_to_string(directory.join("credentials.toml"))
        .context("read Oxide credentials")?;
    let mut credentials: OxideCredentials = toml::from_str(&contents)
        .map_err(|_| anyhow!("parse Oxide credentials: invalid TOML"))?;
    if credentials.profile.len() != 1 {
        bail!("Oxide credentials must contain exactly one profile");
    }
    credentials.profile.remove(PROFILE).ok_or_else(|| {
        anyhow!("Oxide credentials are missing profile {PROFILE}")
    })
}

fn validate_profile(
    directory: &Path,
    expected_host: &str,
    current_user: &str,
) -> Result<()> {
    require_private_file(&directory.join("config.toml"), "Oxide config")?;
    let config: OxideConfig = toml::from_str(
        &fs::read_to_string(directory.join("config.toml"))
            .context("read Oxide config")?,
    )
    .context("parse Oxide config")?;
    if config.default_profile != PROFILE {
        bail!("Oxide default profile is not {PROFILE}");
    }
    let credentials_path = directory.join("credentials.toml");
    require_private_file(&credentials_path, "Oxide credentials")?;
    let credentials_contents = fs::read_to_string(credentials_path)
        .context("read Oxide credentials")?;
    let credentials: OxideCredentials =
        toml::from_str(&credentials_contents)
            .map_err(|_| anyhow!("parse Oxide credentials: invalid TOML"))?;
    if credentials.profile.len() != 1 {
        bail!("Oxide credentials must contain exactly one profile");
    }
    let profile = credentials.profile.get(PROFILE).ok_or_else(|| {
        anyhow!("Oxide credentials are missing profile {PROFILE}")
    })?;
    if profile.host != expected_host {
        bail!("Oxide profile host does not match the configured rack");
    }
    if profile.token.is_empty() {
        bail!("Oxide profile token is empty");
    }
    if profile.user != current_user {
        bail!("Oxide profile user does not match /v1/me");
    }
    Ok(())
}

fn require_private_file(path: &Path, name: &str) -> Result<()> {
    let mode = fs::metadata(path)
        .with_context(|| format!("inspect {name}"))?
        .permissions()
        .mode()
        & 0o777;
    if mode != 0o600 {
        bail!("{name} must have mode 0600");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::io::Read;
    use std::net::TcpListener;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    static ENVIRONMENT: Mutex<()> = Mutex::new(());

    #[test]
    fn api_status_classification_uses_only_exact_first_stdout_line() {
        for (status, expected) in [
            (400, ApiErrorKind::ShapeRejected),
            (401, ApiErrorKind::Authentication),
            (403, ApiErrorKind::Authentication),
            (402, ApiErrorKind::Permanent),
            (404, ApiErrorKind::Retryable),
            (409, ApiErrorKind::Retryable),
            (499, ApiErrorKind::Permanent),
            (500, ApiErrorKind::Retryable),
            (599, ApiErrorKind::Retryable),
        ] {
            let stdout = format!(
                "error; status code: {status} Reason\n{{\"code\": 999}}"
            );
            assert_eq!(classify_api_failure(stdout.as_bytes()), expected);
        }

        for stdout in [
            "",
            "error; status code: 40 Reason\n",
            "error; status code: 4000 Reason\n",
            "error; status code: 400",
            "error; status code: 400Forbidden",
            "error; status code: 400 ",
            "error; status code: 499 ",
            "error; status code: 599 ",
            "prefix error; status code: 400 Reason\n",
            "body first\nerror; status code: 400 Reason\n",
            "{\"incidental\": 401}\n",
        ] {
            assert_eq!(
                classify_api_failure(stdout.as_bytes()),
                ApiErrorKind::Retryable
            );
        }
    }

    #[test]
    fn api_insufficient_capacity_is_permanent_only_when_structured() {
        let capacity = b"error; status code: 507 Insufficient Storage\n{\"error_code\":\"InsufficientCapacity\",\"request_id\":\"6585c62d-11a0-46ea-89eb-84d2b4e548fc\"}\n";
        assert_eq!(classify_api_failure(capacity), ApiErrorKind::Permanent);

        for stdout in [
            b"error; status code: 507 Insufficient Storage\n{}\n".as_slice(),
            b"error; status code: 507 Insufficient Storage\n{\"error_code\":\"Other\"}\n"
                .as_slice(),
            b"error; status code: 500 Internal Server Error\n{\"error_code\":\"InsufficientCapacity\"}\n"
                .as_slice(),
        ] {
            assert_eq!(classify_api_failure(stdout), ApiErrorKind::Retryable);
        }
    }

    #[test]
    fn api_failure_metadata_rejects_unsafe_server_fields() {
        let stdout = b"error; status code: 400 Bad Request\n{\"error_code\":\"Invalid Request; secret\",\"message\":\"distinctive-secret\",\"request_id\":\"not-a-uuid\"}\n";

        let metadata = api_failure_metadata(stdout);
        let diagnostic = metadata.diagnostic();

        assert_eq!(metadata.status, Some(400));
        assert_eq!(metadata.error_code, None);
        assert_eq!(metadata.request_id, None);
        assert_eq!(diagnostic, "; HTTP 400");
        assert!(!diagnostic.contains("distinctive-secret"));
    }

    #[test]
    fn api_request_ignores_stderr_digits_and_redacts_process_output() {
        let _lock =
            ENVIRONMENT.lock().unwrap_or_else(|error| error.into_inner());
        let root = tempfile::tempdir().unwrap();
        let bin = tempfile::tempdir().unwrap();
        let script = bin.path().join("oxide");
        let session = test_session(root.path());
        let _environment = EnvironmentGuard::set(&[("PATH", bin.path())]);

        for (contents, expected, secrets) in [
            (
                "#!/bin/sh\nprintf 'body with 403 later\\n'\nprintf 'incidental 400 secret-stderr' >&2\nexit 1\n",
                ApiErrorKind::Retryable,
                ["body with 403", "secret-stderr"],
            ),
            (
                "#!/bin/sh\nprintf 'error; status code: 403 Forbidden\\nsecret-body\\n'\nprintf 'error' >&2\nexit 1\n",
                ApiErrorKind::Authentication,
                ["secret-body", "error; status code"],
            ),
        ] {
            fs::write(&script, contents).unwrap();
            fs::set_permissions(&script, fs::Permissions::from_mode(0o700))
                .unwrap();
            let error =
                session.api_request("/v1/test", "GET", None).unwrap_err();
            assert_eq!(error.kind, expected);
            assert!(error.to_string().contains("GET /v1/test"));
            for secret in secrets {
                assert!(!error.to_string().contains(secret));
            }
        }
    }

    #[tokio::test]
    async fn builtin_auth_follows_device_flow() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let host = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let replies = [
                (
                    "POST /v1/login/recovery/local ",
                    "HTTP/1.1 204 No Content\r\nSet-Cookie: other=x\r\nSet-Cookie: session=cookie-secret; HttpOnly\r\nContent-Length: 0\r\n\r\n",
                ),
                (
                    "POST /device/auth ",
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 43\r\n\r\n{\"device_code\":\"device\",\"user_code\":\"user\"}",
                ),
                (
                    "POST /device/confirm ",
                    "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n",
                ),
                (
                    "POST /device/token ",
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 31\r\n\r\n{\"access_token\":\"token-secret\"}",
                ),
                (
                    "GET /v1/me ",
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 16\r\n\r\n{\"id\":\"user-id\"}",
                ),
            ];
            for (expected, reply) in replies {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0; 4096];
                let size = stream.read(&mut request).unwrap();
                let request = String::from_utf8_lossy(&request[..size]);
                assert!(
                    request.starts_with(expected),
                    "unexpected request path"
                );
                let body = request.split_once("\r\n\r\n").unwrap().1;
                match expected {
                    value if value.starts_with("POST /v1/login") => assert_eq!(
                        serde_json::from_str::<serde_json::Value>(body)
                            .unwrap(),
                        serde_json::json!({"username":"recovery","password":"oxide"})
                    ),
                    value if value.starts_with("POST /device/auth") => {
                        assert!(request.contains(
                            "content-type: application/x-www-form-urlencoded"
                        ));
                        assert!(body.starts_with("client_id="));
                        assert!(!body["client_id=".len()..].is_empty());
                    }
                    value if value.starts_with("POST /device/confirm") => {
                        assert!(
                            request
                                .to_ascii_lowercase()
                                .contains("cookie: session=cookie-secret")
                        );
                        assert_eq!(body, "{\"user_code\":\"user\"}");
                    }
                    value if value.starts_with("POST /device/token") => {
                        assert!(request.contains(
                            "content-type: application/x-www-form-urlencoded"
                        ));
                        assert!(body.contains(
                            "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code"
                        ));
                        assert!(body.contains("&client_id="));
                        assert!(body.contains("&device_code=device"));
                    }
                    value if value.starts_with("GET /v1/me") => {
                        assert!(
                            request
                                .to_ascii_lowercase()
                                .contains("authorization: bearer token-secret")
                        );
                        assert!(body.is_empty());
                    }
                    _ => unreachable!(),
                }
                stream.write_all(reply.as_bytes()).unwrap();
            }
        });
        let credentials = builtin_credentials(
            &VoxelConfig::default(),
            &host,
            &test_resolver(&host),
            ProviderTiming {
                deadline: Duration::from_secs(2),
                retry_delay: Duration::ZERO,
                helper_timeout: Duration::ZERO,
                request_timeout: Duration::from_secs(1),
            },
        )
        .await
        .unwrap();
        assert_eq!(credentials.user_id, "user-id");
        server.join().unwrap();
    }

    #[tokio::test]
    async fn builtin_auth_retries_initial_confirmation_not_found() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let host = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let replies = [
                (
                    "POST /v1/login/recovery/local ",
                    "HTTP/1.1 204 No Content\r\nSet-Cookie: session=s\r\nContent-Length: 0\r\n\r\n",
                ),
                (
                    "POST /device/auth ",
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 35\r\n\r\n{\"device_code\":\"d\",\"user_code\":\"u\"}",
                ),
                (
                    "POST /device/confirm ",
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n",
                ),
                (
                    "POST /device/confirm ",
                    "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n",
                ),
                (
                    "POST /device/token ",
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 20\r\n\r\n{\"access_token\":\"t\"}",
                ),
                (
                    "GET /v1/me ",
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 10\r\n\r\n{\"id\":\"u\"}",
                ),
            ];
            for (expected, reply) in replies {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0; 4096];
                let size = stream.read(&mut request).unwrap();
                let request = String::from_utf8_lossy(&request[..size]);
                assert!(
                    request.starts_with(expected),
                    "unexpected request: {request}"
                );
                stream.write_all(reply.as_bytes()).unwrap();
            }
        });

        let credentials = builtin_credentials(
            &VoxelConfig::default(),
            &host,
            &test_resolver(&host),
            ProviderTiming {
                deadline: Duration::from_secs(2),
                retry_delay: Duration::ZERO,
                helper_timeout: Duration::ZERO,
                request_timeout: Duration::from_secs(1),
            },
        )
        .await
        .unwrap();

        assert_eq!(credentials.user_id, "u");
        server.join().unwrap();
    }

    #[tokio::test]
    async fn builtin_auth_reconciles_ambiguous_confirmation_through_device_token()
     {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let host = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let replies = [
                (
                    "POST /v1/login/recovery/local ",
                    "HTTP/1.1 204 No Content\r\nSet-Cookie: session=s\r\nContent-Length: 0\r\n\r\n",
                ),
                (
                    "POST /device/auth ",
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 35\r\n\r\n{\"device_code\":\"d\",\"user_code\":\"u\"}",
                ),
                (
                    "POST /device/confirm ",
                    "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n",
                ),
                (
                    "POST /device/token ",
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 20\r\n\r\n{\"access_token\":\"t\"}",
                ),
                (
                    "GET /v1/me ",
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 10\r\n\r\n{\"id\":\"u\"}",
                ),
            ];
            for (expected, reply) in replies {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0; 4096];
                let size = stream.read(&mut request).unwrap();
                let request = String::from_utf8_lossy(&request[..size]);
                assert!(
                    request.starts_with(expected),
                    "unexpected request: {request}"
                );
                stream.write_all(reply.as_bytes()).unwrap();
            }
        });

        let credentials = builtin_credentials(
            &VoxelConfig::default(),
            &host,
            &test_resolver(&host),
            ProviderTiming {
                deadline: Duration::from_secs(2),
                retry_delay: Duration::ZERO,
                helper_timeout: Duration::ZERO,
                request_timeout: Duration::from_secs(1),
            },
        )
        .await
        .unwrap();

        assert_eq!(credentials.user_id, "u");
        server.join().unwrap();
    }

    #[tokio::test]
    async fn builtin_auth_reconfirms_when_ambiguous_confirmation_is_still_pending()
     {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let host = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let replies = [
                (
                    "POST /v1/login/recovery/local ",
                    "HTTP/1.1 204 No Content\r\nSet-Cookie: session=s\r\nContent-Length: 0\r\n\r\n",
                ),
                (
                    "POST /device/auth ",
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 35\r\n\r\n{\"device_code\":\"d\",\"user_code\":\"u\"}",
                ),
                (
                    "POST /device/confirm ",
                    "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n",
                ),
                (
                    "POST /device/token ",
                    "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: 33\r\n\r\n{\"error\":\"authorization_pending\"}",
                ),
                (
                    "POST /device/confirm ",
                    "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n",
                ),
                (
                    "POST /device/token ",
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 20\r\n\r\n{\"access_token\":\"t\"}",
                ),
                (
                    "GET /v1/me ",
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 10\r\n\r\n{\"id\":\"u\"}",
                ),
            ];
            for (expected, reply) in replies {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0; 4096];
                let size = stream.read(&mut request).unwrap();
                let request = String::from_utf8_lossy(&request[..size]);
                assert!(
                    request.starts_with(expected),
                    "unexpected request: {request}"
                );
                stream.write_all(reply.as_bytes()).unwrap();
            }
        });

        let credentials = builtin_credentials(
            &VoxelConfig::default(),
            &host,
            &test_resolver(&host),
            ProviderTiming {
                deadline: Duration::from_secs(2),
                retry_delay: Duration::ZERO,
                helper_timeout: Duration::ZERO,
                request_timeout: Duration::from_secs(1),
            },
        )
        .await
        .unwrap();

        assert_eq!(credentials.user_id, "u");
        server.join().unwrap();
    }

    fn one_response_server(
        response: &'static str,
    ) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let host = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 4096];
            stream.read(&mut request).unwrap();
            stream.write_all(response.as_bytes()).unwrap();
        });
        (host, server)
    }

    fn test_timing() -> ProviderTiming {
        ProviderTiming {
            deadline: Duration::from_millis(200),
            retry_delay: Duration::ZERO,
            helper_timeout: Duration::from_millis(100),
            request_timeout: Duration::from_millis(50),
        }
    }

    fn test_resolver(host: &str) -> OxideResolve {
        OxideResolve::from_host(host, Ipv4Addr::LOCALHOST).unwrap()
    }

    fn provision_error_detail(error: &ProvisionError) -> String {
        match error {
            ProvisionError::Permanent(error)
            | ProvisionError::Transient(error)
            | ProvisionError::Boundary(error) => format!("{error:#}"),
        }
    }

    fn assert_redacted_classification(
        error: &ProvisionError,
        transient: bool,
        secret: &str,
    ) {
        assert_eq!(matches!(error, ProvisionError::Transient(_)), transient);
        assert!(!provision_error_detail(error).contains(secret));
    }

    #[tokio::test]
    async fn builtin_auth_classifies_refused_connection_as_transient() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let host = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);

        let error = match builtin_credentials(
            &VoxelConfig::default(),
            &host,
            &test_resolver(&host),
            test_timing(),
        )
        .await
        {
            Err(error) => error,
            Ok(_) => panic!("refused connection unexpectedly succeeded"),
        };
        assert_redacted_classification(&error, true, "oxide");
        let error = provision_error_detail(&error);
        assert!(
            error.contains("local login request could not connect")
                || error.contains("local login request transport failed")
                || error.contains("built-in provisioning deadline exceeded"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn builtin_auth_classifies_request_timeout_as_transient() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let host = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_millis(150));
        });

        let error = match builtin_credentials(
            &VoxelConfig::default(),
            &host,
            &test_resolver(&host),
            test_timing(),
        )
        .await
        {
            Err(error) => error,
            Ok(_) => panic!("delayed response unexpectedly succeeded"),
        };
        assert_redacted_classification(&error, true, "oxide");
        let error = provision_error_detail(&error);
        assert!(
            error.contains("local login request timed out")
                || error.contains("local login request transport failed")
                || error.contains("built-in provisioning deadline exceeded"),
            "{error}"
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn builtin_auth_classifies_missing_cookie_as_permanent() {
        let (host, server) = one_response_server(
            "HTTP/1.1 204 No Content\r\nSet-Cookie: secret-cookie=value\r\nContent-Length: 0\r\n\r\n",
        );
        let error = match builtin_credentials(
            &VoxelConfig::default(),
            &host,
            &test_resolver(&host),
            test_timing(),
        )
        .await
        {
            Err(error) => error,
            Ok(_) => panic!("missing cookie unexpectedly succeeded"),
        };
        assert_redacted_classification(&error, false, "secret-cookie");
        server.join().unwrap();
    }

    #[tokio::test]
    async fn builtin_auth_classifies_other_client_error_as_permanent() {
        let (host, server) = one_response_server(
            "HTTP/1.1 418 I'm a teapot\r\nContent-Length: 18\r\n\r\ndistinctive-secret",
        );
        let error = match builtin_credentials(
            &VoxelConfig::default(),
            &host,
            &test_resolver(&host),
            test_timing(),
        )
        .await
        {
            Err(error) => error,
            Ok(_) => panic!("client error unexpectedly succeeded"),
        };
        assert_redacted_classification(&error, false, "distinctive-secret");
        server.join().unwrap();
    }

    #[tokio::test]
    async fn builtin_auth_rejects_redirect_and_unauthorized_permanently() {
        for response in [
            "HTTP/1.1 302 Found\r\nLocation: http://invalid/\r\nContent-Length: 0\r\n\r\n",
            "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n",
        ] {
            let (host, server) = one_response_server(response);
            let error = match builtin_credentials(
                &VoxelConfig::default(),
                &host,
                &test_resolver(&host),
                test_timing(),
            )
            .await
            {
                Err(error) => error,
                Ok(_) => panic!("rejected response unexpectedly succeeded"),
            };
            assert!(matches!(error, ProvisionError::Permanent(_)));
            server.join().unwrap();
        }
    }

    #[tokio::test]
    async fn builtin_auth_retries_server_error_then_succeeds() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let host = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let replies = [
                "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n",
                "HTTP/1.1 204 No Content\r\nSet-Cookie: session=s\r\nContent-Length: 0\r\n\r\n",
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 35\r\n\r\n{\"device_code\":\"d\",\"user_code\":\"u\"}",
                "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n",
                "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: 33\r\n\r\n{\"error\":\"authorization_pending\"}",
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 20\r\n\r\n{\"access_token\":\"t\"}",
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 10\r\n\r\n{\"id\":\"u\"}",
            ];
            for reply in replies {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0; 4096];
                stream.read(&mut request).unwrap();
                stream.write_all(reply.as_bytes()).unwrap();
            }
        });
        let result = builtin_credentials(
            &VoxelConfig::default(),
            &host,
            &test_resolver(&host),
            test_timing(),
        )
        .await;
        server.join().unwrap();
        assert_eq!(result.unwrap().user_id, "u");
    }

    #[tokio::test]
    async fn malformed_response_error_redacts_body() {
        let secret = "distinctive-response-secret";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
            secret.len(),
            secret
        );
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let host = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 4096];
            stream.read(&mut request).unwrap();
            stream.write_all(response.as_bytes()).unwrap();
        });
        let error = match builtin_credentials(
            &VoxelConfig::default(),
            &host,
            &test_resolver(&host),
            test_timing(),
        )
        .await
        {
            Err(error) => error,
            Ok(_) => panic!("malformed response unexpectedly succeeded"),
        };
        assert!(matches!(error, ProvisionError::Permanent(_)));
        assert!(!format!("{error:#}").contains(secret));
        server.join().unwrap();
    }

    #[test]
    fn helper_timeout_is_bounded_without_exposing_output() {
        let root = tempfile::tempdir().unwrap();
        let helper = root.path().join("helper");
        fs::write(
            &helper,
            "#!/bin/sh\necho distinctive-helper-secret\nsleep 1\n",
        )
        .unwrap();
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o700))
            .unwrap();
        let error = run_helper(
            &VoxelConfig::default(),
            &helper,
            root.path(),
            "http://expected",
            &test_resolver("http://expected"),
            Duration::from_millis(1),
        )
        .unwrap_err();
        assert!(!format!("{error:#}").contains("distinctive-helper-secret"));
    }

    #[test]
    fn helper_nonzero_is_generic_and_redacted() {
        let root = tempfile::tempdir().unwrap();
        let helper = root.path().join("helper");
        fs::write(&helper, "#!/bin/sh\necho distinctive-secret >&2\nexit 7\n")
            .unwrap();
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o700))
            .unwrap();
        let error = run_helper(
            &VoxelConfig::default(),
            &helper,
            root.path(),
            "http://expected",
            &test_resolver("http://expected"),
            Duration::from_secs(1),
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Oxide auth helper exited unsuccessfully"
        );
        assert!(!format!("{error:#}").contains("distinctive-secret"));
    }

    #[tokio::test]
    async fn valid_helper_creates_exact_private_profile_and_authenticates() {
        let _lock =
            ENVIRONMENT.lock().unwrap_or_else(|error| error.into_inner());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let host = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 4096];
            let size = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..size]);
            assert!(request.starts_with("GET /v1/me "));
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer helper-token")
            );
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 15\r\n\r\n{\"id\":\"helper\"}").unwrap();
        });
        let root = tempfile::tempdir().unwrap();
        let directory = private_directory(root.path()).unwrap();
        let helper = root.path().join("helper");
        let script = "#!/bin/sh\nset -eu\n[ \"$#\" -eq 0 ]\n[ \"$HOME\" = \"$VOXEL_PERFTEST_OXIDE_CONFIG_DIR\" ]\n[ \"$VOXEL_PERFTEST_OXIDE_PROFILE\" = voxel-perftest ]\n[ \"$VOXEL_PERFTEST_OXIDE_RESOLVE\" = \"EXPECTED_RESOLVE\" ]\n[ \"$VOXEL_PERFTEST_RECOVERY_SILO\" = recovery ]\n[ \"$VOXEL_PERFTEST_RECOVERY_USER\" = recovery ]\n[ -z \"${OXIDE_HOST+x}${OXIDE_TOKEN+x}${OXIDE_PROFILE+x}\" ]\nprintf 'default-profile = \"voxel-perftest\"\\n' > \"$HOME/config.toml\"\nprintf '[profile.voxel-perftest]\\nhost = \"%s\"\\ntoken = \"helper-token\"\\nuser = \"helper\"\\ntoken_id = \"token-id\"\\ntime_expires = \"2099-01-01T00:00:00Z\"\\n' \"$VOXEL_PERFTEST_OXIDE_HOST\" > \"$HOME/credentials.toml\"\nchmod 600 \"$HOME/config.toml\" \"$HOME/credentials.toml\"\n"
            .replace("EXPECTED_RESOLVE", &test_resolver(&host).to_string());
        fs::write(&helper, script).unwrap();
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o700))
            .unwrap();
        let inherited = Path::new("inherited-secret");
        let _environment = EnvironmentGuard::set(&[
            ("OXIDE_HOST", inherited),
            ("OXIDE_TOKEN", inherited),
            ("OXIDE_PROFILE", inherited),
        ]);
        run_helper(
            &VoxelConfig::default(),
            &helper,
            directory.path(),
            &host,
            &test_resolver(&host),
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(
            fs::metadata(directory.path()).unwrap().permissions().mode()
                & 0o777,
            0o700
        );
        let profile = profile_at(directory.path()).unwrap();
        assert_eq!(profile.host, host);
        assert_eq!(profile.user, "helper");
        assert_eq!(profile.token_id.as_deref(), Some("token-id"));
        assert_eq!(
            profile.time_expires.as_deref(),
            Some("2099-01-01T00:00:00Z")
        );
        let resolver = test_resolver(&profile.host);
        let authenticated = current_user(
            &http_client(Duration::from_secs(1), &resolver).unwrap(),
            &profile.host,
            &profile.token,
            tokio::time::Instant::now() + Duration::from_secs(1),
            Duration::ZERO,
        )
        .await
        .unwrap();
        assert_eq!(authenticated, profile.user);
        server.join().unwrap();
    }

    struct EnvironmentGuard {
        saved: Vec<(&'static str, Option<OsString>)>,
    }

    impl EnvironmentGuard {
        fn set(values: &[(&'static str, &Path)]) -> Self {
            let saved = values
                .iter()
                .map(|(name, _)| (*name, std::env::var_os(name)))
                .collect();
            for (name, value) in values {
                unsafe { std::env::set_var(name, value) };
            }
            Self { saved }
        }
    }

    impl Drop for EnvironmentGuard {
        fn drop(&mut self) {
            for (name, value) in &self.saved {
                match value {
                    Some(value) => unsafe { std::env::set_var(name, value) },
                    None => unsafe { std::env::remove_var(name) },
                }
            }
        }
    }

    #[test]
    fn derives_exact_recovery_host() {
        assert_eq!(
            derived_host(&VoxelConfig::default()).unwrap(),
            "http://recovery.sys.rack1.oxide.test"
        );

        let mut cfg = VoxelConfig::default();
        cfg.network.dns_zone = "lab.example".into();
        assert_eq!(
            derived_host(&cfg).unwrap(),
            "http://recovery.sys.rack1.lab.example"
        );

        cfg.topology.racks = 2;
        assert_eq!(
            derived_host(&cfg).unwrap(),
            "http://recovery.sys.rack1.lab.example"
        );
    }

    #[tokio::test]
    async fn resolver_discovery_uses_rack1_authoritative_dns_and_first_a_record()
     {
        let _lock =
            ENVIRONMENT.lock().unwrap_or_else(|error| error.into_inner());
        let bin = tempfile::tempdir().unwrap();
        let script = bin.path().join("dig");
        fs::write(
            &script,
            "#!/bin/sh\nset -eu\n[ \"$#\" -eq 6 ]\n[ \"$1\" = +short ]\n[ \"$2\" = +timeout=3 ]\n[ \"$3\" = +tries=1 ]\n[ \"$4\" = @203.0.113.53 ]\n[ \"$5\" = recovery.sys.rack1.lab.example ]\n[ \"$6\" = A ]\nprintf 'diagnostic line\\n  192.0.2.24  \\n192.0.2.25\\n'\n",
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700))
            .unwrap();
        let _environment = EnvironmentGuard::set(&[("PATH", bin.path())]);
        let mut cfg = VoxelConfig::default();
        cfg.network.dns_zone = "lab.example".into();
        cfg.network.external_dns_ips = vec!["203.0.113.53".into()];
        let host = derived_host(&cfg).unwrap();

        let resolver =
            discover_resolve(&cfg, &host, Duration::ZERO).await.unwrap();

        assert_eq!(resolver.hostname, "recovery.sys.rack1.lab.example");
        assert_eq!(resolver.port, 80);
        assert_eq!(resolver.address, "192.0.2.24".parse::<Ipv4Addr>().unwrap());
        assert_eq!(
            resolver.to_string(),
            "recovery.sys.rack1.lab.example:80:192.0.2.24"
        );
    }

    #[tokio::test]
    async fn resolver_discovery_retries_then_rejects_output_without_a_valid_ipv4_record()
     {
        let _lock =
            ENVIRONMENT.lock().unwrap_or_else(|error| error.into_inner());
        let bin = tempfile::tempdir().unwrap();
        let script = bin.path().join("dig");
        fs::write(
            &script,
            "#!/bin/sh\nprintf 'distinctive-diagnostic\\n2001:db8::1\\n192.0.2.1 trailing\\n'\n",
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700))
            .unwrap();
        let _environment = EnvironmentGuard::set(&[("PATH", bin.path())]);

        let error = discover_resolve(
            &VoxelConfig::default(),
            "http://recovery.sys.rack1.oxide.test",
            Duration::ZERO,
        )
        .await
        .unwrap_err();
        let error = format!("{error:#}");

        assert!(error.contains("authoritative DNS 198.51.100.20"), "{error}");
        assert!(
            error.contains(
                "no valid A record for recovery.sys.rack1.oxide.test"
            ),
            "{error}"
        );
        assert!(!error.contains("distinctive-diagnostic"));
    }

    #[tokio::test]
    async fn resolver_discovery_retries_transient_dns_output_on_the_same_rack()
    {
        let _lock =
            ENVIRONMENT.lock().unwrap_or_else(|error| error.into_inner());
        let bin = tempfile::tempdir().unwrap();
        let count = bin.path().join("count");
        let script = bin.path().join("dig");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nif [ -f '{}' ]; then IFS= read -r count < '{}'; else count=0; fi\ncount=$((count + 1))\nprintf '%s' \"$count\" > '{}'\nif [ \"$count\" -eq 1 ]; then printf 'not-an-address\\n'; else printf '192.0.2.44\\n'; fi\n",
                count.display(),
                count.display(),
                count.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700))
            .unwrap();
        let _environment = EnvironmentGuard::set(&[("PATH", bin.path())]);

        let resolver = discover_resolve(
            &VoxelConfig::default(),
            "http://recovery.sys.rack1.oxide.test",
            Duration::ZERO,
        )
        .await
        .unwrap();

        assert_eq!(resolver.address, "192.0.2.44".parse::<Ipv4Addr>().unwrap());
        assert_eq!(fs::read_to_string(count).unwrap(), "2");
    }

    #[tokio::test]
    async fn resolver_discovery_classifies_missing_dns_configuration_as_permanent()
     {
        let mut cfg = VoxelConfig::default();
        cfg.network.external_dns_ips.clear();

        let error = discover_resolve(
            &cfg,
            "http://recovery.sys.rack1.oxide.test",
            Duration::ZERO,
        )
        .await
        .unwrap_err();

        assert!(matches!(error, ProvisionError::Permanent(_)));
        assert!(
            format!("{error:#}")
                .contains("no authoritative external DNS server")
        );
    }

    #[tokio::test]
    async fn http_client_resolves_the_profile_hostname_process_locally() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 4096];
            let size = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..size]);
            assert!(request.starts_with("GET /v1/me "));
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("host: unresolvable.invalid")
            );
            stream
                .write_all(
                    b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n",
                )
                .unwrap();
        });
        let host = format!("http://unresolvable.invalid:{port}");
        let resolver =
            OxideResolve::from_host(&host, "127.0.0.1".parse().unwrap())
                .unwrap();
        let client = http_client(Duration::from_secs(1), &resolver).unwrap();

        assert_eq!(
            client.get(format!("{host}/v1/me")).send().await.unwrap().status(),
            204
        );
        server.join().unwrap();
    }

    #[test]
    fn default_hash_passes_and_custom_hash_needs_helper() {
        let cfg = VoxelConfig::default();
        assert!(validate_config(&cfg, None).is_ok());
        let mut custom = cfg;
        custom.recovery_silo.user_password_hash = "custom".into();
        assert!(validate_config(&custom, None).is_err());
    }

    #[test]
    fn invalid_authoritative_dns_config_is_rejected_before_launch() {
        let mut cfg = VoxelConfig::default();
        cfg.network.external_dns_ips = vec!["not-an-ip-address".into()];

        let error = validate_config(&cfg, None).unwrap_err();

        assert!(error.to_string().contains(
            "rack 1 authoritative external DNS server is not a valid IP address"
        ));
    }

    #[test]
    fn stale_profile_directory_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join(format!("{PROFILE_PREFIX}stale")))
            .unwrap();
        assert!(
            static_preflight(&VoxelConfig::default(), None, root.path())
                .is_err()
        );
    }

    #[test]
    fn static_preflight_uses_version_subcommand() {
        assert_eq!(
            OXIDE_VERSION_ARGS,
            [
                "--timeout",
                "300",
                "--resolve",
                "example.invalid:80:127.0.0.1",
                "version"
            ]
        );
        let _lock =
            ENVIRONMENT.lock().unwrap_or_else(|error| error.into_inner());
        let root = tempfile::tempdir().unwrap();
        let bin = tempfile::tempdir().unwrap();
        let script = bin.path().join("oxide");
        fs::write(
            &script,
            "#!/bin/sh\n[ \"$#\" -eq 5 ] && [ \"$1\" = --timeout ] && [ \"$2\" = 300 ] && [ \"$3\" = --resolve ] && [ \"$4\" = example.invalid:80:127.0.0.1 ] && [ \"$5\" = version ] || exit 2\nprintf 'Oxide CLI 0.1.0\\nBuilt from commit: abc123\\nOxide API: 0.0.1\\n'\n",
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700))
            .unwrap();
        let dig = bin.path().join("dig");
        fs::write(&dig, "#!/bin/sh\n[ \"$#\" -eq 1 ] && [ \"$1\" = -v ]\n")
            .unwrap();
        fs::set_permissions(&dig, fs::Permissions::from_mode(0o700)).unwrap();
        let _environment = EnvironmentGuard::set(&[("PATH", bin.path())]);

        assert_eq!(
            static_preflight(&VoxelConfig::default(), None, root.path())
                .unwrap(),
            "Oxide CLI 0.1.0\nBuilt from commit: abc123\nOxide API: 0.0.1"
        );
    }

    #[test]
    fn static_preflight_rejects_an_unusable_dig_before_launch() {
        let _lock =
            ENVIRONMENT.lock().unwrap_or_else(|error| error.into_inner());
        let root = tempfile::tempdir().unwrap();
        let bin = tempfile::tempdir().unwrap();
        let oxide = bin.path().join("oxide");
        fs::write(&oxide, "#!/bin/sh\nprintf 'oxide test\\n'\n").unwrap();
        fs::set_permissions(&oxide, fs::Permissions::from_mode(0o700)).unwrap();
        let dig = bin.path().join("dig");
        fs::write(&dig, "#!/bin/sh\nprintf 'broken dig' >&2\nexit 2\n")
            .unwrap();
        fs::set_permissions(&dig, fs::Permissions::from_mode(0o700)).unwrap();
        let _environment = EnvironmentGuard::set(&[("PATH", bin.path())]);

        let error =
            static_preflight(&VoxelConfig::default(), None, root.path())
                .unwrap_err();

        assert!(error.to_string().contains("`dig -v` failed"));
        assert!(!format!("{error:#}").contains("broken dig"));
    }

    #[test]
    fn profile_files_are_private_and_exact() {
        let root = tempfile::tempdir().unwrap();
        write_profile(root.path(), "http://expected", "secret", "user-id")
            .unwrap();
        assert_eq!(
            fs::metadata(root.path().join("config.toml"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(root.path().join("credentials.toml"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        validate_profile(root.path(), "http://expected", "user-id").unwrap();
    }

    #[test]
    fn profile_rejects_extra_profile_host_and_user_mismatches() {
        for replacement in [
            "\n[profile.extra]\nhost='x'\ntoken='x'\nuser='x'\n",
            "host = \"http://wrong\"",
            "user = \"wrong\"",
        ] {
            let root = tempfile::tempdir().unwrap();
            write_profile(root.path(), "http://expected", "secret", "user-id")
                .unwrap();
            let path = root.path().join("credentials.toml");
            if replacement.starts_with('\n') {
                OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .unwrap()
                    .write_all(replacement.as_bytes())
                    .unwrap();
            } else {
                let original = fs::read_to_string(&path).unwrap();
                let key = replacement.split(" = ").next().unwrap();
                let changed =
                    original
                        .lines()
                        .map(|line| {
                            if line.starts_with(key) {
                                replacement
                            } else {
                                line
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                fs::write(&path, changed).unwrap();
            }
            assert!(
                validate_profile(root.path(), "http://expected", "user-id")
                    .is_err()
            );
        }
    }

    #[test]
    fn credentials_parse_error_does_not_expose_token() {
        let root = tempfile::tempdir().unwrap();
        write_profile(
            root.path(),
            "http://expected",
            "distinctive-secret-token",
            "user-id",
        )
        .unwrap();
        let path = root.path().join("credentials.toml");
        OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap()
            .write_all(b"unexpected = [")
            .unwrap();

        let error = validate_profile(root.path(), "http://expected", "user-id")
            .unwrap_err();
        assert!(!format!("{error:#}").contains("distinctive-secret-token"));
    }

    #[test]
    fn provision_populates_and_validates_private_directory() {
        let root = tempfile::tempdir().unwrap();
        let cfg = VoxelConfig::default();
        let session = OxideSession::provision_in(
            &cfg,
            OxideAuthProviderMetadata::Builtin,
            "oxide test".into(),
            test_resolver("http://recovery.sys.rack1.oxide.test"),
            root.path(),
            |directory| {
                assert_eq!(
                    fs::metadata(directory).unwrap().permissions().mode()
                        & 0o777,
                    0o700
                );
                write_profile(
                    directory,
                    "http://recovery.sys.rack1.oxide.test",
                    "secret",
                    "user-id",
                )?;
                Ok("user-id".into())
            },
        )
        .unwrap();
        assert_eq!(
            session.metadata().host,
            "http://recovery.sys.rack1.oxide.test"
        );
        assert_eq!(
            profile_at(session.directory().unwrap()).unwrap().user,
            "user-id"
        );
    }

    #[test]
    fn post_construction_provisioning_failure_closes_profile() {
        let root = tempfile::tempdir().unwrap();
        let session = test_session(root.path());
        let profile = session.directory().unwrap().to_path_buf();

        let error = match close_provisioning_failure::<OxideSession>(
            session,
            ProvisionError::Permanent(anyhow!(
                "post-construction validation failed"
            )),
        ) {
            Ok(_) => panic!(
                "post-construction validation failure must fail provisioning"
            ),
            Err(error) => error,
        };

        assert!(matches!(error, ProvisionError::Permanent(_)));
        assert!(!profile.exists());
    }

    #[test]
    fn provisioning_cleanup_failure_becomes_boundary_and_retains_both_errors() {
        let error = combine_provisioning_failure(
            ProvisionError::Transient(anyhow!("authentication failed")),
            Err(anyhow!("profile close failed")),
        );
        let ProvisionError::Boundary(error) = error else {
            panic!("profile cleanup failure must be a boundary failure");
        };
        let error = error.to_string();
        assert!(error.contains("authentication failed"), "{error}");
        assert!(error.contains("profile close failed"), "{error}");
    }

    #[test]
    fn api_request_uses_complete_known_working_post_argv() {
        let _lock =
            ENVIRONMENT.lock().unwrap_or_else(|error| error.into_inner());
        let root = tempfile::tempdir().unwrap();
        let bin = tempfile::tempdir().unwrap();
        let output = root.path().join("seen");
        let script = bin.path().join("oxide");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nIFS= read -r body || true\nprintf '%s' \"$body\" >> '{}'\n",
                output.display(),
                output.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700))
            .unwrap();
        let session = test_session(root.path());
        let _environment = EnvironmentGuard::set(&[("PATH", bin.path())]);

        session.api_request("/v1/disks", "POST", Some("request-body")).unwrap();

        let config_dir = session.directory().unwrap().display();
        assert_eq!(
            fs::read_to_string(output).unwrap(),
            format!(
                "--config-dir\n{config_dir}\n--profile\n{PROFILE}\n--timeout\n300\n--resolve\nexpected:80:127.0.0.1\napi\n/v1/disks\n--method\nPOST\n--header\ncontent-type:application/json\n--input\n-\nrequest-body"
            )
        );
    }

    #[test]
    fn api_request_without_body_omits_json_content_type() {
        let _lock =
            ENVIRONMENT.lock().unwrap_or_else(|error| error.into_inner());
        let root = tempfile::tempdir().unwrap();
        let bin = tempfile::tempdir().unwrap();
        let output = root.path().join("seen");
        let script = bin.path().join("oxide");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nprintf '{{}}'\n",
                output.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700))
            .unwrap();
        let session = test_session(root.path());
        let _environment = EnvironmentGuard::set(&[("PATH", bin.path())]);

        session.api_request("/v1/disks?project=probe", "GET", None).unwrap();

        let seen = fs::read_to_string(output).unwrap();
        assert!(!seen.contains("--header\n"));
        assert!(!seen.contains("content-type:application/json"));
        assert!(!seen.contains("--input\n"));
    }

    #[test]
    fn api_request_failure_identifies_the_operation_without_exposing_the_response()
     {
        let _lock =
            ENVIRONMENT.lock().unwrap_or_else(|error| error.into_inner());
        let root = tempfile::tempdir().unwrap();
        let bin = tempfile::tempdir().unwrap();
        let script = bin.path().join("oxide");
        fs::write(
            &script,
            "#!/bin/sh\nprintf 'error; status code: 400 Bad Request\\n{\"error_code\":\"InvalidRequest\",\"message\":\"distinctive-response-secret\",\"request_id\":\"3297dd70-d7b4-4270-8e4b-20d15072dcba\"}\\n'\nexit 1\n",
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700))
            .unwrap();
        let session = test_session(root.path());
        let _environment = EnvironmentGuard::set(&[("PATH", bin.path())]);

        let error = session
            .api_request("/v1/disks/owned?project=probe", "DELETE", None)
            .unwrap_err();
        let message = error.to_string();

        assert_eq!(error.kind, ApiErrorKind::ShapeRejected);
        assert!(message.contains("DELETE /v1/disks/owned?project=probe"));
        assert!(message.contains("HTTP 400"));
        assert!(message.contains("error_code InvalidRequest"));
        assert!(
            message.contains("request_id 3297dd70-d7b4-4270-8e4b-20d15072dcba")
        );
        assert!(!message.contains("distinctive-response-secret"));
    }

    #[test]
    fn api_request_non_utf8_output_identifies_the_operation() {
        let _lock =
            ENVIRONMENT.lock().unwrap_or_else(|error| error.into_inner());
        let root = tempfile::tempdir().unwrap();
        let bin = tempfile::tempdir().unwrap();
        let script = bin.path().join("oxide");
        fs::write(&script, "#!/bin/sh\nprintf '\\377'\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700))
            .unwrap();
        let session = test_session(root.path());
        let _environment = EnvironmentGuard::set(&[("PATH", bin.path())]);

        let error = session
            .api_request("/v1/disks?project=probe", "GET", None)
            .unwrap_err();
        let message = error.to_string();

        assert_eq!(error.kind, ApiErrorKind::Permanent);
        assert!(message.contains("GET /v1/disks?project=probe"));
        assert!(message.contains("output was not UTF-8"));
    }

    #[test]
    fn private_file_errors_do_not_expose_profile_directory() {
        let root = tempfile::tempdir().unwrap();
        let private = root.path().join("distinctive-private-profile-directory");
        fs::create_dir(&private).unwrap();
        let path = private.join("config.toml");
        fs::write(&path, "already exists").unwrap();

        let error = write_private(&path, "Oxide config", b"replacement")
            .unwrap_err()
            .to_string();
        assert!(!error.contains("distinctive-private-profile-directory"));
        assert!(error.contains("Oxide config"), "{error}");
    }

    #[test]
    fn command_is_explicit_and_removes_inherited_auth() {
        let _lock =
            ENVIRONMENT.lock().unwrap_or_else(|error| error.into_inner());
        let root = tempfile::tempdir().unwrap();
        let bin = tempfile::tempdir().unwrap();
        let output = root.path().join("seen");
        let script = bin.path().join("oxide");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nprintf '%s|%s|%s' \"${{OXIDE_HOST-unset}}\" \"${{OXIDE_TOKEN-unset}}\" \"${{OXIDE_PROFILE-unset}}\" >> '{}'\n",
                output.display(), output.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700))
            .unwrap();
        let session = test_session(root.path());
        let inherited = Path::new("inherited");
        let _environment = EnvironmentGuard::set(&[
            ("PATH", bin.path()),
            ("OXIDE_HOST", inherited),
            ("OXIDE_TOKEN", inherited),
            ("OXIDE_PROFILE", inherited),
        ]);
        let status = session.command().unwrap().status().unwrap();
        assert!(status.success());
        let seen = fs::read_to_string(output).unwrap();
        assert!(seen.contains("--config-dir\n"));
        assert!(seen.contains("--profile\nvoxel-perftest\n"));
        assert!(seen.contains("--timeout\n300\n"));
        assert!(seen.contains("--resolve\nexpected:80:127.0.0.1\n"));
        assert!(seen.ends_with("unset|unset|unset"));
    }

    fn test_session(temp_root: &Path) -> OxideSession {
        OxideSession {
            directory: Some(private_directory(temp_root).unwrap()),
            metadata: OxideSessionMetadata {
                profile: PROFILE.into(),
                host: "http://expected".into(),
                provider: OxideAuthProviderMetadata::Builtin,
                oxide_cli_version: "oxide test".into(),
            },
            resolver: test_resolver("http://expected"),
        }
    }
}
