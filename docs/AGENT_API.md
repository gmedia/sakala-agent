# Sakala Agent API

Dokumen ini mendefinisikan endpoint `sakala-api` yang diharapkan oleh connected mode agent. Sakala API adalah control plane dan harus mengautentikasi serta mengotorisasi setiap request agent.

## Authentication Headers

```http
Authorization: Bearer <agent-token>
X-Agent-Id: <agent-id>
```

Token harus diterbitkan secara aman, disimpan hashed oleh `sakala-api`, dan tidak muncul dalam logs.

## Endpoints

| Method | Endpoint | Tujuan |
| --- | --- | --- |
| `GET` | `/api/agent/v1/commands` | Poll command tersedia untuk node. |
| `POST` | `/api/agent/v1/commands/{command}/claim` | Klaim command sebelum eksekusi. |
| `POST` | `/api/agent/v1/commands/{command}/events` | Kirim lifecycle event. |
| `POST` | `/api/agent/v1/commands/{command}/logs` | Kirim baris log teredaksi. |
| `POST` | `/api/agent/v1/commands/{command}/complete` | Tandai eksekusi berhasil. |
| `POST` | `/api/agent/v1/commands/{command}/fail` | Tandai eksekusi gagal. |
| `POST` | `/api/agent/v1/heartbeat` | Laporkan keberadaan/status node. |

## Command Response Shape

Endpoint polling wajib memakai envelope Laravel API Resource:

```json
{
  "data": [
    {
      "id": "b3c8cb55-3bc8-4725-a004-e69d9917d40b",
      "type": "DeployProject",
      "status": "Pending",
      "project_id": "ff66ed4a-6303-4be6-8ef4-63c28b112680",
      "deployment_id": "4f1f21ef-730d-42d5-a46d-d965353cb993",
      "payload": {
        "repository_url": "https://example.invalid/student/demo-app.git",
        "runtime_network": "sakala-runtime"
      }
    }
  ]
}
```

`type` dan `status` memakai PascalCase. Identifier command, project, dan deployment memakai UUID. Lihat juga `examples/commands/deploy-project.json`.

## Heartbeat Payload

`agent_id` tidak diulang di body karena identitas node berasal dari header `X-Agent-Id` yang telah diautentikasi.

```json
{
  "status": "ready",
  "hostname": "runtime-01",
  "runtime_network": "sakala-runtime",
  "capabilities": ["noop-runtime"],
  "metadata": {
    "version": "0.1.0"
  },
  "sent_at": "2026-06-23T08:00:00Z"
}
```

## Event and Log Payloads

Command identifier berada di URL endpoint dan tidak diulang pada body. `sakala-api` bertanggung jawab menghubungkan record ke command/deployment serta menetapkan sequence secara atomik.

```json
{
  "type": "runtime.noop.completed",
  "level": "info",
  "message": "Noop runtime completed command without host changes.",
  "metadata": {
    "executor": "noop"
  },
  "occurred_at": "2026-06-23T08:00:01Z"
}
```

```json
{
  "stream": "system",
  "message": "Foundation mode: no Docker, Caddy, or Railpack operation executed.",
  "recorded_at": "2026-06-23T08:00:02Z"
}
```

Failure memakai field stabil yang sama dengan persistence model API:

```json
{
  "error_code": "runtime_execution_failed",
  "error_message": "runtime executor failed: deployment failed"
}
```

## Error/Retry Direction

Client memakai timeout 10 detik, memperlakukan HTTP status failure sebagai request error, dan melanjutkan worker loop pada tick berikutnya. Policy retry, backoff per endpoint, idempotency key, lease expiry, dan command locking harus disepakati dengan kontrak `sakala-api` sebelum connected mode dipakai untuk operasi runtime nyata.
