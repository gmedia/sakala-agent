# Dashboard Agent API

Dokumen ini mendefinisikan endpoint yang diharapkan oleh connected mode agent. Dashboard tetap menjadi control plane dan harus mengautentikasi serta mengotorisasi setiap request agent.

## Authentication Headers

```http
Authorization: Bearer <agent-token>
X-Agent-Id: <agent-id>
```

Token harus diterbitkan secara aman, disimpan hashed oleh dashboard, dan tidak muncul dalam logs.

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

Lihat `examples/commands/deploy-project.json`. Protocol saat ini memakai `type` dan `status` berbentuk PascalCase, serta UUID untuk command/project/deployment identifiers.

## Error/Retry Direction

Foundation client memakai HTTP status failure sebagai error request dan melanjutkan worker loop. Policy retry, backoff per endpoint, idempotency key, lease expiry, dan command locking harus disepakati bersama dashboard sebelum connected mode dipakai untuk operasi runtime nyata.
