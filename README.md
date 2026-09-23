# DataExplorer

A Rust CLI and Ratatui terminal workspace for **read-only Azure Data Explorer
(Kusto) queries**. One Cargo package shares configuration, authentication, REST
decoding, table views, and exports between both modes. Query files are submitted
as complete scripts, never split on semicolons.

## Install and authenticate

Requires a current stable Rust toolchain (Rust 1.98 was used for development),
Azure CLI **2.54+**, and access to your cluster/database:

```sh
cargo install --path .
az login --tenant YOUR_TENANT_ID
dataexplorer config init
dataexplorer clusters add dev https://YOUR_CLUSTER.REGION.kusto.windows.net \
  --database YOUR_DATABASE --tenant YOUR_TENANT_ID --default
dataexplorer clusters list
dataexplorer
```

Only the explicit Azure CLI credential is used. There is no implicit
environment/managed-identity fallback or interactive login from this program,
including in headless mode. `az account get-access-token --resource CLUSTER
--tenant TENANT` runs asynchronously without a shell. Tokens are cached **in
memory**, keyed by endpoint and tenant, and refreshed two minutes before their
`expires_on` timestamp. Azure CLI stdout/stderr is never logged. Install/login
failures are actionable errors. Without `--tenant`, Azure CLI's current tenant
is used. The SDK's `AzureCliCredential` was evaluated: its documented
non-caching subprocess behavior did not remove our timeout, cancellation, or
expiry-cache requirements, so this version invokes the CLI directly.

## Configuration

`dataexplorer config init` prints the configuration path. `--config PATH` selects
an explicit path (place this option before subcommands). The default uses the
OS configuration directory from `directories::ProjectDirs`:

- macOS: `~/Library/Application Support/com.dataexplorer.dataexplorer/config.toml`
- Linux: `$XDG_CONFIG_HOME/dataexplorer/config.toml` or `~/.config/dataexplorer/config.toml`
- Windows: the `dataexplorer` application directory under the user's roaming config directory.

Example configuration:

```toml
version = 1
default_cluster = "dev"

[defaults]
query_timeout_secs = 240
max_rows = 100000
max_bytes = 67108864

[clusters.dev]
endpoint = "https://YOUR_CLUSTER.REGION.kusto.windows.net"
database = "YOUR_DATABASE"
tenant = "YOUR_TENANT_ID"
auth = "azure-cli"
query_timeout_secs = 60

[language_server]
command = "kusto-lsp"
args = ["--stdio"]
```

Endpoint, database, and tenant overrides: `-c`, `-d`, `--tenant`. `-c` accepts an
alias or an explicit HTTPS origin. Timeout precedence is `--timeout`, cluster
timeout, global default. `--max-rows`/`--max-bytes` override global safety limits.
Endpoints cannot contain credentials, paths, queries, or fragments. No secrets
belong in this file. Unknown configuration fields and versions fail explicitly.
Only `auth = "azure-cli"` is supported.

`--force config init` can overwrite a configuration; `--force clusters add ...`
can replace an existing alias. Config writes are atomic. Pane sizes live in the
separate, atomically replaced `ui-state.toml` alongside the config. Query text
is not silently persisted.

## Headless scripts

```sh
dataexplorer -r query.kql -c dev -d YOUR_DATABASE --tenant YOUR_TENANT_ID \
  --format csv --output results.csv

dataexplorer -r query.kql -c https://YOUR_CLUSTER.REGION.kusto.windows.net \
  -d YOUR_DATABASE --format json

dataexplorer -r batch.kql -c dev --table 1 --format jsonl \
  --output rows.jsonl --force
```

`-r` never enters raw mode, constructs a TUI, or launches an LSP. Data alone goes
to stdout when no `--output` is given. Diagnostics and correlation IDs go to
stderr. Authentication/network failures are not retried blindly. Shell
redirection works normally; unlike `--output`, shell redirection cannot provide
the application's atomic-write/overwrite guarantees.

| Exit | Meaning |
| --- | --- |
| 0 | Successful execution/export, including explicitly accepted partial output |
| 2 | Arguments, config, target resolution, or ambiguous multi-table export |
| 3 | Azure CLI credential acquisition failure |
| 4 | Network, server, protocol, query, or terminal failure |
| 5 | Partial results refused |
| 6 | Query-file read or export I/O failure |
| 130 | Ctrl-C cancellation |

Multiple primary result tables are retained. CLI exports require `--table INDEX`
(zero-based) when more than one primary table exists: there is no implicit
table dropping. HTTP 200 is **not** proof of success: dataset completion and
query-completion tables are inspected. Server truncation/cancellation and local
row limits mark results **PARTIAL**. Export requires `--accept-partial`; the TUI
labels partial tables prominently. Malformed, interrupted, progressive, and
wire-byte-limit-exceeded responses are refused, not presented as complete.

`--timeout` is the server timeout in seconds (1–3600), with ten seconds of HTTP
transport grace. Credential acquisition has a separate 30-second bound.
`--max-rows` (1–10,000,000) limits total retained primary rows;
`--max-bytes` (1 KiB–1 GiB) limits the entire HTTP body. Server-side truncation
limits are also supplied; wire overhead means the local byte cap can be reached
before the server's data-size cap. No incomplete JSON body is salvaged.

Ctrl-C stops the local request and submits a separately bounded, best-effort
`.cancel query` for its client request ID. Server acceptance does **not**
prove that computation stopped before completion. Metadata commands are limited
to `.show databases` and `.show database schema as json` through the read-only
management endpoint; cancellation is the sole control-command exception.

## Terminal workspace

The header always shows the active target and running query's snapshotted target.
Result headers show the target that produced those results, even after switching
targets. Only one query runs at once; later edits do not change its script.
Asynchronous responses carry request IDs/document versions/view generations.
Network, credential acquisition, export, schema, file I/O, sorting, filtering,
and chart preparation do not run in the terminal input loop.

| Key | Action |
| --- | --- |
| Tab / Shift-Tab | Cycle cluster, editor, result focus |
| Ctrl-arrows / drag pane borders | Resize dividers |
| Alt-1 / Alt-2 / Alt-3 | Collapse/restore a pane |
| F9 | Maximize/restore focused pane |
| F5 / Ctrl-R | Run complete query |
| Ctrl-C | Cancel running query; otherwise editor copy |
| Ctrl-Q | Quit; modified text requires `quit --force` or save |
| F6 | Discover databases and refresh target schema |
| F7 | Chart/table toggle |
| F8 | Results/diagnostics toggle |
| Ctrl-Space / F2 | LSP completion / hover |
| Ctrl-P | Command prompt |
| Ctrl-O / Ctrl-S / Ctrl-E | Open / save / export prompts |
| F1 | Scrollable help |

Cluster pane: arrows select, Enter activates. A cluster without a configured
database discovers databases first; Enter on a database activates it.
`target` also supports unconfigured HTTPS endpoints.

Editor: multiline text, arrows, Shift-arrows selection, Ctrl-A select all,
Ctrl-Z undo, Ctrl-Y redo, Ctrl-X/C/V internal clipboard, bracketed paste.
The clipboard is internal to the editor, not an OS clipboard integration.
Tab is reserved for pane focus; use spaces or paste for indentation.
Text and LSP position conversion handle UTF-8/UTF-16, wide characters,
combining characters and tab stops.

Results: arrows scroll rows/columns; PgUp/PgDn move ten rows; `[`/`]` choose a
primary table. `s` toggles stable ascending/descending sort on the selected
column; `/` prompts for case-insensitive text filtering; `f` for a typed column
filter. Enter opens full cell detail, including nested dynamic data; arrows/page
keys scroll, Esc closes. Only visible rows/cell previews render each frame.
F8 exposes query/connection/LSP errors. The terminal restores raw mode, mouse
capture and alternate-screen state on normal exit, errors, and Rust panics.
The minimum useful size is 50×14; undersized terminals show a resize message.

Commands, entered with Ctrl-P (`:` also works outside the editor):

```text
target dev --database Logs --tenant YOUR_TENANT_ID
database Logs
open "/path/to/query file.kql"
open "/path/to/query file.kql" --force
save "/path/to/query file.kql" --force
filter warning
column 2 gt -1.25
column 0 contains customer
clear
export csv view "/path/to/output.csv"
export json all "/path/to/result.json" --force --accept-partial
quit --force
```

Column indices are zero-based. `eq`, `lt`, `gt`, `contains` are supported.
Integers/decimals compare exactly, not via f64; datetimes compare chronologically,
timespans by duration, booleans by value, strings/dynamic values by serialized
text. Null sorts first ascending and last descending; comparison filters follow
that same explicit ordering. Use `eq null` to select nulls. Unknown scalar types
remain intact and use textual comparison. The literal `null` in comparison
filters means a null cell; use `contains` to search for that text in strings.
Filters and sorts operate on **immutable fetched rows plus indices** and never
rewrite the server query. Invalid filters are reported and block view exports.

## Export semantics

TUI `all` and `view` refer to the **selected primary table**: all fetched rows in
source order, or the filtered/sorted local view. No chart downsampling or screen
truncation affects exports. Output files are written to a same-directory
temporary file, flushed/synced, then atomically persisted. Existing files are
refused unless `--force` is explicit, including in the TUI.

- **CSV:** schema column names as headers, RFC-style escaping, null as an empty
  field, dynamic values as JSON text. CSV inherently cannot distinguish null
  from an empty string or preserve scalar type metadata.
- **JSON:** object with format `version`, `columns` (`ColumnName`/`ColumnType`),
  positional `rows`, table ID/name, partial flag, diagnostics, correlation IDs,
  and visualization metadata. Original numeric representations, long precision,
  decimals (including server-provided strings), dynamic objects, and nulls are
  preserved.
- **JSONL:** one positional JSON array per row. This deliberately preserves
  duplicate column names instead of coercing rows to objects. Use JSON when
  schema/completion metadata must accompany the data.

## External language server

Install the separately developed **kusto-lsp** executable on PATH, or configure
an absolute executable path. A framework-dependent .NET installation can use:

```toml
[language_server]
command = "dotnet"
args = ["/absolute/path/KustoLsp.dll", "--stdio"]
```

The Rust application does **not** parse KQL for highlighting or diagnostics.
It uses standard stdio `Content-Length` JSON-RPC, full document sync, UTF-16
positions, the server's advertised semantic-token legend, versioned published
diagnostics, completion and hover. Missing/crashed/unresponsive services are
reported while the editor remains usable. Server stderr is drained but its
content is suppressed to avoid leaking document contents or credentials.
Pending requests fail on EOF/timeouts. Shutdown uses didClose/shutdown/exit,
with forced child termination as a fallback.

With `experimental.kustoSchemaVersion = 1`, authenticated metadata discovery
sends `kusto/setSchema` with `{uri, schema}`. The schema is
`{version:1, cluster, database, tables:[{name, columns:[{name,type}]}],
functions:[]}`. `schema:null` clears the previous target. No credentials are
sent to the language server. This version sends table/column schemas, not stored
function bodies. Offline parsing works without schema; semantic name checking
depends on successful schema discovery. F6 explicitly refreshes it.

## Charts and current limits

`render timechart`, `linechart`, `scatterchart` and horizontal `barchart` are
driven by the selected table's Kusto `Visualization` annotation, not by parsing
the query. X/Y columns, multiple numeric measures, explicit series groups,
titles and axis labels are used. Time axes are UTC. Horizontal bars use signed
floating-point coordinates, preserving negative/fractional values instead of
casting to unsigned integers. F7 always returns to the original table.

Unsupported visualization types, logarithmic/stacked/accumulated/split axes,
custom axis bounds, null/nonfinite coordinates, dynamic-array series, more than
32 series or 10,000 visible rows explain the refusal and retain the table.
Kusto's `"NaN"` default-bound sentinel is not treated as a custom bound.
No rows are silently sampled. Chart coordinates use f64 and may round very
large/exact decimals; use the table/export for original precision. Dense plots
can overlap at terminal resolution. Full GUI chart interaction and advanced
render options are not implemented.

Other first-version limits: nonprogressive responses only; no write/ingestion
commands, automatic retries, query-parameter UI, system clipboard, snippets or
completion additional edits. Query files opened in the editor are limited to
4 MiB. Stored-function schema transmission and progressive streaming are future
work, not placeholders in the query execution path.

## Development and validation

```sh
cargo fmt --all --check
cargo check --all-targets
cargo test
cargo clippy --all-targets -- -D warnings
cargo build
python3 tests/pty_smoke.py target/debug/dataexplorer
```

Tests cover V2/management frames, HTTP-200 errors, request serialization,
precision-preserving exports, typed stable views, config precedence/atomic
writes, visualization defaults, Unicode/LSP framing, version gating, and
representative TestBackend layouts. Three integration tests are ignored by default:

```sh
DATAEXPLORER_TEST_LSP=/absolute/path/kusto-lsp \
  cargo test external_server_interop -- --ignored

# Only run against an explicitly authorized cluster/database.
DATAEXPLORER_TEST_CLUSTER=https://YOUR_CLUSTER.REGION.kusto.windows.net \
DATAEXPLORER_TEST_DATABASE=YOUR_DATABASE \
  cargo test live_readonly_metadata_and_render -- --ignored

DATAEXPLORER_TEST_CLUSTER=https://YOUR_CLUSTER.REGION.kusto.windows.net \
DATAEXPLORER_TEST_DATABASE=YOUR_DATABASE \
  cargo test live_cli_exports_multiple_tables_partial_and_overwrite -- --ignored

# Optional real-terminal integration with live metadata and a tiny synthetic query.
DATAEXPLORER_TEST_CLUSTER=https://YOUR_CLUSTER.REGION.kusto.windows.net \
DATAEXPLORER_TEST_DATABASE=YOUR_DATABASE \
DATAEXPLORER_TEST_LSP=/absolute/path/kusto-lsp \
  python3 tests/pty_smoke.py target/debug/dataexplorer
```

The live tests read metadata and run tiny synthetic queries, including a syntax
error and explicit truncation; they do not read user-table data or mutate server
data. Export files live in temporary local directories. An optional
`DATAEXPLORER_TEST_TENANT` selects the tenant.

Protocol references:
[Kusto request](https://learn.microsoft.com/en-us/kusto/api/rest/request),
[V2 frames](https://learn.microsoft.com/en-us/kusto/api/rest/response-v2),
[render metadata](https://learn.microsoft.com/en-us/kusto/query/render-operator),
[AzureCliCredential](https://docs.rs/azure_identity/latest/azure_identity/struct.AzureCliCredential.html).
