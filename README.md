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

**To configure multiple clusters**, copy [`config.example.toml`](config.example.toml)
to a location you choose, edit each `[clusters.ALIAS]` section, then launch with
that file:

```sh
cp config.example.toml "$HOME/dataexplorer.toml"
# Edit $HOME/dataexplorer.toml: endpoints, databases, optional tenants and LSP path.
cargo run -- --config "$HOME/dataexplorer.toml"
# Select a different configured alias on launch:
cargo run -- --config "$HOME/dataexplorer.toml" -c production
# Verify the file's aliases without starting the TUI:
cargo run -- --config "$HOME/dataexplorer.toml" clusters list
```

Every configured alias and its default database appear in the explorer. `-c`
selects an initial target, **not** a configuration file. An explicit HTTPS
endpoint supplied with `-c` also appears in the explorer, but only for the current
session: it is not automatically saved as an alias. F6 / `metadata` discovers
additional databases. You can instead create and populate a file through CLI
commands:

```sh
cargo run -- --config "$HOME/dataexplorer.toml" config init
cargo run -- --config "$HOME/dataexplorer.toml" clusters add dev \
  https://YOUR_CLUSTER.REGION.kusto.windows.net --database Logs --default
cargo run -- --config "$HOME/dataexplorer.toml" clusters add production \
  https://OTHER_CLUSTER.REGION.kusto.windows.net --database ProductionLogs
```

Use either **copy/edit** or **init/add**, not both on the same existing file.
While in the TUI, **Ctrl-P → `setup` → Enter** shows the exact active config
path and language-server command, along with setup instructions. Configuration
is read at startup; restart after editing it.

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
| Ctrl-Q | Quit; prompts before discarding unsaved text in any tab |
| F6 | Discover databases and refresh target schema |
| F7 | Chart/table toggle |
| F8 | Results/diagnostics toggle |
| Ctrl-Space / F2 | LSP completion / hover |
| Ctrl-P | Fuzzy command palette with syntax, arguments and examples |
| Ctrl-O / Ctrl-L | Fuzzy query library / reload library |
| Ctrl-S | Save query with description and parameter prompts |
| Ctrl-N / Ctrl-W | New / close query tab (dirty tabs prompt before closing) |
| Alt-Left / Alt-Right | Previous / next query tab |
| Ctrl-PageUp / Ctrl-PageDown | Alternative previous / next tab bindings |
| F4 | Current tab's parameter definitions and execution values |
| Ctrl-E | Export result prompt |
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

### Query library, tabs, and documentation

Set a **top-level** `query_path` in configuration, before any `[section]`:

```toml
version = 1
query_path = "queries"
```

Relative paths resolve beside the selected configuration file, not the working
directory. Absolute paths also work; `~` is not expanded. At startup the app
loads `.kql`, `.csl`, and `.kusto` files recursively into an in-memory library.
It does **not** open every file as an editor tab. Nested relative paths remain
visible, so `production/users.kql` and `development/users.kql` are distinguishable.
**Ctrl-L** refreshes disk changes; successful saves refresh the library too.

**Ctrl-O** opens fuzzy search across relative paths, descriptions, and parameter
documentation. Up/Down highlights a query; its description and parameter types,
descriptions, and defaults appear in the preview. PageUp/PageDown scroll the
preview. Enter opens the query in a new tab, or focuses its existing tab without
replacing unsaved edits. The `open PATH` palette command still opens files
outside the library. Ctrl-S places saved queries in `query_path` when configured;
the `save PATH` palette command retains explicit-path saving (relative paths use
the launch directory). Without `query_path`, Ctrl-S uses the current file path
or asks for a new destination. Both routes prompt for documentation/parameters.
Existing plain KQL files are supported; leading `//` comments provide a preview.

Each tab retains its text, undo history, cursor/scroll position, dirty flag,
file path, and **in-memory parameter values**. Ctrl-N creates a tab, Alt-arrows
switch tabs, and Ctrl-W closes one. Closing a dirty tab offers Save, Discard,
or Cancel. Quitting checks *all* tabs, not just the visible one. Tabs and runtime
values are not restored across app restarts. Cluster/database selection and
results remain shared; result headers identify the query tab that produced them.
The LSP analyzes the active buffer, with a fresh version on every tab switch
so responses from another tab cannot alter its highlighting or diagnostics.

**Ctrl-S** starts the save wizard:

1. Enter a relative filename (for example `operations/users/recent.kql`) and a
   description. Tab/Shift-Tab changes fields; Ctrl-A selects the field text;
   Enter adds a newline. Ctrl-S continues.
2. Review/add parameter definitions. F3 adds one, Enter edits, Delete removes,
   and Ctrl-S continues, including when no parameters are needed.
3. Confirm the destination with Y, explicitly allowing replacement if it exists.
   Esc/N cancels. Saving creates nested directories and uses an atomic write.

Descriptions are required by the save wizard and stored as `///` documentation
comments. Definitions are stored in `/// @param` JSON comment lines and paired
with native `declare query_parameters(...)` statements inside a managed header:

```kusto
// <dataexplorer-query>
/// Find recent events for a region.
/// @param {"name":"region","type":"string","description":"Region to inspect","default":"west"}
/// @param {"name":"limit","type":"long","description":"Maximum records","default":"100"}
declare query_parameters(['region']:string = "west", ['limit']:long = long(100));
// </dataexplorer-query>
Events | where Region == region | take limit
```

Use the save wizard or F4 to edit this header: both update documentation and
declarations together, rather than duplicating them. The query body is retained.
If editing the header by hand, keep its definition comments and generated
declaration consistent; inconsistencies are reported rather than silently
rewritten. Ordinary, manually written KQL declarations are not automatically
imported into the parameter form.

Saving an open file refuses to overwrite changes made to it externally since it
was loaded/saved; save under a new name or close/reopen the file instead.
Saving over a file open in another tab is refused. Save completions are attached
to the originating tab even when you switch tabs during the write, and do not
clear later edits. Library saves reject absolute/traversing paths and symlink
components below the configured root. Scanning does not follow symlinks and
reports unreadable/invalid files. Limits are 4 MiB per query, 10,000 loaded files,
and 64 MiB of loaded query text; skipped files are reported, never silently hidden.
A missing directory is reported at startup and can be created by the first save.

### Native query parameters

Press **F4** (or `parameters` in the palette) to view and edit definitions and
execution values for the current tab. The same form appears during saving:

- **Definition:** name, Kusto type, description, and optional default.
- **Execution value:** an optional per-tab override, kept only in memory.

F3 adds a definition; Enter opens its fields. In a default/value field,
**F4 toggles unset vs set**, including an explicitly set empty string.
Typing also enables the field. Ctrl-S accepts the row, then Ctrl-S applies the
list. Esc cancels the current form without applying its edits.

Enter plain text for strings (no surrounding quotes), `true`/`false` for bool,
numbers for int/long/real/decimal, RFC3339 for datetime,
`[-][days.]hh:mm:ss[.fraction]` for timespan, UUID text for guid, and JSON for
dynamic. Dynamic parameters cannot have defaults in Kusto. Runtime values
override defaults; missing required values prevent execution and open the
parameter dialog. Defaults are persisted in the query, so **do not put secrets
in defaults**. Runtime values are never copied into query comments or declarations.
Changing only runtime values does not mark query text as unsaved.

Queries execute with values in the REST request's `properties.Parameters` bag,
not by textual substitution. Headless execution supports repeated `--param`:

```sh
dataexplorer -r queries/operations/users.kql -c dev \
  --param region=east --param limit=100
```

For managed query files, these values use the types in their documentation;
required values, names, duplicates, and formats are checked before authentication.
For ordinary query files with manually written declarations, `--param` values
are passed directly to Kusto: use plain strings for string parameters and Kusto
literals such as `datetime(2026-01-01)` or `dynamic({"k":1})` for other types.
No library scan or LSP process runs for headless execution.

Results: arrows scroll rows/columns; PgUp/PgDn move ten rows; `[`/`]` choose a
primary table. `s` toggles stable ascending/descending sort on the selected
column; `/` prompts for case-insensitive text filtering; `f` for a typed column
filter. Enter opens full cell detail, including nested dynamic data; arrows/page
keys scroll, Esc closes. Only visible rows/cell previews render each frame.
F8 exposes query/connection/LSP errors. The terminal restores raw mode, mouse
capture and alternate-screen state on normal exit, errors, and Rust panics.
The minimum useful size is 50×14; undersized terminals show a resize message.

### Command palette

**Ctrl-P** opens a popup listing all commands, with detailed help for the
highlighted item. Type a partial name or subsequence (`xpt` finds `export`);
**Up/Down** selects a match, and **Tab/Enter** picks it without executing it.
The search also matches words in command descriptions.

As soon as the first word is an exact command name (typed or picked), the popup
switches to that command's detailed syntax: required positional arguments,
options, valid values, safety rules, and examples. Type the arguments and press
**Enter** to run. **Up/Down/PageUp/PageDown** scroll help in this mode;
**Esc** closes the popup without running anything. Invalid arguments leave
the popup open with your input and an error so you can correct them.
You may paste a complete command and press Enter directly.

| Command | Purpose |
| --- | --- |
| `run` / `cancel` | Execute the whole editor / cancel the active query |
| `target` / `database` | Choose the query target |
| `metadata` | Discover databases and refresh LSP schema |
| `queries` / `reload-queries` | Browse or refresh the recursive query library |
| `new` / `close` / `next-tab` / `previous-tab` | Manage query tabs |
| `open` | Read a query file into a tab |
| `save-query` / `save PATH` | Library save wizard / explicit-path save wizard |
| `parameters` | Edit current tab's definitions and runtime values |
| `filter` / `column` / `clear` | Text filter / typed column filter / reset local view |
| `export` | Write the selected table's all/view rows |
| `chart` / `diagnostics` | Toggle result chart/table or diagnostics |
| `setup` / `help` | Config/LSP instructions or keyboard reference |
| `quit` | Exit, protecting unsaved text |

The palette runs **application commands**, not shell commands or KQL. Type KQL
in the query editor, then use `run` or F5. `config init` and `clusters add` are
CLI subcommands to run outside the TUI. Command arguments use shell-style quoting,
but no shell expansion occurs: use absolute paths rather than `~` in the palette.

Examples (`:` also opens the palette outside the editor):

```text
target dev --database Logs --tenant YOUR_TENANT_ID
run
metadata
database Logs
open "/path/to/query file.kql"
queries
new
parameters
save "team/query file.kql"
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

**No highlighting or query errors?** The language server is a separate executable;
`cargo run` does not install or discover a sibling LSP worktree automatically.
The query pane explicitly reports when it is unavailable. Open **Ctrl-P → setup**
to see the configured command and failure reason.

Install the separately developed **kusto-lsp** executable on PATH, or configure
an absolute executable path **in the same config file passed to `--config`**:

```toml
[language_server]
command = "/absolute/path/to/publish/osx-arm64/kusto-lsp"
args = ["--stdio"]
```

For a local sibling server build, use the full path to that build's published
`kusto-lsp` binary; for an installed server, `command = "kusto-lsp"` resolves it
from PATH. TOML paths do not expand `~` or environment variables. The path belongs
in `command`, and each argument is a separate item in `args`. Restart the TUI after
changing it. A framework-dependent .NET installation can instead use:

```toml
[language_server]
command = "dotnet"
args = ["/absolute/path/KustoLsp.dll", "--stdio"]
```

Once connected, semantic tokens color the editor and published diagnostics
underline affected text with a summary at the bottom of the **query pane**.
F8 / `diagnostics` shows all messages. Schema is fetched automatically for the
active target when the server starts, and F6 / `metadata` refreshes it. A valid
Azure CLI login is needed for schema discovery, not for offline syntax checking.
If the server is ready but table/column errors are absent, inspect the diagnostics
pane for a schema/authentication failure and run `metadata` after logging in.

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
depends on successful schema discovery. Initial connection fetches schema for the
active target; F6 explicitly refreshes it.

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
commands, automatic retries, system clipboard, snippets or
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
representative TestBackend layouts, recursive indexing, safe documented saves,
tab isolation, and parameter serialization. Four integration tests are ignored by default:

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

DATAEXPLORER_TEST_CLUSTER=https://YOUR_CLUSTER.REGION.kusto.windows.net \
DATAEXPLORER_TEST_DATABASE=YOUR_DATABASE \
  cargo test live_native_query_parameters -- --ignored

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
