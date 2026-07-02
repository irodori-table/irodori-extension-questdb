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
    redaction_values: Vec<String>,
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
    if config.tls {
        let connector = TlsConnector::builder()
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
    if bool_option(request, &["tls", "ssl"]).unwrap_or(false) {
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

fn request_containers(request: &Value) -> Vec<&Value> {
    [
        Some(request),
        request.get("profile"),
        request.get("options"),
        request.get("auth"),
        request.get("secrets"),
        request
            .get("profile")
            .and_then(|profile| profile.get("options")),
        request
            .get("profile")
            .and_then(|profile| profile.get("auth")),
        request
            .get("profile")
            .and_then(|profile| profile.get("secrets")),
    ]
    .into_iter()
    .flatten()
    .collect()
}

fn option_string(request: &Value, fields: &[&str]) -> Option<String> {
    request_containers(request)
        .into_iter()
        .find_map(|container| {
            fields.iter().find_map(|field| {
                container
                    .get(*field)
                    .map(|value| match value {
                        Value::String(value) => value.clone(),
                        Value::Number(value) => value.to_string(),
                        Value::Bool(value) => value.to_string(),
                        _ => String::new(),
                    })
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty())
            })
        })
}

fn bool_option(request: &Value, fields: &[&str]) -> Option<bool> {
    request_containers(request)
        .into_iter()
        .find_map(|container| {
            fields
                .iter()
                .find_map(|field| container.get(*field).and_then(Value::as_bool))
        })
}

fn push_sensitive(values: &mut Vec<String>, value: Option<&str>) {
    if let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) {
        if !values.iter().any(|existing| existing == value) {
            values.push(value.to_string());
        }
    }
}

fn collect_url_auth(url: &str, values: &mut Vec<String>) {
    let Some(after_scheme) = url.split_once("://").map(|(_, rest)| rest) else {
        return;
    };
    let Some(auth) = after_scheme
        .split('/')
        .next()
        .and_then(|host| host.split('@').next())
    else {
        return;
    };
    if auth.contains(':') {
        for part in auth.split(':') {
            push_sensitive(values, Some(part));
        }
    }
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
}
