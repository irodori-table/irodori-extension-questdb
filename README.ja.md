<!-- i18n: language-switcher -->
[English](README.md) | [日本語](README.ja.md)

# QuestDB コネクタ

QuestDB 用のネイティブ Irodori テーブルコネクタ拡張です。

このクレートは、Irodori 拡張マーケットプレイスで使用されるコネクタのメタデータ、ネイティブ ABI エクスポート、およびドライバー実装をパッケージ化しています。

## コネクタ

- 拡張機能 ID: `irodori.questdb`
- エンジン ID: `questdb`
- ワイヤープロトコル: `postgres`
- デフォルトポート: `8812`
- ネイティブ ABI: `irodori.connector.native.v1`
- ドライバー連携: `あり`
- マーケットプレイス公開範囲: `公開`
- パッケージバージョン: `0.1.3`

パッケージには `db/postgres.rs` からのデスクトップアダプターのソーススナップショットが含まれています。

コネクタのメタデータは `connector.config.json` と `irodori.extension.json` にあります。
Rust クレートは `src/lib.rs` からネイティブ ABI をエクスポートし、共有の JSON/バッファヘルパーに `irodori-connector-abi` を使用し、コネクタの動作は `src/driver.rs` に保持しています。

## 接続メタデータ

- エンドポイントモード: `hostPort`, `connectionString`
- トランスポートモード: `direct`, `sshTunnel`, `socks5Proxy`, `httpConnectProxy`, `proxyChain`
- TLS 対応: `あり`
- デフォルトで TLS 必須: `いいえ`
- カスタムドライバーオプション: `あり`

### エンドポイントフィールド

| フィールド | ラベル | 型 | 必須 |
| --- | --- | --- | --- |
| `host` | ホスト | `string` | はい |
| `port` | ポート | `number` | いいえ |
| `database` | データベース | `string` | いいえ |

## 認証

コネクタはこれらの認証モードを広告し、クライアントが適切な認証情報フィールドを表示できるようにします。
ドライバー固有またはプロバイダー固有の値は必要に応じて `options` 経由で渡すことが可能です。

| 認証方式 | ラベル | 種類 | 秘密情報の用途 |
| --- | --- | --- | --- |
| `none` | 認証なし | `none` | なし |
| `connectionString` | 接続文字列 / DSN | `connectionString` | なし |
| `userPassword` | ユーザー/パスワード | `userPassword` | `password` |
| `restToken` | REST / ILP トークン | `token` | `token` |
| `clientCertificate` | クライアント証明書 / mTLS | `certificate` | `privateKey`, `privateKeyPassphrase` |
| `customDriverOptions` | カスタムドライバーオプション | `custom` | `password`, `token`, `privateKey`, `privateKeyPassphrase` |

## エクスペリエンスメタデータ

- ドメイン: `timeSeries`
- 結果ビュー: `timeChart`, `table`, `heatmap`
- オブジェクトタイプ: `tables`, `designatedTimestamps`, `symbols`, `partitions`, `walTables`
- インスパイア元: QuestDB Web Console、SAMPLE BY、LATEST ON、ASOF JOIN

| ワークフロー | 結果ビュー | テンプレート |
| --- | --- | --- |
| Sample by window | `timeChart` | `time-questdb-sample-by` |
| Latest per key | `table` | `time-questdb-latest` |
| As-of join | `table` | `time-questdb-asof-join` |

| テンプレート | ラベル | 言語 | 結果ビュー |
| --- | --- | --- | --- |
| `time-questdb-sample-by` | SAMPLE BY 集約 | `sql` | `timeChart` |
| `time-questdb-latest` | LATEST ON per key | `sql` | `table` |
| `time-questdb-asof-join` | ASOF JOIN | `sql` | `table` |

## ネイティブ ABI コール

| メソッド | レスポンス |
| --- | --- |
| `health` | コネクタのヘルス、エンジン ID、ABI バージョン、ドライバー状態を返します。 |
| `describe` | 埋め込みマニフェストとコネクタ設定を返します。 |
| `manifest` | 生の `irodori.extension.json` を返します。 |
| `config` | 生の `connector.config.json` を返します。 |
| `connect` | ネイティブコネクタ接続を開き、検証します。 |
| `query` | コネクタクエリを実行し、構造化された行または JSON 結果を返します。 |
| `metadata` | スキーマ、テーブル、カラム、インデックス、コレクション、または同等のメタデータを読み取ります。 |
| `close` | キャッシュされたネイティブ接続を閉じて削除します。 |

## 開発

このチェックアウト内のすべての拡張クレートは `../target` を共有しているため、依存関係は兄弟リポジトリ間で一度だけコンパイルされます。

```sh
make check
make build
```

リリースパッケージはプラットフォーム固有のネイティブアーティファクトを `dist/native` に配置します。

## ライセンス

0BSD。ほぼあらゆる目的でこのプロジェクトを使用、コピー、修正、配布できます。