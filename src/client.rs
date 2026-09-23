use crate::{
    config::Target,
    model::{QueryResult, Table, decode_mgmt, decode_v2},
};
use anyhow::{Context, Result, bail, ensure};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{process::Command, sync::Mutex};
use tokio_util::sync::CancellationToken;

struct Token {
    value: String,
    expires: u64,
}
type TokenCache = Arc<Mutex<HashMap<(String, Option<String>), Token>>>;

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    tokens: TokenCache,
}

#[derive(Debug)]
pub struct AuthError(pub String);
impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for AuthError {}
#[derive(Debug)]
pub struct Cancelled;
impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cancelled locally; server cancellation is best effort")
    }
}
impl std::error::Error for Cancelled {}

impl Client {
    pub fn new() -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .user_agent("dataexplorer/0.1")
                .build()?,
            tokens: Arc::new(Mutex::new(HashMap::new())),
        })
    }
    pub async fn token(&self, target: &Target) -> Result<String> {
        let key = (target.endpoint.clone(), target.tenant.clone());
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let mut cache = self.tokens.lock().await;
        if let Some(token) = cache.get(&key).filter(|token| token.expires > now + 120) {
            return Ok(token.value.clone());
        }
        let mut cmd = Command::new("az");
        cmd.args([
            "account",
            "get-access-token",
            "--resource",
            &target.endpoint,
            "--output",
            "json",
            "--only-show-errors",
        ])
        .kill_on_drop(true);
        if let Some(tenant) = &target.tenant {
            cmd.args(["--tenant", tenant]);
        }
        let output = tokio::time::timeout(Duration::from_secs(30), cmd.output())
            .await
            .map_err(|_| {
                AuthError("Azure CLI token acquisition timed out; run az login first".into())
            })?
            .map_err(|_| {
                AuthError(
                    "cannot execute Azure CLI; install az and run az login --tenant TENANT".into(),
                )
            })?;
        // Neither stdout nor stderr is included in errors: CLI output can contain credentials.
        if !output.status.success() {
            return Err(AuthError("Azure CLI authentication failed; run az login --tenant TENANT and verify database access".into()).into());
        }
        let value: Value = serde_json::from_slice(&output.stdout)
            .map_err(|_| AuthError("Azure CLI returned invalid token JSON".into()))?;
        let token = value["accessToken"]
            .as_str()
            .filter(|t| !t.is_empty())
            .ok_or_else(|| AuthError("Azure CLI did not return an access token".into()))?
            .to_owned();
        let expires = value["expires_on"]
            .as_u64()
            .or_else(|| value["expires_on"].as_str().and_then(|v| v.parse().ok()))
            .ok_or_else(|| {
                AuthError(
                    "Azure CLI must provide expires_on; upgrade Azure CLI to 2.54 or newer".into(),
                )
            })?;
        ensure!(
            expires > now + 60,
            AuthError("Azure CLI returned an expired token; run az login again".into())
        );
        cache.insert(
            key,
            Token {
                value: token.clone(),
                expires,
            },
        );
        Ok(token)
    }
    pub fn request_id() -> String {
        format!("DataExplorer.Query;{}", uuid::Uuid::new_v4())
    }
    pub fn body(target: &Target, query: &str, id: &str) -> Value {
        Self::body_with_parameters(target, query, id, &crate::query_library::Values::new())
    }
    pub fn body_with_parameters(
        target: &Target,
        query: &str,
        id: &str,
        parameters: &crate::query_library::Values,
    ) -> Value {
        let secs = target.limits.query_timeout_secs;
        json!({
            "db": target.database, "csl": query,
            "properties": json!({
                "ClientRequestId":id, "Options":{
                    "results_progressive_enabled":false,
                    "servertimeout":format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60),
                    "truncationmaxrecords":target.limits.max_rows,
                    "truncationmaxsize":target.limits.max_bytes
                }, "Parameters":parameters
            }).to_string()
        })
    }
    async fn post(
        &self,
        target: &Target,
        path: &str,
        query: &str,
        id: &str,
        token: &str,
        readonly: bool,
    ) -> Result<(Vec<u8>, Option<String>)> {
        self.post_body(
            target,
            path,
            id,
            token,
            readonly,
            &Self::body(target, query, id),
        )
        .await
    }
    async fn post_body(
        &self,
        target: &Target,
        path: &str,
        id: &str,
        token: &str,
        readonly: bool,
        body: &Value,
    ) -> Result<(Vec<u8>, Option<String>)> {
        let mut req = self
            .http
            .post(format!("{}{path}", target.endpoint))
            .bearer_auth(token)
            .header("Accept", "application/json")
            .header("x-ms-client-request-id", id)
            .header("x-ms-app", "DataExplorer")
            .timeout(Duration::from_secs(target.limits.query_timeout_secs + 10))
            .json(body);
        if readonly {
            req = req.header("x-ms-readonly", "true");
        }
        let response = req.send().await.context("Kusto HTTP request failed")?;
        let status = response.status();
        let activity = response
            .headers()
            .get("x-ms-activity-id")
            .and_then(|s| s.to_str().ok())
            .map(str::to_owned);
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("Kusto response interrupted")?;
            ensure!(
                bytes.len().saturating_add(chunk.len()) <= target.limits.max_bytes,
                "response exceeds local byte safety limit {}; no complete result retained (request {id})",
                target.limits.max_bytes
            );
            bytes.extend_from_slice(&chunk);
        }
        if !status.is_success() {
            let message = serde_json::from_slice::<Value>(&bytes)
                .ok()
                .and_then(|v| {
                    v["error"]["@message"]
                        .as_str()
                        .or(v["error"]["message"].as_str())
                        .map(str::to_owned)
                })
                .unwrap_or_else(|| "no structured error message".into());
            bail!(
                "Kusto HTTP {status}: {message} (request {id}, activity {})",
                activity.as_deref().unwrap_or("unavailable")
            );
        }
        Ok((bytes, activity))
    }
    pub async fn query(
        &self,
        target: &Target,
        query: &str,
        id: &str,
        cancel: CancellationToken,
    ) -> Result<QueryResult> {
        self.query_with_parameters(
            target,
            query,
            id,
            cancel,
            &crate::query_library::Values::new(),
        )
        .await
    }
    pub async fn query_with_parameters(
        &self,
        target: &Target,
        query: &str,
        id: &str,
        cancel: CancellationToken,
        parameters: &crate::query_library::Values,
    ) -> Result<QueryResult> {
        ensure!(!query.trim().is_empty(), "query is empty");
        let work = async {
            let token = self.token(target).await?;
            let (bytes, activity) = self
                .post_body(
                    target,
                    "/v2/rest/query",
                    id,
                    &token,
                    true,
                    &Self::body_with_parameters(target, query, id, parameters),
                )
                .await?;
            let max_rows = target.limits.max_rows;
            let mut result =
                tokio::task::spawn_blocking(move || decode_v2(&bytes, max_rows)).await??;
            result.client_request_id = id.into();
            result.activity_id = activity;
            Ok(result)
        };
        tokio::select! { result = work => result, _ = cancel.cancelled() => Err(Cancelled.into()) }
    }
    pub async fn cancel_server(&self, target: &Target, request_id: &str) -> Result<()> {
        ensure!(
            request_id.starts_with("DataExplorer.Query;")
                && request_id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | ';' | '-')),
            "invalid cancellation request ID"
        );
        tokio::time::timeout(Duration::from_secs(8), async {
            let token = self.token(target).await?;
            let csl = format!(".cancel query '{request_id}'");
            let (bytes, _) = self
                .post(
                    target,
                    "/v1/rest/mgmt",
                    &csl,
                    &Self::request_id(),
                    &token,
                    false,
                )
                .await?;
            decode_mgmt(&bytes)?;
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("server cancellation timed out (local request already stopped)")?
    }
    pub async fn metadata(&self, target: &Target, kind: Metadata) -> Result<Vec<Table>> {
        let query = match kind {
            Metadata::Databases => ".show databases",
            Metadata::Schema => ".show database schema as json",
        };
        let token = self.token(target).await?;
        let (bytes, _) = self
            .post(
                target,
                "/v1/rest/mgmt",
                query,
                &Self::request_id(),
                &token,
                true,
            )
            .await?;
        tokio::task::spawn_blocking(move || decode_mgmt(&bytes)).await?
    }
}
#[derive(Clone, Copy)]
pub enum Metadata {
    Databases,
    Schema,
}

pub fn normalize_schema(tables: &[Table], target: &Target) -> Result<Value> {
    let cell = tables
        .first()
        .and_then(|t| t.rows.first())
        .and_then(|r| r.first())
        .context("schema response is empty")?;
    let schema = crate::model::unpack(cell);
    let database = schema["Databases"]
        .get(&target.database)
        .context("database missing from returned schema")?;
    let map = database["Tables"]
        .as_object()
        .context("schema Tables missing")?;
    let mut normalized = Vec::new();
    for (name, table) in map {
        let columns = table["OrderedColumns"].as_array().context("schema OrderedColumns missing")?.iter().map(|col| {
            Ok(json!({"name":col["Name"].as_str().context("schema column Name missing")?,
                "type": col["CslType"].as_str().or(col["Type"].as_str()).context("schema column type missing")?}))
        }).collect::<Result<Vec<_>>>()?;
        normalized.push(json!({"name":name,"columns":columns}));
    }
    Ok(
        json!({"version":1,"cluster":target.endpoint,"database":target.database,"tables":normalized,"functions":[]}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parameters_are_separate_from_query_text() {
        let target = Target {
            label: "fixture".into(),
            endpoint: "https://example.invalid".into(),
            database: "db".into(),
            tenant: None,
            limits: Default::default(),
        };
        let query = "declare query_parameters(name:string); print name";
        let parameters =
            crate::query_library::Values::from([("name".into(), "'; print secret=1 //".into())]);
        let body = Client::body_with_parameters(&target, query, "fixture", &parameters);
        assert_eq!(body["csl"], query);
        let properties: Value = serde_json::from_str(body["properties"].as_str().unwrap()).unwrap();
        assert_eq!(properties["Parameters"]["name"], parameters["name"]);
        assert_eq!(properties["Options"]["results_progressive_enabled"], false);
    }
    use crate::config::Defaults;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method, path},
    };
    #[tokio::test]
    async fn wire_byte_limit_and_http_errors_are_explicit() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(" ".repeat(2048)))
            .mount(&server)
            .await;
        let target = Target {
            label: "fixture".into(),
            endpoint: server.uri(),
            database: "db".into(),
            tenant: None,
            limits: Defaults {
                max_bytes: 1024,
                ..Defaults::default()
            },
        };
        let client = Client::new().unwrap();
        client.tokens.lock().await.insert(
            (target.endpoint.clone(), None),
            Token {
                value: "fixture-token".into(),
                expires: u64::MAX,
            },
        );
        let error = client
            .query(&target, "print 1", "fixture-id", CancellationToken::new())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("byte safety limit"));
        server.reset().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(403)
                    .set_body_json(json!({"error":{"message":"Access denied"}})),
            )
            .mount(&server)
            .await;
        let error = client
            .query(&target, "print 1", "fixture-id", CancellationToken::new())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("403"));
        assert!(error.to_string().contains("Access denied"));
        assert!(!error.to_string().contains("fixture-token"));
    }
    #[tokio::test]
    async fn real_http_v2_and_request_contract() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v2/rest/query"))
            .and(header("x-ms-readonly", "true"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(crate::model::fixture(), "application/json")
                    .insert_header("x-ms-activity-id", "activity-1"),
            )
            .mount(&server)
            .await;
        let target = Target {
            label: "test".into(),
            endpoint: server.uri(),
            database: "db".into(),
            tenant: None,
            limits: Defaults::default(),
        };
        let client = Client::new().unwrap();
        client.tokens.lock().await.insert(
            (target.endpoint.clone(), None),
            Token {
                value: "fixture-token".into(),
                expires: u64::MAX,
            },
        );
        let result = client
            .query(
                &target,
                "let x=1; print x; print 2",
                "test-id",
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(result.activity_id.as_deref(), Some("activity-1"));
        let requests = server.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["csl"], "let x=1; print x; print 2");
        assert!(body["properties"].is_string());
        assert_eq!(
            serde_json::from_str::<Value>(body["properties"].as_str().unwrap()).unwrap()["Options"]
                ["results_progressive_enabled"],
            false
        );
    }
    #[tokio::test]
    async fn cancellation_and_200_error_propagation() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"error":{"message":"semantic failure"}})),
            )
            .mount(&server)
            .await;
        let target = Target {
            label: "test".into(),
            endpoint: server.uri(),
            database: "db".into(),
            tenant: None,
            limits: Defaults::default(),
        };
        let client = Client::new().unwrap();
        client.tokens.lock().await.insert(
            (target.endpoint.clone(), None),
            Token {
                value: "fixture-token".into(),
                expires: u64::MAX,
            },
        );
        assert!(
            client
                .query(&target, "print 1", "id", CancellationToken::new())
                .await
                .unwrap_err()
                .to_string()
                .contains("semantic failure")
        );
        let c = CancellationToken::new();
        c.cancel();
        assert!(client.query(&target, "print 1", "id", c).await.is_err());
    }

    #[tokio::test]
    #[ignore = "requires explicit live cluster/database and Azure CLI login"]
    async fn live_readonly_metadata_and_render() {
        let endpoint =
            std::env::var("DATAEXPLORER_TEST_CLUSTER").expect("explicit live cluster required");
        let database = std::env::var("DATAEXPLORER_TEST_DATABASE")
            .expect("explicit live test database required");
        let target = Target {
            label: "live-test".into(),
            endpoint: crate::config::endpoint(&endpoint).unwrap(),
            database,
            tenant: std::env::var("DATAEXPLORER_TEST_TENANT").ok(),
            limits: Defaults {
                query_timeout_secs: 30,
                ..Defaults::default()
            },
        };
        let client = Client::new().unwrap();
        let databases = client.metadata(&target, Metadata::Databases).await.unwrap();
        assert!(!databases.is_empty());
        let schema = client.metadata(&target, Metadata::Schema).await.unwrap();
        let normalized = normalize_schema(&schema, &target).unwrap();
        assert_eq!(normalized["version"], 1);
        assert!(
            normalized["tables"]
                .as_array()
                .is_some_and(|a| !a.is_empty())
        );
        let result = client.query(&target, "datatable(Label:string,Value:real)['negative',-1.25,'fraction',0.5] | render barchart with (xcolumn=Label, ycolumns=Value)", &Client::request_id(), CancellationToken::new()).await.unwrap();
        assert!(!result.partial, "live render query unexpectedly partial");
        let chart = crate::chart::prepare(&result.tables[0], &[0, 1]).unwrap_or_else(|e| {
            panic!("{e}; render metadata: {:?}", result.tables[0].visualization)
        });
        assert_eq!(chart.kind, crate::chart::ChartKind::Bar);
        assert_eq!(chart.series[0].points, [(0., -1.25), (1., 0.5)]);
        let error = client
            .query(
                &target,
                "print (",
                &Client::request_id(),
                CancellationToken::new(),
            )
            .await;
        assert!(
            error.is_err(),
            "live syntax error must not look like successful results"
        );
        let mut limited = target.clone();
        limited.limits.max_rows = 2;
        let token = client.token(&limited).await.unwrap();
        let (bytes, _) = client
            .post(
                &limited,
                "/v2/rest/query",
                "range x from 1 to 10 step 1",
                &Client::request_id(),
                &token,
                true,
            )
            .await
            .unwrap();
        let partial = decode_v2(&bytes, 2).unwrap_or_else(|e| {
            panic!(
                "synthetic truncation fixture: {e:#}\n{}",
                String::from_utf8_lossy(&bytes)
            )
        });
        assert!(partial.partial);
    }
}
