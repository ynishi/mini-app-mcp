# mini-app-mcp — Quickstart

Agent-First CRUD store backed by SQLite. One daemon mounts many tables;
each table's shape is defined by a `schema.yaml` file. CRUD is exposed
exclusively as MCP tools.

This document is the **agent quickstart** served via `docs://quickstart`:
server identity, mode detection, first-call recipe, and pointers to the
other documentation resources. The human-facing project README (build,
releases, contribution) lives at
https://github.com/ynishi/mini-app-mcp and is **not** served as an MCP
resource.

## Mode detection

The server runs in one of two modes. Determine which one at session
start by calling `info` with no `table` argument:

- **Multi-table mode** (when `MINI_APP_USER_DIR` and/or
  `MINI_APP_PROJECT_DIR` are set, or default to `~/.mini-app/` and
  `./.mini-app/`): `info` returns a `TABLE_REQUIRED` error
  (`data.code = "TABLE_REQUIRED"`). The `table` argument is **required**
  for every tool call. Discover mounted tables by inspecting the
  resource list — every `schema://yaml?table=<name>` URI corresponds to
  a mounted table.

- **Legacy single-table mode** (when `MINI_APP_SCHEMA` and `MINI_APP_DB`
  are set): `info` succeeds and returns the single mounted schema. The
  `table` argument may be **omitted** in every tool call; the configured
  table is used automatically.

Specifying an unknown `table` name returns `TABLE_NOT_FOUND`
(`data.code = "TABLE_NOT_FOUND"`).

## First-call recipe

1. **Discover schemas**: call `info` (multi-table: once per table;
   legacy: once with no `table`). The response carries
   `{ table, title, description, fields: [...] }`.
2. **List rows**: call `list` with optional `limit` / `offset` /
   `filter`. Results are ordered by `created_at` descending.
3. **Read a row**: call `get` with the row UUID.
4. **Mutate**: call `create` / `update` / `delete`. `update` defaults to
   RFC 7396 shallow merge; pass `mode: "replace"` to overwrite the
   entire `data` object.

For complex queries, store a reusable filter as a named alias
(`alias_create` → `alias_run`).

## Other documentation resources

- `docs://tools` — full per-tool reference (input shapes, outputs, MCP
  annotations) for all 18 tools.
- `docs://errors` — error code table with the structured `data` shapes
  returned alongside each `code`.
- `docs://filters` — how to construct `filter` objects (`Eq` / `In` /
  `Like` / `Or` / `And`) used by `list`, `alias_create`, and
  `row_materialize`.

## Schema resources

- `schema://yaml?table=<name>` — the raw `schema.yaml` file as-is.
- `schema://json?table=<name>` — the parsed `SchemaConfig` serialised as
  JSON. Same content as the `info` tool, fetched as a resource.
- `schema://json-schema?table=<name>` — a JSON Schema (draft-07) derived
  from the fields. Use this to validate `data` arguments locally before
  calling `create` / `update`.

In legacy single-table mode the `?table=<name>` query parameter may be
omitted on every schema URI.
