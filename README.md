# QuestDB Connector

Adds QuestDB connectivity as an installable connector extension.

This connector is listed in the public Irodori extension marketplace.

## Connector

- Extension ID: `irodori.questdb`
- Engine ID: `questdb`
- Wire: `postgres`
- Default port: `8812`
- Native ABI: `irodori.connector.native.v1`
- Driver linked: `true`

A desktop adapter source snapshot is staged in `native/source/` from `db/postgres.rs`.

Connector metadata lives in `connector.config.json` and `irodori.extension.json`.
The Rust code keeps native ABI exports in `src/lib.rs`, shared buffer/JSON helpers in `src/abi.rs`, and QuestDB PostgreSQL-wire behavior in `src/driver.rs`.

## Connection Metadata

- Endpoint modes: `hostPort`, `connectionString`
- Transport modes: `direct`, `sshTunnel`, `socks5Proxy`, `httpConnectProxy`, `proxyChain`
- TLS supported: `true`
- Custom driver options: `true`

| Auth method | Label | Secret purposes |
|---|---|---|
| `none` | No authentication | none |
| `connectionString` | Connection string / DSN | none |
| `userPassword` | User/password | `password` |
| `restToken` | REST / ILP token | `token` |
| `clientCertificate` | Client certificate / mTLS | `privateKey`, `privateKeyPassphrase` |
| `customDriverOptions` | Custom driver options | `password`, `token`, `privateKey`, `privateKeyPassphrase` |

## Experience Metadata

- Domains: `timeSeries`
- Result views: `timeChart`, `table`, `heatmap`
- Inspired by: `QuestDB Web Console`, `SAMPLE BY`, `LATEST ON`, `ASOF JOIN`

| Workflow | Result view | Templates |
|---|---|---|
| Sample by window | timeChart | time-questdb-sample-by |
| Latest per key | table | time-questdb-latest |
| As-of join | table | time-questdb-asof-join |

| Template | Label | Language | Result view |
|---|---|---|---|
| `time-questdb-sample-by` | SAMPLE BY aggregate | `sql` | `timeChart` |
| `time-questdb-latest` | LATEST ON per key | `sql` | `table` |
| `time-questdb-asof-join` | ASOF JOIN | `sql` | `table` |

## ABI Calls

The driver handles these JSON requests today:

| Method | Response |
|---|---|
| `health` / `ping` | Connector health, engine id, ABI version, and driver link status. |
| `describe` / `capabilities` | Embedded manifest and connector config. |
| `manifest` | Raw `irodori.extension.json`. |
| `config` | Raw `connector.config.json`. |
| `connect` | Opens a QuestDB PostgreSQL-wire connection and reads `select version()`. |
| `query` | Runs a SQL statement through the PostgreSQL wire protocol. |
| `metadata` | Reads table and column metadata from `information_schema`. |
| `close` | Removes the cached native connection. |

## Development


Generated extension repositories share `../target` across sibling repositories so Rust dependencies are compiled once per checkout. DuckDB and MotherDuck are driver-linked by default; set `IRODORI_CONNECTOR_LINK_DUCKDB=0` only when you need metadata-only DuckDB-compatible scaffolds.


```sh
make check
make build
```

Release packages place platform-specific native artifacts under `dist/native`.
