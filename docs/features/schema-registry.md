# Feature: Schema Registry

## What This Feature Does

This feature governs message contracts by:

- Registering JSON Schemas by schema ID
- Validating message content at publish and ingest
- Supporting strict mode (`schema_strict`) for rejecting unknown schemas/types

## When To Use It

Use this when you need to:

- Enforce structured payload contracts
- Prevent unknown/accidental content shapes
- Roll out schema-governed control-plane patterns

## Usage Examples

Enable strict mode at runtime:

```bash
cargo run -- --data-dir ./data --schema-strict
```

Register schema:

```bash
curl -X POST http://localhost:7654/v1/schemas \
  -H 'Content-Type: application/json' \
  -d '{
    "schema_id": "event/custom/v1",
    "content_type": "event/custom",
    "version": 1,
    "schema": {
      "$schema": "https://json-schema.org/draft/2020-12/schema",
      "type": "object",
      "properties": {"type": {"const": "event/custom"}},
      "required": ["type"]
    }
  }'
```

Validate content:

```bash
curl -X POST http://localhost:7654/v1/schemas/validate \
  -H 'Content-Type: application/json' \
  -d '{
    "content": {"type": "event/custom"},
    "schema_id": "event/custom/v1"
  }'
```

## CLI Configuration

| Flag | Purpose |
|---|---|
| `--schema-strict` | Enable strict schema enforcement |

## `config.yaml` Configuration

| Key | Purpose |
|---|---|
| `schema_strict` | Enable strict schema enforcement |

## API Surface

| Endpoint | Purpose |
|---|---|
| `GET /v1/schemas` | List schemas |
| `POST /v1/schemas` | Register schema |
| `GET /v1/schemas/*schema_id` | Read schema |
| `POST /v1/schemas/validate` | Validate content payload |

## Documentation Gaps

1. No CLI workflow exists for schema register/list/validate operations.
2. No MCP tools currently expose schema registry operations.
3. No documented migration checklist for upgrading schemas with live producers/consumers.
