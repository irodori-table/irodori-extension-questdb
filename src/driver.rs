use irodori_connector_abi::{collect_url_auth, option_bool, option_string, push_sensitive};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, OnceLock};

use base64::Engine;
use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use native_tls::TlsConnector;
use postgres::types::Type;
use postgres::{Client, NoTls, Row};
use postgres_native_tls::MakeTlsConnector;
use serde_json::{json, Map, Value};

use crate::abi::{self, IrodoriConnectorBuffer};
use crate::{ABI_VERSION, CONFIG_JSON, DRIVER_LINKED, ENGINE, MANIFEST_JSON};

static CONNECTIONS: OnceLock<Mutex<HashMap<String, QuestDbConnection>>> = OnceLock::new();

struct QuestDbConnection {
    client: Client,
    config: QuestDbConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct QuestDbConfig {
    conninfo: String,
    database: String,
    tls: bool,
    client_tls: ClientTls,
    redaction_values: Vec<String>,
}

/// Transport security beyond "use TLS", as `connector.config.json` declares it
/// under `clientCertificate`.
///
/// Paths, never key material: connector options persist to the workspace in the
/// clear, so the profile carries a path and the driver reads the file at
/// connect time.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct ClientTls {
    root_cert_path: Option<String>,
    client_cert_path: Option<String>,
    client_key_path: Option<String>,
}

impl ClientTls {
    fn from_request(request: &Value) -> Self {
        Self {
            root_cert_path: option_string(
                request,
                &["sslRootCert", "sslrootcert", "ssl-ca", "caCert"],
            ),
            client_cert_path: option_string(
                request,
                &["sslCert", "sslcert", "ssl-cert", "clientCert"],
            ),
            client_key_path: option_string(request, &["sslKey", "sslkey", "ssl-key", "clientKey"]),
        }
    }

    fn is_configured(&self) -> bool {
        self.root_cert_path.is_some()
            || self.client_cert_path.is_some()
            || self.client_key_path.is_some()
    }

    /// Apply the profile's certificates to a `native-tls` builder.
    ///
    /// `native-tls` takes the client identity as a PEM certificate and key
    /// pair, which is what every other tool asks for, so the two files go
    /// straight through.
    fn apply(&self, builder: &mut native_tls::TlsConnectorBuilder) -> Result<(), String> {
        if let Some(path) = &self.root_cert_path {
            let pem = read_pem(path, "SSL root certificate")?;
            let certificate = native_tls::Certificate::from_pem(&pem)
                .map_err(|err| format!("SSL root certificate at {path} is not valid PEM: {err}"))?;
            builder.add_root_certificate(certificate);
        }
        match (&self.client_cert_path, &self.client_key_path) {
            (Some(cert_path), Some(key_path)) => {
                let cert = read_pem(cert_path, "SSL client certificate")?;
                let key = read_pem(key_path, "SSL client key")?;
                require_pkcs8_key(&key, key_path)?;
                let identity = native_tls::Identity::from_pkcs8(&cert, &key)
                    .map_err(|err| format!("SSL client identity is not usable: {err}"))?;
                builder.identity(identity);
            }
            (Some(_), None) => {
                return Err("SSL client certificate needs a matching client key.".to_string())
            }
            (None, Some(_)) => {
                return Err("SSL client key needs a matching client certificate.".to_string())
            }
            (None, None) => {}
        }
        Ok(())
    }
}

fn read_pem(path: &str, label: &str) -> Result<Vec<u8>, String> {
    std::fs::read(path).map_err(|err| format!("{label} at {path} could not be read: {err}"))
}

/// `native-tls` only accepts a PKCS#8 client key, and only says so on some
/// platforms.
///
/// The Windows and macOS backends reject anything not starting with
/// `-----BEGIN PRIVATE KEY-----`; the OpenSSL backend is more permissive. A key
/// in the older PKCS#1 (`BEGIN RSA PRIVATE KEY`) or SEC1
/// (`BEGIN EC PRIVATE KEY`) form therefore works on Linux and fails on a
/// colleague's machine with a message that does not say why. Say why here, on
/// every platform, and give the command that fixes it.
fn require_pkcs8_key(key: &[u8], path: &str) -> Result<(), String> {
    if key.starts_with(b"-----BEGIN PRIVATE KEY-----") {
        return Ok(());
    }
    Err(format!(
        "SSL client key at {path} must be in PKCS#8 PEM form \
         (-----BEGIN PRIVATE KEY-----). Convert it with: \
         openssl pkcs8 -topk8 -nocrypt -in {path} -out client.pk8.pem"
    ))
}

#[derive(Default)]
struct ObjectMeta {
    kind: String,
    columns: Vec<Value>,
}

type QueryRows = Vec<Vec<Value>>;
type QueryOutput = (Vec<String>, QueryRows, bool);

fn connections() -> &'static Mutex<HashMap<String, QuestDbConnection>> {
    CONNECTIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn call_json(request: IrodoriConnectorBuffer) -> IrodoriConnectorBuffer {
    let request = match abi::parse_request(request) {
        Ok(request) => request,
        Err(response) => return response,
    };
    let method = match abi::request_method(request.as_ref()) {
        Ok(method) => method,
        Err(response) => return response,
    };

    match method {
        "health" | "ping" => abi::ok(Map::from_iter([
            ("engine".to_string(), Value::String(ENGINE.to_string())),
            ("abiVersion".to_string(), json!(ABI_VERSION)),
            ("driverLinked".to_string(), Value::Bool(DRIVER_LINKED)),
        ])),
        "describe" | "capabilities" => abi::ok(Map::from_iter([
            ("engine".to_string(), Value::String(ENGINE.to_string())),
            ("abiVersion".to_string(), json!(ABI_VERSION)),
            ("driverLinked".to_string(), Value::Bool(DRIVER_LINKED)),
            (
                "manifest".to_string(),
                serde_json::from_str(MANIFEST_JSON).unwrap_or(Value::Null),
            ),
            (
                "config".to_string(),
                serde_json::from_str(CONFIG_JSON).unwrap_or(Value::Null),
            ),
        ])),
        "manifest" => abi::owned_buffer(MANIFEST_JSON.to_string()),
        "config" => abi::owned_buffer(CONFIG_JSON.to_string()),
        "connect" => connect(request.as_ref().expect("connect has request")),
        "query" => query(request.as_ref().expect("query has request")),
        "metadata" => metadata(request.as_ref().expect("metadata has request")),
        "close" => close(request.as_ref().expect("close has request")),
        other => abi::error(
            "connector.unknownMethod",
            format!("unknown connector method: {other}"),
        ),
    }
}

fn connect(request: &Value) -> IrodoriConnectorBuffer {
    let connection_id = abi::connection_id(Some(request));
    let config = match QuestDbConfig::from_request(request) {
        Ok(config) => config,
        Err(err) => return abi::error("connector.invalidRequest", err),
    };
    let mut client = match connect_client(&config) {
        Ok(client) => client,
        Err(err) => return abi::error("connector.connectFailed", config.redact(&err)),
    };
    let version = match load_version(&mut client) {
        Ok(version) => version,
        Err(err) => return abi::error("connector.connectFailed", config.redact(&err)),
    };

    let mut guard = match connections().lock() {
        Ok(guard) => guard,
        Err(_) => {
            return abi::error(
                "connector.statePoisoned",
                "Connector connection state is poisoned.",
            )
        }
    };
    let response = Map::from_iter([
        ("engine".to_string(), Value::String(ENGINE.to_string())),
        (
            "connectionId".to_string(),
            Value::String(connection_id.clone()),
        ),
        ("driverLinked".to_string(), Value::Bool(DRIVER_LINKED)),
        (
            "database".to_string(),
            Value::String(config.database.clone()),
        ),
        ("serverVersion".to_string(), Value::String(version)),
    ]);
    guard.insert(connection_id, QuestDbConnection { client, config });
    abi::ok(response)
}

fn query(request: &Value) -> IrodoriConnectorBuffer {
    let connection_id = abi::connection_id(Some(request));
    let Some(sql) = abi::string_field(request, "sql")
        .or_else(|| abi::string_field(request, "query"))
        .or_else(|| abi::string_field(request, "statement"))
    else {
        return abi::error(
            "connector.invalidRequest",
            "query requires a string sql, query, or statement field.",
        );
    };
    let mut guard = match connections().lock() {
        Ok(guard) => guard,
        Err(_) => {
            return abi::error(
                "connector.statePoisoned",
                "Connector connection state is poisoned.",
            )
        }
    };
    let Some(connection) = guard.get_mut(&connection_id) else {
        return abi::error(
            "connector.connectionNotFound",
            format!("no open connection: {connection_id}"),
        );
    };
    match run_query(&mut connection.client, sql, abi::max_rows(request)) {
        Ok((columns, rows, truncated)) => abi::ok(Map::from_iter([
            ("connectionId".to_string(), Value::String(connection_id)),
            (
                "columns".to_string(),
                Value::Array(columns.into_iter().map(Value::String).collect()),
            ),
            (
                "rows".to_string(),
                Value::Array(rows.into_iter().map(Value::Array).collect()),
            ),
            ("truncated".to_string(), Value::Bool(truncated)),
        ])),
        Err(err) => abi::error("connector.queryFailed", connection.config.redact(&err)),
    }
}

fn metadata(request: &Value) -> IrodoriConnectorBuffer {
    let connection_id = abi::connection_id(Some(request));
    let mut guard = match connections().lock() {
        Ok(guard) => guard,
        Err(_) => {
            return abi::error(
                "connector.statePoisoned",
                "Connector connection state is poisoned.",
            )
        }
    };
    let Some(connection) = guard.get_mut(&connection_id) else {
        return abi::error(
            "connector.connectionNotFound",
            format!("no open connection: {connection_id}"),
        );
    };
    match load_metadata(&mut connection.client) {
        Ok(metadata) => abi::ok(Map::from_iter([
            ("connectionId".to_string(), Value::String(connection_id)),
            ("metadata".to_string(), metadata),
        ])),
        Err(err) => abi::error("connector.metadataFailed", connection.config.redact(&err)),
    }
}

fn close(request: &Value) -> IrodoriConnectorBuffer {
    let connection_id = abi::connection_id(Some(request));
    let mut guard = match connections().lock() {
        Ok(guard) => guard,
        Err(_) => {
            return abi::error(
                "connector.statePoisoned",
                "Connector connection state is poisoned.",
            )
        }
    };
    let existed = guard.remove(&connection_id).is_some();
    abi::ok(Map::from_iter([
        ("connectionId".to_string(), Value::String(connection_id)),
        ("closed".to_string(), Value::Bool(existed)),
    ]))
}

impl QuestDbConfig {
    fn from_request(request: &Value) -> Result<Self, String> {
        let database =
            option_string(request, &["database", "db"]).unwrap_or_else(|| "qdb".to_string());
        let tls = uses_tls(request);
        let conninfo = option_string(request, &["connectionString", "url", "dsn"])
            .unwrap_or_else(|| build_conninfo(request, &database, tls));
        let mut redaction_values = Vec::new();
        push_sensitive(
            &mut redaction_values,
            option_string(request, &["password", "token"]).as_deref(),
        );
        collect_url_auth(&conninfo, &mut redaction_values);
        Ok(Self {
            conninfo,
            database,
            tls: tls || conninfo_requests_tls(request),
            client_tls: ClientTls::from_request(request),
            redaction_values,
        })
    }

    fn redact(&self, message: &str) -> String {
        self.redaction_values.iter().fold(
            message.replace(&self.conninfo, "<questdb-conninfo>"),
            |message, secret| {
                if secret.is_empty() {
                    message
                } else {
                    message.replace(secret, "****")
                }
            },
        )
    }
}

fn connect_client(config: &QuestDbConfig) -> Result<Client, String> {
    // Supplying a certificate is a request for TLS: the material is unusable
    // over a plaintext connection, so honouring one without the other would
    // connect in a weaker mode than the user configured.
    if config.tls || config.client_tls.is_configured() {
        let mut builder = TlsConnector::builder();
        config.client_tls.apply(&mut builder)?;
        let connector = builder
            .build()
            .map_err(|err| format!("TLS setup failed: {err}"))?;
        let connector = MakeTlsConnector::new(connector);
        Client::connect(&config.conninfo, connector)
            .map_err(|err| format!("QuestDB PostgreSQL wire connect failed: {err}"))
    } else {
        Client::connect(&config.conninfo, NoTls)
            .map_err(|err| format!("QuestDB PostgreSQL wire connect failed: {err}"))
    }
}

fn load_version(client: &mut Client) -> Result<String, String> {
    let row = client
        .query_opt("select version()", &[])
        .map_err(|err| format!("version query failed: {err}"))?;
    Ok(row
        .as_ref()
        .and_then(|row| row.try_get::<usize, Option<String>>(0).ok().flatten())
        .map(|version| format!("QuestDB {version}"))
        .unwrap_or_else(|| "QuestDB".to_string()))
}

fn run_query(client: &mut Client, sql: &str, cap: usize) -> Result<QueryOutput, String> {
    let statement = client
        .prepare(sql)
        .map_err(|err| format!("prepare failed: {err}"))?;
    let columns = statement
        .columns()
        .iter()
        .map(|column| column.name().to_string())
        .collect::<Vec<_>>();
    let result_rows = client
        .query(&statement, &[])
        .map_err(|err| format!("query failed: {err}"))?;
    let truncated = result_rows.len() > cap;
    let rows = result_rows
        .iter()
        .take(cap)
        .map(|row| {
            row.columns()
                .iter()
                .enumerate()
                .map(|(index, _)| cell_to_json(row, index))
                .collect()
        })
        .collect();
    Ok((columns, rows, truncated))
}

fn load_metadata(client: &mut Client) -> Result<Value, String> {
    let mut schemas: BTreeMap<String, BTreeMap<String, ObjectMeta>> = BTreeMap::new();

    if let Ok(rows) = client.query(
        r#"
        select table_schema, table_name, table_type
        from information_schema.tables
        where table_schema not in ('pg_catalog', 'information_schema')
          and table_type in ('BASE TABLE', 'VIEW')
        order by table_schema, table_name
        "#,
        &[],
    ) {
        for row in rows {
            let schema = string_column(&row, "table_schema").unwrap_or_else(|| "public".into());
            let name = string_column(&row, "table_name").unwrap_or_default();
            if name.is_empty() {
                continue;
            }
            let table_type = string_column(&row, "table_type").unwrap_or_default();
            let object = schemas.entry(schema).or_default().entry(name).or_default();
            object.kind = if table_type.eq_ignore_ascii_case("VIEW") {
                "view".to_string()
            } else {
                "table".to_string()
            };
        }
    }

    let column_rows = client
        .query(
            r#"
            select table_schema, table_name, column_name, data_type, is_nullable,
                   ordinal_position, column_default
            from information_schema.columns
            where table_schema not in ('pg_catalog', 'information_schema')
            order by table_schema, table_name, ordinal_position
            "#,
            &[],
        )
        .map_err(|err| format!("metadata columns failed: {err}"))?;

    for row in column_rows {
        let schema = string_column(&row, "table_schema").unwrap_or_else(|| "public".into());
        let table = string_column(&row, "table_name").unwrap_or_default();
        if table.is_empty() {
            continue;
        }
        let object = schemas
            .entry(schema)
            .or_default()
            .entry(table)
            .or_insert_with(|| ObjectMeta {
                kind: "table".to_string(),
                columns: Vec::new(),
            });
        let nullable = string_column(&row, "is_nullable")
            .map(|value| value.eq_ignore_ascii_case("YES"))
            .unwrap_or(true);
        let ordinal = row
            .try_get::<&str, Option<i32>>("ordinal_position")
            .ok()
            .flatten()
            .unwrap_or((object.columns.len() + 1) as i32);
        object.columns.push(json!({
            "name": string_column(&row, "column_name").unwrap_or_default(),
            "dataType": string_column(&row, "data_type").unwrap_or_default(),
            "nullable": nullable,
            "ordinal": ordinal,
            "defaultValue": string_column(&row, "column_default")
        }));
    }

    Ok(json!({
        "schemas": schemas
            .into_iter()
            .map(|(schema, objects)| json!({
                "name": schema,
                "objects": objects
                    .into_iter()
                    .map(|(name, object)| json!({
                        "schema": schema,
                        "name": name,
                        "kind": if object.kind.is_empty() { "table" } else { &object.kind },
                        "columns": object.columns,
                        "indexes": [],
                        "primaryKey": [],
                        "foreignKeys": []
                    }))
                    .collect::<Vec<_>>()
            }))
            .collect::<Vec<_>>()
    }))
}

fn cell_to_json(row: &Row, index: usize) -> Value {
    let ty = row.columns()[index].type_();
    match *ty {
        Type::BOOL => optional_cell::<bool, _>(row, index, Value::Bool),
        Type::INT2 => optional_cell::<i16, _>(row, index, |value| json!(value)),
        Type::INT4 => optional_cell::<i32, _>(row, index, |value| json!(value)),
        Type::INT8 => optional_cell::<i64, _>(row, index, |value| json!(value)),
        Type::FLOAT4 => optional_cell::<f32, _>(row, index, |value| json!(value)),
        Type::FLOAT8 => optional_cell::<f64, _>(row, index, |value| json!(value)),
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME | Type::UNKNOWN => {
            optional_cell::<String, _>(row, index, Value::String)
        }
        Type::JSON | Type::JSONB => optional_cell::<Value, _>(row, index, |value| value),
        Type::TIMESTAMP => {
            optional_cell::<NaiveDateTime, _>(row, index, |value| Value::String(value.to_string()))
        }
        Type::TIMESTAMPTZ => {
            optional_cell::<DateTime<Utc>, _>(row, index, |value| Value::String(value.to_rfc3339()))
        }
        Type::DATE => {
            optional_cell::<NaiveDate, _>(row, index, |value| Value::String(value.to_string()))
        }
        Type::TIME => {
            optional_cell::<NaiveTime, _>(row, index, |value| Value::String(value.to_string()))
        }
        Type::UUID => {
            optional_cell::<uuid::Uuid, _>(row, index, |value| Value::String(value.to_string()))
        }
        Type::BYTEA => optional_cell::<Vec<u8>, _>(row, index, |value| {
            Value::String(base64::engine::general_purpose::STANDARD.encode(value))
        }),
        _ => fallback_cell(row, index),
    }
}

fn optional_cell<T, F>(row: &Row, index: usize, convert: F) -> Value
where
    T: postgres::types::FromSqlOwned,
    F: FnOnce(T) -> Value,
{
    match row.try_get::<usize, Option<T>>(index) {
        Ok(Some(value)) => convert(value),
        Ok(None) => Value::Null,
        Err(_) => fallback_cell(row, index),
    }
}

fn fallback_cell(row: &Row, index: usize) -> Value {
    if let Ok(value) = row.try_get::<usize, Option<String>>(index) {
        return value.map(Value::String).unwrap_or(Value::Null);
    }
    if let Ok(value) = row.try_get::<usize, Option<i64>>(index) {
        return value.map(|value| json!(value)).unwrap_or(Value::Null);
    }
    if let Ok(value) = row.try_get::<usize, Option<f64>>(index) {
        return value.map(|value| json!(value)).unwrap_or(Value::Null);
    }
    if let Ok(value) = row.try_get::<usize, Option<bool>>(index) {
        return value.map(Value::Bool).unwrap_or(Value::Null);
    }
    Value::Null
}

fn string_column(row: &Row, column: &str) -> Option<String> {
    row.try_get::<&str, Option<String>>(column)
        .ok()
        .flatten()
        .filter(|value| !value.trim().is_empty())
}

fn build_conninfo(request: &Value, database: &str, tls: bool) -> String {
    let host = option_string(request, &["host", "endpoint"]).unwrap_or_else(|| "127.0.0.1".into());
    let port = option_string(request, &["port"]).unwrap_or_else(|| "8812".into());
    let user = option_string(request, &["user", "username"]).unwrap_or_else(|| "admin".into());
    let password = option_string(request, &["password", "token"]);
    let mut parts = vec![
        format!("host={}", conninfo_value(&host)),
        format!("port={}", conninfo_value(&port)),
        format!("user={}", conninfo_value(&user)),
        format!("dbname={}", conninfo_value(database)),
    ];
    if let Some(password) = password {
        parts.push(format!("password={}", conninfo_value(&password)));
    }
    if tls {
        parts.push("sslmode=require".to_string());
    }
    parts.join(" ")
}

fn conninfo_value(value: &str) -> String {
    if value
        .chars()
        .any(|ch| ch.is_whitespace() || ch == '\'' || ch == '\\')
    {
        format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
    } else {
        value.to_string()
    }
}

fn uses_tls(request: &Value) -> bool {
    if option_bool(request, &["tls", "ssl"]).unwrap_or(false) {
        return true;
    }
    option_string(request, &["sslmode", "tlsMode"])
        .map(|mode| !matches!(mode.as_str(), "disable" | "disabled" | "false" | "none"))
        .unwrap_or(false)
}

fn conninfo_requests_tls(request: &Value) -> bool {
    option_string(request, &["connectionString", "url", "dsn"])
        .map(|value| {
            let lower = value.to_ascii_lowercase();
            lower.contains("sslmode=require")
                || lower.contains("sslmode=verify-ca")
                || lower.contains("sslmode=verify-full")
                || lower.contains("sslmode=prefer")
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_keyword_conninfo_from_profile_fields() {
        let request = json!({
            "profile": {
                "host": "localhost",
                "port": 8812,
                "database": "qdb",
                "user": "admin",
                "password": "quest",
                "tls": true
            }
        });
        let config = QuestDbConfig::from_request(&request).unwrap();
        assert_eq!(
            config.conninfo,
            "host=localhost port=8812 user=admin dbname=qdb password=quest sslmode=require"
        );
        assert!(config.tls);
    }

    #[test]
    fn quotes_keyword_conninfo_values() {
        assert_eq!(conninfo_value("simple"), "simple");
        assert_eq!(conninfo_value("has space"), "'has space'");
        assert_eq!(conninfo_value("has'quote"), "'has\\'quote'");
    }

    #[test]
    fn reads_the_client_tls_paths_from_the_connector_options() {
        let tls = ClientTls::from_request(&json!({
            "profile": { "options": {
                "sslRootCert": "/etc/ssl/ca.pem",
                "sslCert": "/etc/ssl/client.pem",
                "sslKey": "/etc/ssl/client.key"
            } }
        }));
        assert_eq!(tls.root_cert_path.as_deref(), Some("/etc/ssl/ca.pem"));
        assert_eq!(tls.client_cert_path.as_deref(), Some("/etc/ssl/client.pem"));
        assert_eq!(tls.client_key_path.as_deref(), Some("/etc/ssl/client.key"));
        assert!(tls.is_configured());
    }

    #[test]
    fn accepts_the_driver_spellings_too() {
        let tls = ClientTls::from_request(&json!({
            "profile": { "options": { "sslrootcert": "/ca.pem", "ssl-cert": "/c.pem" } }
        }));
        assert_eq!(tls.root_cert_path.as_deref(), Some("/ca.pem"));
        assert_eq!(tls.client_cert_path.as_deref(), Some("/c.pem"));
    }

    #[test]
    fn a_profile_without_certificates_is_not_configured() {
        let tls = ClientTls::from_request(&json!({ "profile": {} }));
        assert_eq!(tls, ClientTls::default());
        assert!(!tls.is_configured());
        let mut builder = native_tls::TlsConnector::builder();
        assert!(tls.apply(&mut builder).is_ok());
    }

    #[test]
    fn half_a_client_identity_is_rejected() {
        // Connecting without the certificate the user asked for is worse than
        // refusing: it succeeds in a weaker mode than they configured.
        let mut builder = native_tls::TlsConnector::builder();
        let cert_only = ClientTls {
            client_cert_path: Some("/etc/ssl/client.pem".into()),
            ..ClientTls::default()
        };
        assert_eq!(
            cert_only.apply(&mut builder).unwrap_err(),
            "SSL client certificate needs a matching client key."
        );

        let key_only = ClientTls {
            client_key_path: Some("/etc/ssl/client.key".into()),
            ..ClientTls::default()
        };
        assert_eq!(
            key_only.apply(&mut builder).unwrap_err(),
            "SSL client key needs a matching client certificate."
        );
    }

    #[test]
    fn an_unreadable_certificate_names_the_file_and_the_field() {
        let mut builder = native_tls::TlsConnector::builder();
        let tls = ClientTls {
            root_cert_path: Some("/definitely/not/here.pem".into()),
            ..ClientTls::default()
        };
        let err = tls.apply(&mut builder).unwrap_err();
        assert!(
            err.starts_with("SSL root certificate at /definitely/not/here.pem"),
            "{err}"
        );
    }

    #[test]
    fn a_certificate_that_is_not_pem_is_rejected() {
        let dir = std::env::temp_dir().join("irodori-questdb-tls-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("not-a-cert.pem");
        std::fs::write(&path, b"this is not a certificate").unwrap();

        let mut builder = native_tls::TlsConnector::builder();
        let tls = ClientTls {
            root_cert_path: Some(path.to_string_lossy().into_owned()),
            ..ClientTls::default()
        };
        assert!(tls.apply(&mut builder).is_err());

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_pkcs1_client_key_is_rejected_with_the_conversion_command() {
        // native-tls accepts only PKCS#8, and only the Windows and macOS
        // backends say so — on Linux an older key shape fails later and less
        // clearly, or not at all until a colleague tries it.
        let err = require_pkcs8_key(
            b"-----BEGIN RSA PRIVATE KEY-----\nMII...\n-----END RSA PRIVATE KEY-----\n",
            "/etc/ssl/client.key",
        )
        .unwrap_err();
        assert!(err.contains("PKCS#8"), "{err}");
        assert!(err.contains("openssl pkcs8 -topk8"), "{err}");

        assert!(require_pkcs8_key(b"-----BEGIN PRIVATE KEY-----\nMII...\n", "/k").is_ok());
    }
}
