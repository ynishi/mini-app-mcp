# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **S3-compatible snapshot upload (opt-in `s3-upload` build feature)** — `data_snapshot` accepts `upload: true` and pushes each written snapshot to an S3-protocol destination (AWS S3 / Backblaze B2 S3-Compatible API / R2 / MinIO) selected by `MINI_APP_S3_ENDPOINT`. Configuration is environment-only (`MINI_APP_S3_*`, rides `.mini-app-mcp.env`); validated before any snapshot is written (`UPLOAD_NOT_CONFIGURED`), per-table upload failures are non-fatal and reported in `upload_errors[]` (`uploaded[]` on success). Remote retention is intentionally out of scope — use bucket lifecycle rules. Default build carries zero new dependencies. `MINI_APP_S3_REGION` is optional: unset, it is derived from `s3.<region>.<domain>` endpoints (B2 / AWS regional; verified against a real B2 bucket), with `us-east-1` as the fallback for hosts without an embedded region.
- **Scheduled backups** — `contrib/backup/mini-app-backup.sh`: cron-friendly client that calls `data_snapshot(upload=true)` on a running `--mcp-http` daemon over streamable HTTP and exits non-zero on tool errors or non-empty `upload_errors[]` (deps: `curl`, `jq`). Bundled in the container image as `mini-app-backup` together with supercronic v0.2.49; the new entrypoint wrapper (`contrib/docker/entrypoint.sh`) runs it on the `BACKUP_CRON` schedule alongside the server (inert when unset).
- **Fly.io hosting** — `contrib/fly/fly.toml` (single machine + volume, edge TLS, bearer auth, daily backup cron) and `docs/runbooks/fly-io-deploy.md` (app/volume/secrets setup, deploy, smoke, data migration from a local `~/.mini-app/`, client registration).

### Changed

- **Container image now builds with `--features s3-upload`** and includes `ca-certificates` / `curl` / `jq`; `ENTRYPOINT` is the `mini-app-entrypoint` wrapper, which is argument-transparent to the previous `mini-app-mcp` entrypoint when `BACKUP_CRON` is unset.

### Deprecated

### Removed

### Fixed

### Security

## [0.17.0] - 2026-08-13

### Added

- **Streamable HTTP transport (multi-device mode)** — `--mcp-http` flag + `--bind` (default `127.0.0.1:8484`) serves MCP over streamable HTTP at `/mcp`. One central daemon owns the SQLite files; any number of devices connect as remote MCP clients, so the single-writer storage model is preserved with no cross-device sync. Bearer auth via `MINI_APP_HTTP_TOKEN`; non-loopback binds are refused at startup without a token. stdio mode (`--mcp`) unchanged.
- Daemon templates under `contrib/`: systemd user unit (Linux) and launchd user agent (macOS).
- `docs/runbooks/multi-device-smoke.md` — 8-item smoke test runbook for the HTTP transport (verified on Linux and macOS).

### Changed

- rmcp 1.5 → 1.8 (enables `transport-streamable-http-server`); axum 0.8 added for the HTTP listener.

### Deprecated

### Removed

### Fixed

- **Empty daemon logs** — the tracing subscriber was never initialized, so `tracing::info!` output went nowhere. Logs now go to stderr (stdout stays the MCP protocol channel in stdio mode), default level `info` with `RUST_LOG` override, ANSI disabled for non-tty writers (launchd/systemd log files).

### Security

## [0.16.0] - 2026-07-05

### Added

### Changed

- **rusqlite 0.32 → 0.37 (libsqlite3-sys 0.30 → 0.35)** (`crates/core/Cargo.toml` / `crates/mcp/Cargo.toml`) — aligns with the `libsqlite3-sys 0.35` cluster used by ai-store-sqlite 0.7 / rusqlite-isle 0.4, so downstream projects that combine mini-app-core with crates on the 0.35 band resolve without the `links = "sqlite3"` conflict. Cross-ref: ynishi/journal-mcp#1.
- `rusqlite::DatabaseName::Main` → `rusqlite::MAIN_DB` (`crates/core/src/backup.rs` / `crates/core/src/snapshot.rs`) — follows the rusqlite 0.37 API change that replaced the `DatabaseName` enum with the `Name` trait + `MAIN_DB` constant.

### Deprecated

### Removed

### Fixed

- **Dockerfile build context** — `COPY src ./src` predated the workspace split and broke the image build (no top-level `src/`; the `include_str!` target `crates/mcp/QUICKSTART.md` was outside the build context). Now copies `crates/` and builds `--package mini-app-mcp`.
- **server.json version drift** — `version` / `packages[].identifier` were stuck at 0.9.0 while the crates advanced; re-synced with the workspace version.

### Security

## [0.15.0] - 2026-06-26

### Added

- **`_row_history` table + auto history log** (`crates/core/src/row_history.rs` — new module) — populated atomically inside `Store::create` / `update` / `delete` Tx. Stores full row JSON (post-state + pre-state) so any past row state can be reconstructed. Schema-side backup pairs are untouched.
- **`row_restore(table, id, at_unix_secs)` MCP tool** (`crates/mcp/src/mcp/server.rs`) — locates the latest history entry with `recorded_at <= at_unix_secs` and replays it: existing rows are replaced, deleted rows are re-inserted under the same id. The restore itself is recorded as a new history version, so it remains reversible.
- **`purge_old_history(retention_days, max_per_row)`** (`crates/core/src/row_history.rs`) — modeled on `backup::purge_old_backups` for retention control. Default scope keeps full JSON per version; jsonpatch diff form carried to a later phase.
- 5 row_history e2e tests (`crates/core/tests/row_history_e2e.rs`) — multi-update fetch_at / restore round-trip / delete re-insert / purge bounds / Tx rollback atomicity.
- **3 partial-edit MCP tools** (`crates/mcp/src/mcp/server.rs`) — `content_view` / `content_replace` / `content_insert`. Anthropic `str_replace_based_edit_tool` 互換の緩 form で string field の partial edit を提供する。 row_history v0.14.0 を backstop として「事故を起こさせない IF」 を構造化:
  - `content_view(table, id, field, view_range?=[s,e])` — cat -n 形式の line# 付き出力。 1-indexed inclusive、 range 省略 = 全文。
  - `content_replace(table, id, field, old_str, new_str, view_range?, replace_all?=false)` — default unique 強制、 0 match → STRING_NOT_FOUND、 複数 match → AMBIGUOUS_MATCH error + line# candidates (最大 20 件、 真の match 数は `matches` field で別途返却)。 view_range で絞り込み、 replace_all で一括置換。 hint 文字列に「Pass view_range=[start,end] to scope, or replace_all=true to batch.」 を含める。
  - `content_insert(table, id, field, line, content)` — `line=0` 先頭挿入、 `line=N` で N 行目の後。 末尾追加対応 (`line=total`)。 line > total は OUT_OF_RANGE。
  - 3 tool すべて `Store::update(mode=replace)` 経路を通過するため、 row_history hook で自動記録される (別 hook 追加なし)。
  - string field 専任 (Number / Array / Object は TYPE_ERROR で reject、 `actual_type` を error data に含める)。
  - unique 検査 + 適用は同一 Store Tx 内 atomic。
- **3 internal store APIs** (`crates/core/src/store.rs`) — `view_string_field` / `replace_string_field` / `insert_into_string_field` + `ReplaceResult { matches: u32 }` 型。 上記 MCP tool の backend。
- **4 new error codes + variants** (`crates/core/src/error.rs`) — `AmbiguousMatch { matches, candidates }` / `StringNotFound` / `OutOfRange { line, total_lines }` / `TypeError { field, actual_type }` を `MiniAppError` enum に追加。 `MatchCandidate { line: u32, col: u32, snippet: String }` struct も同 file に新設。
- 13 new e2e tests (`crates/mcp/tests/e2e_mcp.rs`) — view 全文 / view_range / out_of_range、 replace unique pass / 0-match / ambiguous (candidates 検証) / view_range scoping / replace_all count、 insert line=0 / line=last / out_of_range、 TYPE_ERROR (Array field)。
- 5 hardening regression tests (holistic-reviewer Auto-Fix 由来) — inverted view_range guard / off-by-one (`line > total` reject) / AMBIGUOUS candidates cap (20 件) / empty old_str reject / UTF-8 char boundary safe snippet。

### Changed

- `format_partial_edit_error` (`crates/mcp/src/mcp/server.rs`) — `MiniAppError::Validation` arm を追加し、 empty old_str 等の VALIDATION_ERROR が `error: "VALIDATION"` で正しく serialize されるよう修正。 AMBIGUOUS hint 文字列は `matches > 20` (CAND_CAP 超過) 時に「First 20 of {N} candidates shown.」 prefix を付ける form に拡張。

## [0.14.0] - 2026-06-21

### Added

- **`ListFilter::ArrayContains` / `ArrayNotContains` primitives** (`crates/core/src/filter.rs`) — server-side filtering for array-typed schema fields. Wire form `{"type": "array_contains", "field": "<name>", "value": <scalar>}` (and `array_not_contains` sibling). SQL generated as `EXISTS (SELECT 1 FROM json_each(json_extract(data, '$.<field>')) WHERE value = ?)` (and `NOT EXISTS` for the negative variant). Validation rejects non-array fields with a `MiniAppError::Validation` mirroring the existing `Like`/`String` guard pattern. Note: `ArrayNotContains` matches rows where `<field>` is absent or NULL (SQLite `json_each` returns an empty set → `NOT EXISTS` is true) — documented in `FILTERS_DOC`.
- New private helper `validate_array_element_scalar` (`crates/core/src/filter.rs`) — scalar guard (string/number/boolean accept; null/object/array reject) without field-type matching, used by the two new variants.
- 8 new unit tests in `crates/core/src/filter.rs` covering: ok / unknown field reject / non-array field reject / null value reject / object value reject / `ArrayContains` build_sql / `ArrayNotContains` build_sql / serde roundtrip.
- **`FILTERS_DOC` documentation** (`crates/mcp/src/mcp/resources.rs`) — `## \`ArrayContains\`` and `## \`ArrayNotContains\`` sections inserted between `Like` and `Or`, plus a usage line added to the Composition example. The self-test `filters_doc_contains_all_filter_types` literal list extended accordingly.
- **`order_by` primitive** (`crates/core/src/order_by.rs` — new module) — server-side multi-key ORDER BY for `list` / `alias_create` / `alias_run`. `Vec<OrderByItem>` where each item is `{field, direction: "asc" | "desc"}`. SQL generated as `ORDER BY json_extract(data, '$.<field>') <DIR>, ...` with the direction as an enum-to-literal token (SQL-injection safe; field names schema-whitelisted). Validation rejects unknown fields and empty arrays.
- **`list` accepts `order_by`** (`crates/core/src/store.rs` — `Store::list` 4th positional argument; `crates/mcp/src/mcp/server.rs` — `ListParams.order_by`). When supplied, the default `ORDER BY created_at DESC` is replaced; when omitted the legacy default is preserved (backward-compatible).
- **`alias_create` persists `order_by`** (`crates/mcp/src/mcp/server.rs` — `AliasCreateParams.order_by`; `crates/core/src/alias_storage.rs` — `_global_aliases.order_by TEXT` column at index 8). Idempotent migration via `PRAGMA table_info` + conditional `ALTER TABLE ADD COLUMN` on every `open_scope_db`, mirroring the v0.13.0 `fields` column pattern. Existing `_global.db` files (including v0.13.0 fields-only DBs) are upgraded in place with `order_by = NULL` (no behaviour change for pre-existing aliases). `migrate_from_per_table` inserts `NULL` for both `fields` and `order_by` so legacy per-table aliases survive intact. Legacy single-table mode rejects `order_by` explicitly (per-table `_aliases` table does not carry the column).
- **`alias_run` runtime override + stored fallback** (`crates/mcp/src/mcp/server.rs` — `AliasRunParams.order_by`; `crates/core/src/alias_run.rs` — `execute_alias_run` 8th positional). Same two-stage fallback shape as the v0.13.0 `fields` flow: explicit runtime override wins; otherwise the stored `order_by` from the alias record is deserialised and applied.
- **`docs://order-by` MCP resource** (`crates/mcp/src/mcp/resources.rs` — new `ORDER_BY_DOC` const) — documents syntax, single / multi-key examples, default behaviour, storage as alias, and the aggregator-path limitation (aggregator paths currently log a `tracing::warn!` and skip `order_by`; post-aggregation sort is the aggregator's own responsibility).
- New tests: `order_by` unit tests (10) + alias-storage round-trip / v0.13.0 fields-only DB idempotent-migration regression / aggregator-path warn-and-skip / `ORDER_BY_DOC` self-test.

### Changed

- **`AliasRecord::new` signature gains a 9th positional parameter** (`crates/core/src/alias_storage.rs`) — `order_by: Option<String>` appended after `fields`. All existing call sites updated to pass `None` or the serialized `Vec<OrderByItem>` JSON. (`#[allow(clippy::too_many_arguments)]` retained.)
- **`execute_alias_run` signature gains an 8th positional parameter** (`crates/core/src/alias_run.rs`) — `order_by_override: Option<Vec<OrderByItem>>` appended after `fields`.
- **`Store::list` signature gains a 4th positional parameter** (`crates/core/src/store.rs`) — `order_by: Option<Vec<OrderByItem>>` appended after `filter`. All ~30 call sites (production + tests across `mini-app-core` and `mini-app-mcp`) updated to pass `None`.
- **`alias_record_to_json` now exposes `fields` and `order_by`** (`crates/mcp/src/mcp/server.rs`) — fixes a v0.13.0 oversight where the stored `fields` projection was not visible via `alias_list`. Same fix carries `order_by` so the two stay together going forward.
- **`list` / `alias_create` / `alias_run` tool descriptions + `ListParams.filter` / `AliasCreateParams.filter` / `AliasAggregateParams.filter` doc comments** (`crates/mcp/src/mcp/server.rs`) — filter primitive enumeration normalised to `Eq / In / Like / ArrayContains / ArrayNotContains / Or / And` (the previous wording omitted `Like`; fixed in the same commit for consistency); `order_by` mentioned in the three tool descriptions.

### Deprecated

### Removed

### Fixed

### Security

## [0.13.0] - 2026-06-17

### Added

- **`AliasCreateParams.fields` parameter** (`crates/mcp/src/mcp/server.rs`) — `tool_alias_create` accepts an optional `fields: FieldSelector` argument. The selector is serialized to JSON and persisted in the new `_global_aliases.fields` column, then applied as the default projection when `alias_run` is invoked without a run-time `fields` override. Callers can now register an alias once with `fields={"mode":"list","fields":[...]}` and re-run it without re-specifying the projection each call.
- **`_global_aliases.fields` column** (`crates/core/src/alias_storage.rs`) — new `TEXT` column appended at index 7. Idempotent migration via `PRAGMA table_info` + conditional `ALTER TABLE ADD COLUMN` runs on every `open_scope_db`, so existing `_global.db` files are upgraded in place with `fields = NULL` (no behavioral change for pre-existing aliases).
- **`FieldSelector` derives `Serialize`** (`crates/core/src/materialize.rs`) — symmetric with the existing `Deserialize` derive. No wire shape change.
- 2 new e2e tests (`crates/mcp/tests/e2e_mcp.rs`):
  - `alias_create_with_fields_then_alias_run_uses_stored_fields` — stored `fields` applies when run-time argument is omitted.
  - `alias_create_without_fields_alias_run_no_projection` — NULL regression guard (NULL stored `fields` ≠ empty projection list; full rows return when neither stored nor run-time `fields` is set).

### Changed

- **`AliasRecord::new` signature gains an 8th positional parameter** (`crates/core/src/alias_storage.rs`) — `fields: Option<String>` appended at the end. All existing call sites updated to pass `None` or the serialized `FieldSelector` JSON.
- **`execute_alias_run` falls back to `record.fields`** (`crates/core/src/alias_run.rs`) — when the caller does not pass a run-time `fields` argument, the persisted alias projection is now applied. `None` stored value preserves the legacy "no projection / full rows" behavior; only an explicit `Some` projection is applied.

### Deprecated

### Removed

### Fixed

- **Legacy single-table `alias_create` path now rejects `fields` explicitly** (`crates/mcp/src/mcp/server.rs`) — when the per-table `_aliases` storage path is taken and the caller passes `fields`, the handler returns a clear actionable error instead of silently dropping the projection.

### Security

## [0.12.1] - 2026-06-17

### Added

- **`AliasCreateParams.scope` parameter** (`crates/mcp/src/mcp/server.rs`) — `tool_alias_create` accepts an optional `scope: "project" | "user"` argument. When omitted, the server selects `Project` if that scope is mounted (legacy backward-compatible default) and falls back to `User` otherwise. This lets callers (e.g. `persona-wire` Adapters consuming `mini-app://<table>?alias=<name>` URIs) explicitly target the User scope without setting `MINI_APP_PROJECT_DIR`, and lets `alias_create` succeed in the common Claude Code default env where Project scope unmounts because the CWD has no `.mini-app/` directory.
- **`AliasScope` derives `Deserialize` / `Serialize` / `JsonSchema`** (`crates/core/src/alias_storage.rs`) — the SDK enum is now wire-representable as `"project"` / `"user"` (lowercase) so MCP tool layers can accept it as a JSON parameter. Pure additive derive (`Debug` / `Clone` / `Copy` / `PartialEq` / `Eq` preserved).
- 4 new unit tests for the user-only mount environment (`crates/mcp/src/mcp/server.rs`):
  - `alias_create_user_only_mount_default_scope_falls_back_to_user`
  - `alias_create_user_only_mount_explicit_user_scope_succeeds`
  - `alias_create_user_only_mount_explicit_project_scope_returns_clear_error`
  - `alias_delete_user_only_mount_round_trip`

### Changed

- **`tool_alias_create` dispatch is no longer hardcoded to `AliasScope::Project`** (`crates/mcp/src/mcp/server.rs:1648` previously) — the handler now picks the target scope from `params.scope`, falling back to a runtime check (`global.path_for_scope(Project).is_some() ? Project : User`) when the caller omits the field. When the caller explicitly requests a scope that is not mounted, the handler surfaces a clear, actionable error mentioning both `MINI_APP_PROJECT_DIR` and `MINI_APP_USER_DIR` instead of the generic storage-layer "scope is not mounted" config error.
- **`tool_alias_delete` dispatch handles single-scope mounts symmetrically** (`crates/mcp/src/mcp/server.rs:1830` previously) — when Project scope is mounted the existing Project-first / User-on-NotFound precedence is preserved (mirrors `alias_get`). When Project scope is unmounted the handler dispatches directly to User scope so the delete cannot fail with a Project-scope config error.

### Fixed

- **`alias_create` 5/5 fail with "GlobalAliasStorage scope Project is not mounted" in Claude Code default env** (`crates/mcp/src/mcp/server.rs`) — Claude Code launches `mini-app-mcp` from arbitrary consumer CWDs that often have no `.mini-app/` directory. `Config::load` resolves `MINI_APP_PROJECT_DIR` default to `./.mini-app/` (CWD-relative), but `TableRegistry::mount_from_dirs` at `registry.rs:109-110` filters non-existent directories out of `GlobalAliasStorage::open`, leaving only the User scope mounted. The pre-v0.12.1 `tool_alias_create` hardcoded `AliasScope::Project` and unconditionally failed in this single-scope environment, even though sibling tools (`list` / `alias_get` / `alias_list`) are scope-agnostic and fall back to User scope transparently. The new caller-supplied `scope` parameter plus runtime auto-fallback restores symmetry with the read-side tools and unblocks downstream consumers like `persona-wire` Adapters that integrate `mini-app-core 0.12.0`'s `execute_alias_run` path. The underlying SDK contract (`GlobalAliasStorage::open(project_dir, user_dir)` accepting `Option<&Path>` for either scope, `alias_create(scope, record)` accepting any mounted `AliasScope`) was already correct at the storage layer; only the MCP handler hardcode needed removing.

### Carry (separate issue)

- **SDK DomainEntity Layer purification** (`mini-app-core` v0.13.0 minor refactor, breaking) — `Config::load()` (env::var + dotenvy + dirs::home_dir 直読み), `dump.rs:85` `std::env::current_dir()`, and the `dotenvy` / `dirs` crate dependencies will be removed from `mini-app-core` and re-located to `mini-app-mcp` as an `env_loader` module. The SDK invariant "mini-app-core does not read env / FS / process state" will be documented in `core/src/lib.rs` and `CONTRIBUTING.md`. Trigger: pair-programming session.

## [0.12.0] - 2026-06-16

### Added

- **`mini_app_core::alias_run` module** (`crates/core/src/alias_run.rs`) — top-level orchestration for `alias_run` exposed as a single Core SDK entry point. `execute_alias_run(registry, record, params, table_fallback, limit_override, offset, fields)` accepts an `AliasRecord` and dispatches across the MiniJinja render → filter parse → source resolve → aggregator / rows path pipeline, returning a new `AliasRunValue` enum (`Rows(Vec<Record>)` / `Aggregate(AliasRunResult)`). SDK consumers can now invoke the full alias_run pipeline without re-implementing the ~120-line orchestration that previously lived in the MCP tool handler.

### Changed

- **`tool_alias_run` (MCP handler) is now a thin wrapper** (`crates/mcp/src/mcp/server.rs`) — orchestration body (~120 lines) was extracted to `mini_app_core::alias_run::execute_alias_run`. The MCP handler now resolves the `AliasRecord` (global storage vs legacy per-table fallback) via the private `alias_run_resolve_record` adapter, delegates the full pipeline to Core, and serialises the `AliasRunValue` back to the existing JSON shape via `alias_run_value_to_json`. MCP JSON output shape is unchanged (backward compat).
- **`minijinja` dependency moved to `mini-app-core`** (`crates/mcp/Cargo.toml`) — `minijinja = "2"` is removed from the `mini-app-mcp` crate now that template rendering happens inside the Core SDK. The transport crate's dependency tree is one entry lighter.

### Deprecated

### Removed

### Fixed

- **`alias_run` parameterised templates work over Claude Code MCP stdio transport** (`crates/core/src/alias_run.rs`) — the Claude Code stdio client delivers `Option<serde_json::Value>` argument fields as JSON-encoded strings (`Value::String("{...}")`) rather than parsed `Value::Object(...)`. minijinja received a String context with no resolvable keys, `{{ key }}` evaluated to undefined, and lenient render emitted an empty value — the rendered filter then matched rows whose target field was empty, silently returning incorrect results. Defensive parse now detects `Value::String` payloads and re-parses them via `serde_json::from_str` into the expected `Value::Object` shape before passing to minijinja. Object payloads (existing `e2e_mcp` subprocess MCP client, direct SDK callers) pass through unchanged. New unit test `jinja_render_with_stringified_params` exercises the failure mode and the re-parse path.

### Security

## [0.11.0] - 2026-06-14

### Changed (MCP Resources)

- **`docs://readme` resource is renamed to `docs://quickstart`** (`crates/mcp/src/mcp/server.rs`, `crates/mcp/src/mcp/resources.rs`, `crates/mcp/QUICKSTART.md`) — the URI is `docs://quickstart`, the advertised resource name is `"Quickstart"`, and the embedded content is a purpose-built agent quickstart at `crates/mcp/QUICKSTART.md`. The previous resource embedded the full workspace-root `README.md` (human-facing — build / releases / contribution, ~36KB) via `include_str!("../../../../README.md")`, which broke `cargo publish` (the workspace-root path resolves differently in the extracted package tarball) and was a structural mismatch besides: agents reading the resource at runtime need mode detection (multi-table vs legacy `TABLE_REQUIRED` signal) and a first-call recipe, not the human-facing project README. The new `QUICKSTART.md` (~70 lines) documents server identity, multi-table vs legacy mode detection, the first-call recipe (`info` → `list` → `get` → mutate), and pointers to the other `docs://` resources (`docs://tools`, `docs://errors`, `docs://filters`). The file lives inside the crate package root so `cargo publish` finds it identically in the git tree and in the extracted tarball. Internal renames: `pub const README` → `pub const QUICKSTART` (`resources.rs`); `const URI_DOCS_README` → `const URI_DOCS_QUICKSTART` (`server.rs`). This crate has not been published to crates.io before this release, so the rename is part of the initial public surface and not a backward-incompatible break of a prior release.

### Added (Phase 2 — Global Alias)

- **`GlobalAliasStorage`** (`crates/core/src/alias_storage.rs`) — Phase 2 single-source-of-truth for named queries that span Single / Multi / Pattern table sources. Persists records in a dedicated `_global.db` SQLite file in each of the Project (`<project_dir>/_global.db`) and User (`<user_dir>/_global.db`) scope directories, with lookup precedence **Project → User** on name collisions. Holds at most two `Arc<Mutex<rusqlite::Connection>>` handles (one per scope, both `Send + Sync`-safe inside `spawn_blocking`). `alias_create(scope, record)` writes to the chosen scope; `alias_get(name)` resolves Project first then falls back to User; `alias_list()` returns the union sorted ascending with Project entries overwriting User collisions; `alias_delete(scope, name)` removes from the named scope.
- **`AliasScope` enum** (`crates/core/src/alias_storage.rs`) — `Project` / `User`. Distinguishes write destination (`alias_create` / `alias_delete`) and tags the loaded record's origin (`AliasRecord.scope`).
- **`alias_storage::AliasRecord`** (`crates/core/src/alias_storage.rs`) — Phase 2 record with 7+1 fields: `name`, `sources: SourceSpec`, `aggregator: Option<AliasAggregator>`, `filter`, `default_limit`, `description`, `params_schema`, plus `scope: Option<AliasScope>` populated by `alias_get` / `alias_list`. Distinct from the legacy 5-field `store::AliasRecord` which remains for backward-compat fallback.
- **`SourceSpec::Pattern(String)`** (`crates/core/src/aggregator.rs`) — glob source variant (e.g. `"shi_*"`) that resolves against the live `TableRegistry` table-name list at `alias_run` time via `SourceSpec::resolve_pattern(all_tables)`. Single-`*` glob matcher supports prefix (`shi_*`), suffix (`*_log`), middle (`shi_*_log`), and match-all (`*`); `?` / `[]` are reserved for a future revision and return `AGGREGATOR_ERROR`. Zero-match resolution returns `AGGREGATOR_ERROR` so the caller surfaces the empty-sources error early. `SourceSpec::requires_resolve()` flags Pattern; `SourceSpec::tables()` returns an empty slice for Pattern as a fail-fast bug detector.
- **Lossless + idempotent per-table → global migration** (`GlobalAliasStorage::migrate_from_per_table`, `crates/core/src/registry.rs`) — `TableRegistry::mount_from_dirs` automatically invokes the migration helper at mount time, copying every legacy per-table `_aliases` row into the chosen scope's `_global_aliases` with `sources = Single(<table_name>)` and `aggregator = None`. `INSERT OR IGNORE` semantics make the call safe on every registry rebuild (collisions are silently skipped); destination scope is Project when mounted else User fallback. The per-table `_aliases` tables are left untouched so existing `Store::alias_*` callers keep working (rollback path preserved).
- **`TableRegistry.global_aliases() -> Option<&Arc<GlobalAliasStorage>>`** (`crates/core/src/registry.rs`) — accessor exposing the storage handle when the registry was built via `mount_from_dirs` with at least one of `user_dir` / `project_dir` populated. `None` in legacy `mount_legacy` mode and in test-only `from_entries` / `from_single` constructors.
- **`Store::conn() -> Arc<Mutex<rusqlite::Connection>>`** (`crates/core/src/store.rs`) — public accessor handing out a cloned connection handle for `GlobalAliasStorage::migrate_from_per_table`. The handle is shared (no copy); callers must acquire the `Mutex` lock inside `spawn_blocking`.

### Changed (Phase 2 — Global Alias)

- **MCP `alias_create` tool**: accepts new optional `sources: SourceSpec` (Single / Multi / Pattern) and `aggregator: Option<AliasAggregator>` arguments. `sources` is mutually exclusive with the legacy `table` argument; supplying neither falls back to `resolve_table` for the default table (single-table mode) or surfaces `TABLE_REQUIRED` (multi-table mode). When the registry exposes a `GlobalAliasStorage` the alias is persisted to the Project scope; otherwise it falls back to the per-table `Store::alias_create` path so legacy single-table mode keeps working unchanged.
- **MCP `alias_run` tool**: when a `GlobalAliasStorage` is present, the alias is loaded from global storage and dispatched as follows — aggregator present → resolves `Pattern` against the live registry table list, then calls `execute_aggregate`; aggregator absent + `Single` source → per-table `Store::list` (back-compat path); aggregator absent + `Multi` / `Pattern` source → structured error (Phase 2 limitation, aggregator required for cross-table aliases).
- **MCP `alias_list` tool**: when global storage is available, returns the merged Project + User listing (Project entries override User collisions). The legacy `table` argument is honoured as a post-fetch `sources=Single(<table>)` filter so existing callers still see only their table's aliases.
- **MCP `alias_delete` tool**: when global storage is available, tries the Project scope first and falls back to User scope on `ALIAS_NOT_FOUND` — mirroring `alias_get`'s lookup precedence.

### Added (Phase 1 — Multi-Table Aggregate)

- **New MCP tool `query_aggregate`** (`crates/mcp/src/mcp/server.rs`, `crates/mcp/src/mcp/resources.rs`) — multi-table aggregation supporting `Count` / `Sum` / `Avg` / `Min` / `Max` / `GroupBy` (with optional `HAVING` predicate and optional per-group inner aggregator). Read-only / idempotent. Returns an externally-tagged `AliasRunResult` (`count` / `value` / `groups`) so MCP callers can dispatch on the result kind. Tool count is now 18 (previously 17); the cheat sheet (`docs://tools`) and `info` description are updated in lockstep.
- **`SourceSpec::Multi(Vec<String>)`** (`crates/core/src/aggregator.rs`) — multi-table source specifier that mounts each backing `.db` file via SQLite `ATTACH DATABASE` and emits a literal `UNION ALL` between per-table sub-queries (never `JOIN`, never application-layer merge). `SourceSpec::Single` is the backward-compatible 1-table case and the normalisation target for any legacy single-table caller. `SourceSpec::Pattern` is reserved for Phase 2 (Global Alias unification).
- **`mini_app_core::aggregator` module** — public API surface for the new aggregation primitives: `AliasAggregator` enum (6 variants), `SourceSpec`, `AliasRunResult`, `GroupResult`, and `pub async fn execute_aggregate(...)` which mounts the per-table `.db` files into a fresh in-memory connection and dispatches on the aggregator variant. Re-exported from `mini-app-mcp` as `crate::aggregator` for the MCP tool body.
- **`ListFilter::build_subquery(table)`** (`crates/core/src/filter.rs`) — sibling method to `build_sql` that wraps the WHERE fragment into a full `SELECT id, data, created_at, updated_at FROM <table> WHERE <fragment>` statement. The aggregator uses this to compose each `UNION ALL` branch without touching `build_sql` (Crux #1 preservation).
- **`Store::db_path() -> &Path`** (`crates/core/src/store.rs`) — accessor that returns the filesystem path of the SQLite database backing the store. Captured at `Store::open` time and exposed for the aggregator's `ATTACH DATABASE` path. The `Store::open` signature is unchanged (the new field is private).
- **`MiniAppError::Aggregator(String)` variant** (`crates/core/src/error.rs`) + **`codes::AGGREGATOR_ERROR`** — structured error for aggregator-specific failures (empty sources, ATTACH-limit exceeded (10), nested `GroupBy`, non-UTF-8 db path, identifier rejected by the `[A-Za-z_][A-Za-z0-9_]*` regex). The `error_conv` ACL adapter's catch-all arm surfaces this with `data = { "code": "AGGREGATOR_ERROR", "message": "..." }`.

### Changed

- **Workspace split** (commit `bb2a208`) — the single `mini-app-mcp` crate has been split into a 2-crate workspace: `mini-app-core` (lib, v0.1.0) carries the transport-agnostic DB layer (`schema` / `error` / `config` / `store` / `filter` / `materialize` / `dump` / `backup` / `snapshot` / `registry`), and `mini-app-mcp` (bin) re-exports the same module paths and provides the MCP stdio transport. Existing callers using `mini_app_mcp::{schema, error, store, ...}` continue to compile without path changes via re-exports in `crates/mcp/src/lib.rs`. `mini-app-core` carries zero `rmcp` dependency (one-way `mcp → core` boundary); `rmcp::ErrorData` conversion lives in `crates/mcp/src/error_conv.rs` as a `pub(crate) fn miniapp_error_to_mcp_error` ACL adapter (Outline rust book §5-1-10 K-orphan-rule pattern). The `mini-app-mcp` binary version remains `0.10.0` on this branch; the workspace-split-aware binary will ship as the next patch / minor release.

## [0.10.0] - 2026-05-27

### Added

- **UUID prefix match for `get`, `update`, and `delete` tools** (`src/store.rs`, `src/error.rs`, `src/mcp/server.rs`) — when the supplied `id` is shorter than 36 characters (the canonical UUID v4 length) the server performs a `WHERE id LIKE '<prefix>%'` query before the main operation. Three outcomes are possible: zero matches return `NOT_FOUND` (same code as before); exactly one match proceeds with the resolved full UUID; two or more matches return `AMBIGUOUS_ID` with the list of candidate ids so the caller can disambiguate. A full 36-character UUID bypasses the prefix scan entirely and resolves via the existing exact-match path — no behaviour change for existing callers that always supply complete ids. The prefix scan and the main operation execute within the same `spawn_blocking` closure and `Mutex` lock, preventing TOCTOU races. The `delete` hook (`dump::on_delete`) receives the resolved full UUID, not the prefix string, so dump filenames remain consistent.
- **`MiniAppError::AmbiguousId { id_prefix, candidates }` variant** (`src/error.rs`) — new error variant returned when a prefix matches two or more rows. The `From<MiniAppError> for McpError` conversion serialises it as `{ "code": "AMBIGUOUS_ID", "message": "...", "id_prefix": "...", "candidates": [...] }`. The `candidates` array is always present and non-empty, giving callers the full list of matching ids for display or re-query. `codes::AMBIGUOUS_ID` constant added alongside the existing code constants.
- **Tool description updates for `get`, `update`, and `delete`** (`src/mcp/server.rs`) — `#[tool]` descriptions now document the prefix match contract: `id` values shorter than 36 characters are treated as prefixes; `NOT_FOUND` and `AMBIGUOUS_ID` are listed as possible outcomes alongside the existing error codes.
- **`fields` parameter on `list`, `get`, and `alias_run` tools** (`src/materialize.rs`, `src/mcp/server.rs`) — field projection for all three read tools. Each tool now accepts an optional `fields` argument following the existing `FieldSelector` shape: `{"mode": "all"}` returns all schema fields (default, backward-compatible) and `{"mode": "list", "fields": ["field1", "field2"]}` restricts the returned `data` object to the named subset. `id`, `created_at`, and `updated_at` are always returned unchanged; only the `data` portion is projected. Unknown field names return `VALIDATION_ERROR` before any query executes. Omitting `fields` is fully backward-compatible — all existing callers continue to receive complete rows without change.
- **`materialize::apply_projection` helper** (`src/materialize.rs`) — single post-materialization, pre-serialization boundary shared by `list`, `get`, and `alias_run`. Validates field names against the schema's canonical field definitions (never against actual data keys), then projects each `RowRecord` by replacing its `data` field with a filtered `serde_json::Map`. The existing private `project_row` helper continues to perform the per-row field extraction; `apply_projection` owns schema validation and drives the `Vec<RowRecord>` transformation.
- **`FieldSelector::validate` method** (`src/materialize.rs`) — new `pub fn validate(&self, schema: &SchemaConfig) -> Result<(), MiniAppError>` method. For `FieldSelector::List`, checks every field name against `schema.fields` and returns `MiniAppError::Validation { field, reason }` (`VALIDATION_ERROR` code) for any unknown name. Follows the same pattern as `ListFilter::validate` in `filter.rs`.
- **Schema-based field validation for projection** (`src/materialize.rs`) — field name validation consults `SchemaConfig.fields` (the canonical definition list), never `row.data` keys, ensuring unknown fields are detected reliably even when the projected data would simply omit the key.
- **Unit and integration tests** (`src/materialize.rs`, `src/mcp/server.rs`) — tests cover: `FieldSelector::validate` (valid subset, unknown field returns `VALIDATION_ERROR`); `apply_projection` (all-mode no-op, list-mode projection, unknown field propagation); `list` with `fields` (projected response); `get` with `fields` (projected single row); `alias_run` with `fields` (projected alias result).

- **`alias_create` MCP tool** (`src/mcp/server.rs`) — registers a named query alias for a table. Accepts `name`, `filter` (a `ListFilter` expression), optional `default_limit`, and optional `description`. Alias names are unique per table; a second call with the same name returns `ALIAS_ALREADY_EXISTS`. Annotations: `read_only_hint=false`, `destructive_hint=false`, `idempotent_hint=false`.
- **`alias_list` MCP tool** (`src/mcp/server.rs`) — returns all aliases registered for the specified table as a JSON array of `{ name, filter, default_limit, description }` objects. Annotations: `read_only_hint=true`, `idempotent_hint=true`.
- **`alias_run` MCP tool** (`src/mcp/server.rs`) — executes a stored alias by name. Accepts optional runtime `limit` and `offset` arguments that override the stored `default_limit` at execution time; if neither is supplied, `default_limit` from the alias is used. Internally calls `Store::list` with the resolved `limit`, `offset`, and stored `filter`. Returns the same shape as the `list` tool. Annotations: `read_only_hint=true`, `idempotent_hint=true`.
- **`alias_delete` MCP tool** (`src/mcp/server.rs`) — removes a named alias for the specified table. Returns `ALIAS_NOT_FOUND` if the alias does not exist. Annotations: `read_only_hint=false`, `destructive_hint=true`, `idempotent_hint=true`.
- **`_aliases` DDL** (`src/store.rs`) — `CREATE TABLE IF NOT EXISTS _aliases (name TEXT PRIMARY KEY, filter TEXT NOT NULL, default_limit INTEGER, description TEXT)` is executed inside `Store::open` alongside the existing `rows` table DDL. The `_` prefix marks this as an internal management table. Aliases are scoped per table: each table's `.db` file contains its own `_aliases` table, so alias names are isolated across tables.
- **`Store::alias_create`, `alias_get`, `alias_list`, `alias_delete` methods** (`src/store.rs`) — four async `Store` methods following the existing `spawn_blocking` + `conn.lock()` pattern. `alias_create` uses `INSERT OR IGNORE` + `changes() == 0` to detect duplicates without catching constraint violations directly. `alias_get` maps `QueryReturnedNoRows` to `MiniAppError::AliasNotFound`.
- **`AliasRecord` struct** (`src/store.rs`) — holds `name: String`, `filter: ListFilter`, `default_limit: Option<i64>`, `description: Option<String>`; returned by `alias_get` and `alias_list`.
- **`MiniAppError::AliasNotFound { name }` and `MiniAppError::AliasAlreadyExists { name }` variants** (`src/error.rs`) — with dedicated error codes `ALIAS_NOT_FOUND` and `ALIAS_ALREADY_EXISTS`. Both variants carry the alias `name` as structured data in the `From<MiniAppError> for McpError` conversion.
- **E2E alias round-trip test** (`tests/e2e_mcp.rs`) — covers `alias_create` → `alias_list` → `alias_run` (with runtime limit override) → `alias_delete` → verify deleted, plus `alias_run` on a missing alias returning `ALIAS_NOT_FOUND`.

- **`Like` variant in `ListFilter`** (`src/filter.rs`) — adds partial-match filtering to the `list` tool. Serialises as `{"type": "like", "field": "...", "pattern": "..."}` where `%` matches any substring and `_` matches any single character, delegating to SQLite's native `LIKE` operator via `json_extract(data, '$.{field}') LIKE ?`. Restricted to `string`-typed schema fields; non-string fields are rejected with `VALIDATION_ERROR` before any SQL executes. The variant is automatically reflected in the MCP list tool's JSON schema through the existing `#[derive(Deserialize, Serialize, JsonSchema)]` and `#[serde(tag = "type", rename_all = "snake_case")]` attributes — no edits to `store.rs`, `mcp/server.rs`, or any other source file are required.
- **6 new tests** (`src/filter.rs`) — `like_validate_ok`, `like_unknown_field_reject`, `like_non_string_field_reject` (validate path); `like_build_sql`, `like_in_and_composition_build_sql` (build_sql path); `like_serde_roundtrip` (serde round-trip).

- **`docs://filters` MCP resource** (`src/mcp/resources.rs`, `src/mcp/server.rs`) — new read-only resource serving a filter construction guide (Markdown). Covers `Eq`, `In`, `Like`, `Or`, `And` filter types with JSON examples, nested composition examples, and a worked `alias_create` + `alias_run` example showing runtime limit/offset override. Registered in `resource_list` and `read_resource_impl` alongside the existing `docs://` resources.
- **`docs://tools` updated** (`src/mcp/resources.rs`) — added documentation sections for `schema_create`, `schema_update`, `schema_delete`, `schema_batch`, `alias_create`, `alias_list`, `alias_run`, and `alias_delete`. Updated the header comment and `docs://tools` resource description from "12 tools" to "17 tools".
- **`docs://errors` updated** (`src/mcp/resources.rs`) — added `ALIAS_NOT_FOUND` (404) and `ALIAS_ALREADY_EXISTS` (409) error code entries to the error code reference table.
- **`MCP resources` section in README updated** — resource count updated from 6 to 7; `docs://filters` row added to the resource table; `docs://tools` description updated to "all 17 MCP tools".

- **Parameterized alias templates** (`src/mcp/server.rs`, `src/store.rs`, `src/error.rs`, `Cargo.toml`) — `alias_create` now accepts an optional `filter_template` (a MiniJinja template string) and an optional `params_schema` (array of parameter name strings) in addition to the existing `filter` field. At run time, `alias_run` accepts a `params` object whose key-value pairs are injected into the template via `minijinja::Environment::render_str`; the rendered output is then parsed as JSON and validated through the existing `ListFilter` path before being passed to `Store::list`. Existing aliases that use `filter` (no template) continue to work without any change — if `params_schema` is null the render step is skipped entirely and the stored filter JSON is used directly (backward-compatible no-op path).
- **`filter` / `filter_template` mutual exclusion** (`src/mcp/server.rs`) — `alias_create` enforces that exactly one of `filter` or `filter_template` is supplied. Providing both or neither returns a `VALIDATION_ERROR` before any write occurs.
- **`_aliases` DDL migration** (`src/store.rs`) — `Store::open` now runs `PRAGMA table_info(_aliases)` to check whether the `params_schema` column exists and, if not, issues `ALTER TABLE _aliases ADD COLUMN params_schema TEXT`. This migration is idempotent and executes only when an older database (created before 0.10.0-params) is opened. New databases receive the column via the `CREATE TABLE IF NOT EXISTS` DDL directly.
- **`AliasRecord.params_schema` field** (`src/store.rs`) — the `AliasRecord` struct gains `params_schema: Option<String>`. The `alias_get` and `alias_list` queries now SELECT five columns; `alias_list` includes `params_schema` in the returned JSON objects.
- **`minijinja` dependency** (`Cargo.toml`) — `minijinja` added to render parameterized filter templates at `alias_run` time.
- **`MiniAppError::AliasParamsRequired { name }` variant** (`src/error.rs`) — returned (code `ALIAS_PARAMS_REQUIRED`) when `alias_run` is called on an alias whose `params_schema` is non-null but no `params` object is supplied by the caller. Agents can detect this code and prompt the user or upstream logic for the missing parameter values.
- **`MiniAppError::AliasTemplateError(String)` variant** (`src/error.rs`) — returned (code `ALIAS_TEMPLATE_ERROR`) when MiniJinja fails to render the stored template (e.g. syntax error in the template, unknown variable in `params`, or type mismatch). Distinct from `VALIDATION_ERROR` so agents can distinguish a render failure from a filter-schema validation failure and take appropriate recovery action.
- **E2E parameterized alias test** (`tests/e2e_mcp.rs`) — covers `alias_create` with `filter_template` + `params_schema` → `alias_run` with `params` (successful render → list) → `alias_run` without `params` (returns `ALIAS_PARAMS_REQUIRED`).

## [0.9.0] - 2026-05-26

> **BREAKING CHANGE**: The `update` tool now defaults to **merge** (RFC 7396 shallow merge) instead of full replacement. Callers that depend on the old full-replacement behaviour must pass `"mode": "replace"` explicitly to restore it. Callers that omit `mode` will now receive merge semantics, which preserves fields absent from the patch and deletes fields whose patch value is `null` (subject to `required` constraints).

### Added

- **`UpdateMode` enum** (`src/store.rs`, re-exported from `src/lib.rs`) — two variants: `Merge` (default) and `Replace`. `Merge` performs RFC 7396 shallow merge; `Replace` restores the pre-breaking-change full-replacement behaviour byte-for-byte.
- **`mode` parameter on the `update` MCP tool** (`src/mcp/server.rs`) — optional `"mode"` field (`"merge"` or `"replace"`, default `"merge"`). Omitting `mode` is equivalent to `"mode": "merge"` and is the new default.
- **RFC 7396 shallow merge logic** (`src/store.rs`) — new private `shallow_merge` helper applies top-level field-by-field patching: absent fields are preserved from the stored row, non-null patch values overwrite the stored value for that key, and `null` patch values either delete the field (`required=false`) or raise a `Validation` error (`required=true`). Nested objects are replaced wholesale (not recursively merged). Post-merge full schema validation runs before persisting.
- **5 merge/replace grid tests** (`src/mcp/server.rs`) — `test_update_replace_round_trip` (Replace mode byte-for-byte identity), `test_update_merge_absent_field_preserved` (absent fields survive merge), `test_update_merge_null_required_error` (null on required field → Validation error), `test_update_merge_null_optional_deletes_and_validates` (null on optional field deletes key + post-merge validation runs), `test_update_merge_nested_object_full_replace` (nested object is fully replaced, not deeply merged).

### Changed

- **`update` tool default semantics** — the default behaviour changed from full replacement to RFC 7396 merge. This is a breaking change for callers that relied on the old default (sending a partial `data` object now preserves unmentioned fields rather than silently deleting them). To restore old behaviour pass `"mode": "replace"`.
- **`Store::update` signature** (`src/store.rs`) — added `mode: UpdateMode` parameter. All existing internal call sites now pass `UpdateMode::Replace` for backward-compatible test coverage; the MCP layer passes the caller-supplied mode (defaulting to `Merge`).

## [0.8.0] - 2026-05-26

> **Note**: This release adds a new public MCP tool (`row_materialize`), 9 new `MiniAppError` variants, and 2 new crate dependencies (`sha2`, `hex`). Per [Cargo SemVer Compatibility §1.3.5](https://doc.rust-lang.org/cargo/reference/semver.html#enum-variant-new), enum variant additions on `MiniAppError` (non-`#[non_exhaustive]`) are SemVer-major; pre-1.0 this is signalled by a minor bump (0.7.0 → 0.8.0), same convention as 0.6.0 → 0.7.0. JSON / MCP wire format is fully back-compatible; only Rust-level downstream consumers that exhaustively `match` on `MiniAppError` need to add arms for the new `Materialize*` variants.

### Added

- **`row_materialize` MCP tool** (`src/materialize.rs`, `src/mcp/server.rs`) — writes one or more rows to arbitrary absolute paths on the local filesystem. Rows are selected by `id` (`RowSelector::ById`) or by a `ListFilter` expression (`RowSelector::ByFilter`). Output format is one of `raw` (newline-separated field values), `markdown` (field names as headings), `json` (pretty JSON object or array), or `yaml` (YAML document stream). When `concat=false` (default) the destination is treated as a directory and each row is written to `{dest}/{id}.{ext}`; when `concat=true` all rows are concatenated into a single file at `dest`. An optional `write_mode` controls whether an existing file is overwritten (`Overwrite`, default) or rejected (`Error`). Supports `dry_run=true` to compute path, byte count, and SHA-256 without writing any file. Returns `{ count, files: [{path, bytes, sha256, row_id}] }` — every output file always includes a 64-character SHA-256 hex digest computed from the written bytes, providing an integrity fingerprint for idempotency verification. `row_id` is `Some` for per-row files and `None` for concatenated output. Marked `read_only_hint=false`, `destructive_hint=true` (overwrite-by-default), `idempotent_hint=true`.
- **9 new `MiniAppError` variants** (`src/error.rs`) — `MaterializeDestRelative`, `MaterializeDestInvalid`, `MaterializeIo`, `MaterializeSha256`, `MaterializeRowNotFound`, `MaterializeEmptyResult`, `MaterializeFormatError`, `MaterializeFieldUnknown`, `MaterializeInvalidParam` — each with a dedicated `MATERIALIZE_*` error code for programmatic handling.
- **`sha2` and `hex` crate dependencies** (`Cargo.toml`) — used for SHA-256 digest computation inside `tokio::task::spawn_blocking`.

- ListFilter enum (Eq/In/Or/And) for server-side row filtering in `list` tool. Supports recursive Or/And composition over schema-validated fields with typed scalar checking. `filter=None` keeps full backward compatibility for existing callers.

## [0.7.0] - 2026-05-21

> **Note**: This release adds a new `BatchOp::Replace` variant to the existing `BatchOp` enum. Per [Cargo SemVer Compatibility §1.3.5](https://doc.rust-lang.org/cargo/reference/semver.html#enum-variant-new), adding a variant to a non-`#[non_exhaustive]` enum is a SemVer-major change — pre-1.0 this is signalled by a minor bump (0.6.0 → 0.7.0), same convention as 0.5.x → 0.6.0. JSON / MCP wire format is fully back-compatible via `#[serde(tag = "op", rename_all = "snake_case")]`; only Rust-level downstream consumers that exhaustively `match` on `BatchOp` need to add a `BatchOp::Replace { .. } => ...` arm.

### Added

- **`BatchOp::Replace` variant** (`src/mcp/schema_tools.rs`) — new batch operation that replaces an edge set atomically. The caller supplies a `match` scope (key/value pairs) and an `items` list; the server generates UUIDs, timestamps, and JSON serialisation. Internally, DELETE WHERE (match) and N INSERTs execute within a single existing SAVEPOINT so both phases roll back together on any failure. Serialises to `"op": "replace"` on the wire.
- **`ReplaceAffects` struct and `BatchResult.affects` field** (`src/mcp/schema_tools.rs`) — `BatchResult` gains an `affects: Option<ReplaceAffects>` field (`#[serde(skip_serializing_if = "Option::is_none")]`). When a Replace op is committed the field is present as `{"deleted": N, "inserted": M}`; all other batch results omit it. `ReplaceAffects` derives `JsonSchema` via the already-present `schemars` dependency.
- **Empty `match` guard** — a Replace op with an empty `match` object (`{}`) is rejected with `VALIDATION_ERROR` before any SQL executes, preventing accidental full-table deletion.
- **AND-chained `json_extract` predicates** — each key in `match` is translated to a separate `json_extract(data, '$.key') = value` clause joined by AND. Scalar values only (string/number/bool/null); array/object values and keys with characters outside `[A-Za-z0-9_-]` are rejected.
- **4 crux regression tests** (`src/mcp/schema_tools.rs` `mod tests`) — `test_replace_savepoint_rollback_on_error` (crux MNS 1: atomicity), `test_replace_empty_match_validation_error` (crux MNS 3: guard), `test_replace_multi_key_and_predicate` (crux MNS 2: AND chain), `test_replace_preserves_non_matched_rows` (set-diff correctness).

### Fixed

- **`BatchOp::Replace` silent no-op on typed scalar match values** (`src/mcp/schema_tools.rs`) — match values of JSON `Null` / `Number` / `Bool` were previously stringified via `v.to_string()` and bound as text, which never matched SQLite `json_extract` results (SQL NULL / INTEGER / 1-0), causing DELETE to silently affect zero rows. Added file-local `validate_match_value` helper that admits only `Value::String` and rejects Null / Number / Bool / Array / Object with `MiniAppError::Validation` before any SQL executes (Option A: smallest correct diff). 3 new named tests (`test_batch_replace_null_match_value_rejected_with_validation_error`, `..._number_...`, `..._bool_...`) cover each rejected scalar type.

## [0.6.0] - 2026-05-08

> **Note**: This release adds public fields to several existing structs (`SchemaConfig`, `FieldDef`, `FieldDefInput`, `SchemaCreateParams`, `SchemaUpdateParams`, `BatchOp::SchemaCreate`, `BatchOp::SchemaUpdate`). Per [Cargo SemVer Compatibility §1.3.1](https://doc.rust-lang.org/cargo/reference/semver.html#struct-add-public-field-when-no-private), adding a public field to a non-`#[non_exhaustive]` struct is a SemVer-major change — pre-1.0 this is signalled by a minor bump (0.5.x → 0.6.0). YAML / JSON wire format is fully back-compatible via `#[serde(default)]`; only Rust-level downstream consumers that construct these structs by literal initialization need to be updated to add `title: None` / `description: None`.

### Added

- **`title` and `description` fields on `SchemaConfig`** (`src/schema.rs`) — table-level `title: Option<String>` and `description: Option<String>` added to `SchemaConfig` with `#[serde(default)]`. Existing `schema.yaml` files without these keys continue to deserialize correctly. When present, both fields are serialized back into the YAML file and included in the `info` tool response and `schema://json` resource automatically.
- **`description` field on `FieldDef`** (`src/schema.rs`) — field-level `description: Option<String>` added to `FieldDef` with `#[serde(default)]`. Enables per-field documentation that round-trips through YAML and appears in `info` tool output.
- **`title` and `description` parameters on `schema_create` / `schema_update` tools** (`src/mcp/schema_tools.rs`, `src/mcp/server.rs`) — `SchemaCreateParams` and `SchemaUpdateParams` now accept optional `title` and `description` arguments. `BatchOp::SchemaCreate` and `BatchOp::SchemaUpdate` variants also accept these fields. Values are written to `schema.yaml` and immediately readable via `info` or `schema://json`. Naming follows OpenAPI 3.1 §4.7 / JSON Schema 2020-12 §9.1 / RustDoc conventions; no non-standard aliases are used.
- **`description` parameter on `FieldDefInput`** (`src/mcp/schema_tools.rs`) — field definitions supplied to `schema_create` / `schema_update` / `schema_batch` may now include a `description` string per field. The value is forwarded to `FieldDef` via `parse_fields` and persisted in `schema.yaml`.
- **Round-trip tests** (`src/schema.rs`, `src/mcp/schema_tools.rs`, `src/mcp/server.rs`) — `yaml_with_title_and_description_deserializes`, `yaml_without_title_section_yields_none`, `field_def_with_description_deserializes`, `schema_create_round_trips_title_and_description`, `tool_info_includes_title_and_description` verify the full write → persist → read path.

## [0.5.1] - 2026-05-07

### Fixed

- **`create` / `update` reject all object payloads from Anthropic-style clients** (`src/mcp/server.rs`) — `CreateParams.data` and `UpdateParams.data` were typed as `serde_json::Value`, which schemars 1.x emits as a permissive schema with no `type` field. Anthropic's tool-use serializer treats untyped params as opaque and stringifies them, so the server received a `Value::String` and rejected every call with `validation error on field '(root)': value must be a JSON object`. A `data_object_schema` helper now applies `#[schemars(schema_with = ...)]` to both fields, advertising `{"type":"object","additionalProperties":true}` in the public tool schema so clients send the value as an actual JSON object.

### Added

- **MCP stdio E2E test suite** (`tests/e2e_mcp.rs`) — spawns the real `mini-app-mcp --mcp` binary via `std::process::Command` and drives it over stdio JSON-RPC. Covers (a) `tools/list` advertising `data` as `"type":"object"` for `create`/`update`, (b) all 12 tools present, (c) `create` → `get` → `update` → `list` → `delete` round-trip via `tools/call` wire format, (d) negative path: a stringified `data` argument is rejected. Uses tempdir-rooted `MINI_APP_USER_DIR` / `MINI_APP_PROJECT_DIR` so the suite never touches `~/.mini-app/`. Closes the gap that allowed the 0.5.0 schema-shape regression to ship: in-process unit tests bypassed the JSON-RPC serialization layer where the bug manifested.

## [0.5.0] - 2026-05-05

### Added

- **`data_snapshot` MCP tool** (`src/snapshot.rs`, `mcp/server.rs`) — creates per-table SQLite-only snapshot dumps via `rusqlite::Connection::backup` (hot backup API) to `<scope_root>/_snapshots/<table>.<unix_secs>.db`. Works in three modes: `table=Some` targets one table; `table=None + scope=Some` fan-outs over all mounted tables in the given scope; `table=None + scope=None` snapshots all mounted tables. Supports `dry_run=true` to return `{ affects: { target_tables, row_counts, would_purge_generations } }` with zero FS/DB writes guaranteed. Retention is controlled exclusively by `MINI_APP_SNAPSHOT_RETENTION` (default 10) and is strictly separate from the backup retention used by schema tools. Snapshot I/O runs inside `tokio::task::spawn_blocking` with a fresh `Connection::open` per snapshot so the source database remains open and writable throughout. Marked `read_only_hint=false`, `idempotent_hint=false` (successive calls produce distinct timestamped files), `destructive_hint=false` (purge is bounded by retention).
- **`MINI_APP_SNAPSHOT_RETENTION` environment variable** (`config.rs`) — controls how many snapshot generations to keep per table under `<scope_root>/_snapshots/`. Defaults to 10. Strictly isolated from `MINI_APP_BACKUP_RETENTION`; neither variable reads nor writes the other's directory.
- **Snapshot module** (`src/snapshot.rs`) — `write_snapshot_db` / `purge_old_snapshots` / `do_data_snapshot` public async functions. `write_snapshot_db` uses `PRAGMA wal_checkpoint(TRUNCATE)` before backup (warn-on-error, non-fatal). `MiniAppError::Snapshot(String)` variant added to `src/error.rs` (code: `SNAPSHOT_ERROR`) with the same string-tuple pattern as `Backup` to avoid `#[from]` conflicts (K-79).

- **`schema_create` MCP tool** (`mcp/schema_tools.rs`, `mcp/server.rs`) — creates a new `schema.yaml` under the specified scope (`project` or `user`) directory and atomically rebuilds the live table registry. Accepts the full schema definition (table name, fields) as a JSON argument. Returns the path where the schema was written. Fails with `SCHEMA_EXISTS` if a schema for that table name already exists. Supports `dry_run=true` to preview the operation without writing any files. Path-traversal characters in the table name are rejected up front.
- **`schema_update` MCP tool** (`mcp/schema_tools.rs`, `mcp/server.rs`) — replaces an existing `schema.yaml` with a new definition (full overwrite). Backs up the previous YAML and a point-in-time SQLite snapshot to `<scope_root>/_backup/<table>.<unix_secs>.{yaml,db}` before writing. Rebuilds the table registry after the write. Supports `dry_run=true` (returns `{ fields_added, fields_removed }` without touching disk). Idempotent: calling twice with identical args produces the same observable state.
- **`schema_delete` MCP tool** (`mcp/schema_tools.rs`, `mcp/server.rs`) — removes a table's `schema.yaml` by moving it to the `_backup/` directory and removes the table from the live registry. **Does not touch the SQLite database file** — altering or dropping the underlying table remains the operator's explicit responsibility (no automatic DDL migration). Supports `dry_run=true`. Marked `destructive_hint=true`.
- **`schema_batch` MCP tool** (`mcp/schema_tools.rs`, `mcp/server.rs`) — executes an array of operations (`ops[]`) atomically under a single SQLite SAVEPOINT: any op failure rolls back all preceding ops including schema mutations, leaving YAML and DB in the exact state they were before the batch started. All ops must target the same table (cross-table batches are rejected with `VALIDATION` error). YAML writes within a batch are deferred; rename is applied only on SAVEPOINT commit, and tmp files are removed on rollback. Registry is rebuilt once after all ops succeed.
- **Backup module** (`src/backup.rs`) — `write_yaml_backup` / `write_db_backup` functions that write point-in-time copies of a table's schema file and SQLite database to `<scope_root>/_backup/<table>.<unix_secs>.yaml` and `<table>.<unix_secs>.db`. A retention sweep runs immediately after each backup write and deletes the oldest copies beyond the configured limit (default 10, overridable via `MINI_APP_BACKUP_RETENTION`). Backup I/O runs inside `tokio::task::spawn_blocking`.
- **New error variants** (`src/error.rs`) — `SchemaExists { table }` (code: `SCHEMA_EXISTS`), `Backup(String)` (code: `BACKUP_ERROR`), and `BatchAborted { op_index, reason }` (code: `BATCH_ABORTED`). All three carry structured fields through `From<MiniAppError> for McpError` so agents can handle them programmatically.

### Fixed

- **Path-traversal rejection in schema CRUD tools** (`mcp/schema_tools.rs`) — table names containing `/`, `\`, or `..` components are rejected with `MiniAppError::Validation` before any filesystem operation is attempted. This prevents a caller from escaping the configured scope directory by supplying a crafted table name.

## [0.4.0] - 2026-05-04

### Added

- **`reload` MCP tool** (`mcp/server.rs`) — new tool that re-scans `MINI_APP_USER_DIR` / `MINI_APP_PROJECT_DIR` (and re-applies `MINI_APP_SCHEMA` + `MINI_APP_DB` if set) and atomically replaces the live table registry without restarting the server. Returns `{ mounted: usize, added: Vec<String>, removed: Vec<String> }` so callers can observe which tables changed. The swap is performed via `ArcSwap::store()` — in-flight tool calls running against the previous registry complete normally; subsequent calls see the new registry. Limitations: no file-system watcher (explicit invocation only); whole-registry replacement (no per-table partial reload); no schema migration for existing rows; concurrent `reload` calls are last-write-wins.

### Changed

- **WAL journal mode on all SQLite connections** (`store.rs`) — `Store::open` now executes `PRAGMA journal_mode = WAL` immediately after opening every connection. WAL mode is persistent (SQLite retains it across close/reopen) and enables one writer + many concurrent readers, which is required for safe operation during the dual-registry window that exists while `reload` replaces the table registry. Existing `.db` files are migrated transparently on next open. Sidecar files `<db>.db-wal` and `<db>.db-shm` are created alongside each `.db` file; these are managed by SQLite and must not be deleted manually.
- **`MiniAppMcpServer` internals** (`mcp/server.rs`) — `tables` field changed from `Arc<TableRegistry>` to `Arc<ArcSwap<TableRegistry>>` to support atomic hot-reload. `Config` is now retained on the server struct (`Arc<Config>`) so the `reload` tool can re-scan the same directories that were used at startup. All existing tool implementations (`info`, `create`, `get`, `list`, `update`, `delete`) load a snapshot of the registry via `ArcSwap::load()` at the start of each call and release the guard before any `await` point. `TableRegistry` doc comment updated from "immutable, no interior mutability" to "snapshot is immutable; replaced via ArcSwap on reload".
- **`arc-swap` dependency added** (`Cargo.toml`) — `arc-swap = "1"` added to support the wait-free atomic swap of `TableRegistry`.

### Fixed

- **`reload` early-reject on legacy single-table servers** (`mcp/server.rs`) — when `MiniAppMcpServer` is constructed via `new_single` (legacy adapter path), all four `mount_config` fields are `None`. Calling the `reload` tool on such a server previously would re-mount an empty registry and atomically swap out the originally-mounted table, leaving the server inaccessible until restart. `tool_reload` now detects this all-`None` configuration up front and returns `MiniAppError::Config("reload not configured: server was constructed via new_single without a mount config")` without touching the registry.
- **`PRAGMA journal_mode = WAL` read-back warning** (`store.rs`) — SQLite silently falls back to a non-WAL mode (memory / delete) on filesystems that do not support WAL (notably `:memory:` databases, some network filesystems). `Store::open` now reads back the resulting `journal_mode` after issuing the WAL pragma and emits `tracing::warn!(actual_mode = ..., "PRAGMA journal_mode=WAL fell back to non-WAL mode; concurrent reload may hit SQLITE_BUSY")` when the actual mode is not `wal`. The fallback is observable instead of silent; behaviour is unchanged otherwise (no error returned).

## [0.3.1] - 2026-05-03

### Changed

- **Empty-registry start is no longer fatal** (`mcp/server.rs`) — when 0 tables resolve from `MINI_APP_USER_DIR` / `MINI_APP_PROJECT_DIR` and no legacy `MINI_APP_SCHEMA` + `MINI_APP_DB` is set, the server now logs a `tracing::warn!` and proceeds to serve `info` and resources instead of erroring out. Tool calls return `TABLE_REQUIRED` on a per-call basis. This lets `mini-app-mcp` be deployed once into a user-global MCP registry (e.g. `~/.claude.json`) and have table directories added later without restarting the host.
- **Auto-create `MINI_APP_USER_DIR`** (`mcp/server.rs`) — at startup the server runs `tokio::fs::create_dir_all` on the resolved User-scope directory (default `~/.mini-app/`). Failures are logged as a warning, not propagated. Project-scope directory is intentionally left untouched to avoid polluting arbitrary CWDs.

## [0.3.0] - 2026-05-03

### Added

- **Multi-table support** (`mcp/registry.rs`, `mcp/server.rs`, `config.rs`) — a single `mini-app-mcp` daemon can now mount and serve multiple SQLite tables. Tables are discovered automatically from two directory layers: User scope (`~/.mini-app/<table>/`) as the base and Project scope (`{project_root}/.mini-app/<table>/`) as an override. A Project-level `schema.yaml` for a given table name fully replaces the User-level one (file-level swap, no field merging). The new `TableRegistry` struct (`mcp/registry.rs`) manages the `HashMap<String, Arc<Store>>` backing this.
- **`table` argument on all tools** (`mcp/server.rs`) — `info`, `create`, `get`, `list`, `update`, and `delete` now accept an optional `table: Option<String>` argument. In multi-table mode the argument is required; omitting it returns `MiniAppError::TableRequired` (`code: "TABLE_REQUIRED"`). Supplying an unknown name returns `MiniAppError::TableNotFound` (`code: "TABLE_NOT_FOUND"`). Tool descriptions and `server_info.instructions` have been updated to document the new semantics (§K-49 / §1-8-1).
- **New error variants** (`error.rs`) — `MiniAppError::TableNotFound { table: String }` and `MiniAppError::TableRequired`; both carry structured `code` fields through `From<MiniAppError> for McpError` so agents can handle them programmatically.
- **New environment variables** (`config.rs`) — `MINI_APP_USER_DIR` (default `~/.mini-app/`) and `MINI_APP_PROJECT_DIR` (default `./.mini-app/`) control the two directory layers. Both are optional; omitting them falls back to the defaults.

### Changed

- **`Config` struct** (`config.rs`) — extended with `user_dir: Option<PathBuf>` and `project_dir: Option<PathBuf>` alongside the existing `schema_path` / `db_path` fields. Legacy single-table mode (`MINI_APP_SCHEMA` + `MINI_APP_DB`) is fully preserved; when those variables are set the server behaves exactly as before with the specified table loaded as the default.
- **`MiniAppMcpServer`** (`mcp/server.rs`) — internal fields replaced by a `TableRegistry`. Legacy single-table startup mounts the one table under `default_table`, preserving all existing tool call semantics for callers that do not pass a `table` argument.

## [0.2.0] - 2026-05-03

### Added

- **MCP Resources** (`mcp/resources.rs`, `mcp/server.rs`) — six read-only Resources exposed alongside the existing tools: `schema://yaml` (raw schema file), `schema://json` (parsed `SchemaConfig` as JSON), `schema://json-schema` (draft-07 JSON Schema derived from `fields[]`, usable for client-side validation of `create` / `update` arguments), `docs://readme` (this README, embedded via `include_str!`), `docs://tools` (tool cheat sheet), and `docs://errors` (error code reference). `ServerCapabilities` now declares `resources` capability.

## [0.1.0] - 2026-05-01

### Added

- **Crate scaffold** — `mini-app-mcp` Rust crate with `Cargo.toml`, `lib.rs`, and `main.rs` (`--mcp` flag entry point via clap).
- **Schema parser** (`schema.rs`) — parses `schema.yaml` at startup into `SchemaConfig` / `FieldDef`; supports field types `string`, `number`, `boolean`, `array`, `object`; enforces `required` constraints. `schema.yaml` is the sole runtime source of truth for all field definitions.
- **Error types** (`error.rs`) — `MiniAppError` enum (Validation / NotFound / Schema / Storage / Io / Config variants) with `thiserror::Error` derive; `From<MiniAppError> for McpError` conversion that produces a structured JSON error object with a machine-readable `code` field on every MCP tool error path.
- **Config** (`config.rs`) — `Config::load()` reads `MINI_APP_SCHEMA` and `MINI_APP_DB` environment variables (with `.mini-app-mcp.env` dotenv fallback) to provide schema and database paths at startup.
- **SQLite store** (`store.rs`) — `Store` struct wrapping `Arc<Mutex<rusqlite::Connection>>`; fixed DDL (`rows` table with `id`, `data`, `created_at`, `updated_at` columns); CRUD methods (`create` / `get` / `list` / `update` / `delete`) bridged to async via `tokio::task::spawn_blocking`; JSON row validation against schema on write paths.
- **MCP server** (`mcp/server.rs`) — `MiniAppMcpServer` implementing `ServerHandler` with six MCP tools: `info`, `create`, `get`, `list`, `update`, `delete`; structured JSON error responses on all error paths; stdio transport via `rmcp`.
- **Dump / file-materialization** (`dump.rs`) — framework-level `on_change` / `on_delete` hooks that write each created or updated row as a Markdown file (format: `# <title>` heading, blank line, body). Enabled per-schema via the `dump:` section in `schema.yaml`. Default output path is `<cwd>/.mini-app/<table>/<id>.md`; overridable with `dump.dir`. Title and body field names are configurable via `dump.title_field` / `dump.body_field` (default `title` / `body`). File I/O runs inside `tokio::task::spawn_blocking` using `std::fs`, consistent with the existing store I/O pattern.
- **`DumpConfig` and `SyncMode`** (`dump.rs`) — new public types embedded in `SchemaConfig.dump` (optional, backward-compatible via `#[serde(default)]`). `SyncMode` accepts `write-only` (default) or `bidirectional` in YAML; bidirectional mode is reserved for a future release and emits a `tracing::warn!` at server startup when configured.
- **`SchemaConfig.dump` field** (`schema.rs`) — `Option<DumpConfig>` field added with `#[serde(default)]`; existing `schema.yaml` files without a `dump:` section continue to deserialize correctly.
- **Store dump hook integration** (`store.rs`) — `Store::create`, `Store::update`, and `Store::delete` now call `dump::on_change` / `dump::on_delete` after each successful database operation. A dump write failure propagates as a CRUD error (prevents silent DB-file divergence). `Store::open` logs a `tracing::warn!` when `sync: bidirectional` is configured but not yet implemented.
