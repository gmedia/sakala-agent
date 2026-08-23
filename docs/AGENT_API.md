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

### Arti `{command}`

`{command}` adalah **UUID field `id` milik satu `AgentCommand`** yang diterima
agent dari `GET /api/agent/v1/commands`. Ini bukan nilai `type`, bukan
`project_id`, dan bukan `deployment_id`.

Misalnya hasil polling mengandung:

```json
{
  "id": "b3c8cb55-3bc8-4725-a004-e69d9917d40b",
  "type": "DeployProject",
  "project_id": "ff66ed4a-6303-4be6-8ef4-63c28b112680",
  "deployment_id": "4f1f21ef-730d-42d5-a46d-d965353cb993"
}
```

Maka agent memakai ID command itu untuk seluruh lifecycle command yang sama:

```text
POST /api/agent/v1/commands/b3c8cb55-3bc8-4725-a004-e69d9917d40b/claim
POST /api/agent/v1/commands/b3c8cb55-3bc8-4725-a004-e69d9917d40b/events
POST /api/agent/v1/commands/b3c8cb55-3bc8-4725-a004-e69d9917d40b/logs
POST /api/agent/v1/commands/b3c8cb55-3bc8-4725-a004-e69d9917d40b/complete
```

`type` menjelaskan **pekerjaan apa** yang harus dilakukan, misalnya
`InspectProject` atau `DeployProject`. Sedangkan `id` command adalah identitas
rekam pekerjaan tersebut untuk claim, event, log, dan status akhirnya.

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
        "repository_url": "https://github.com/gmedia/example-app.git",
        "commit_sha": "0123456789abcdef0123456789abcdef01234567",
        "domain": "portfolio.run.sakala.localhost",
        "container_port": 3000,
        "builder": "auto",
        "environment": {
          "APP_ENV": "production"
        },
        "resources": {
          "memory_mb": 256,
          "cpu_millis": 500,
          "pids_limit": 128
        },
        "timeouts": {
          "build_timeout_seconds": 600,
          "start_timeout_seconds": 120,
          "command_timeout_seconds": 900
        },
        "log_bounds": {
          "max_line_length": 4096,
          "max_batch_lines": 500,
          "max_total_bytes": 10485760
        }
      }
    }
  ]
}
```

`type` dan `status` memakai PascalCase. Identifier command, project, dan deployment memakai UUID. Lihat juga `examples/commands/deploy-project.json`.

`sakala-api` menentukan `resources`, `timeouts`, dan `log_bounds` berdasarkan policy project/workspace/plan. `cpu_millis=500` berarti `0.5` vCPU. Agent menjalankan build, start/health, dan seluruh command menggunakan deadline payload, tetapi menolak timeout yang nol atau melebihi hard maximum node. Agent juga merahasiakan secret, membatasi panjang baris dan total byte log sebelum report; API tetap memvalidasi ulang log sebagai trust boundary terakhir. Network Docker tidak boleh berasal dari payload karena merupakan konfigurasi lokal runtime node.

### Workload Lifecycle Commands

Untuk `RestartProject`, `StopProject`, `SleepProject`, `WakeProject`,
`HealthCheck`, dan `RefreshRoute`, API mengirim `project_id` serta
`deployment_id` target dan memakai payload object kosong:

```json
{
  "id": "b3c8cb55-3bc8-4725-a004-e69d9917d40b",
  "type": "SleepProject",
  "status": "Pending",
  "project_id": "ff66ed4a-6303-4be6-8ef4-63c28b112680",
  "deployment_id": "4f1f21ef-730d-42d5-a46d-d965353cb993",
  "payload": {}
}
```

Identity tersebut dipetakan Agent ke label workload Sakala. API tidak boleh
mengirim nama Docker, domain, port, command shell, atau credential pada payload
lifecycle. Semantik lengkap `Stop` versus `Sleep`, hasil health, dan route
refresh berada di [Command Lifecycle](COMMAND_LIFECYCLE.md).

`DrainNode` dan `ResumeNode` adalah command node-level. Keduanya memakai
`project_id: null`, `deployment_id: null`, dan payload `{}`. Saat draining atau
drained, API hanya perlu menawarkan kedua command ini kepada node tersebut;
command workload lain dibiarkan pending sampai node kembali active.

## Polling and Claim Semantics

Polling bukan pemberian ownership. Endpoint `GET /api/agent/v1/commands` hanya mengembalikan command `Pending` yang eligible untuk agent/node terautentikasi. Agent wajib memanggil endpoint claim sebelum melakukan inspection atau perubahan runtime.

Claim harus dilakukan atomik di `sakala-api`, misalnya melalui conditional update atau transaction yang memastikan status masih `Pending`. Hanya satu agent boleh menerima claim sukses. Bila claim mendapat conflict karena command sudah diklaim, dibatalkan, kedaluwarsa, atau tidak lagi eligible, agent harus melewati command tersebut tanpa menjalankannya.

Command yang sudah `Claimed`, `Running`, `Succeeded`, `Failed`, `Cancelled`, atau `Expired` tidak boleh dikembalikan lagi sebagai pekerjaan pending. Polling berikutnya hanya mengembalikan pekerjaan baru atau pekerjaan yang secara eksplisit dikembalikan ke antrean oleh policy lease/recovery API.

```txt
Pending in database
-> returned by poll when eligible
-> atomic claim succeeds for one agent
-> execute and report
-> complete or fail
-> no longer returned by normal polling
```

Agent tidak menyediakan HTTP server. Semua endpoint dalam dokumen ini dimiliki oleh `sakala-api`; agent bertindak sebagai HTTP client outbound.

### Project Inspection Command

Create-project preview menggunakan command terpisah agar tidak menjalankan pipeline deployment:

```json
{
  "id": "b3c8cb55-3bc8-4725-a004-e69d9917d40b",
  "type": "InspectProject",
  "status": "Pending",
  "project_id": "ff66ed4a-6303-4be6-8ef4-63c28b112680",
  "deployment_id": null,
  "payload": {
    "repository_url": "https://github.com/gmedia/example-app.git",
    "commit_sha": "0123456789abcdef0123456789abcdef01234567"
  }
}
```

Setelah `railpack info` dan scanner selesai, agent menyelesaikan command dengan result berikut:

```json
{
  "result": {
    "repository_url": "https://github.com/gmedia/example-app.git",
    "commit_sha": "0123456789abcdef0123456789abcdef01234567",
    "dockerfile_found": false,
    "env_example_found": true,
    "compose_found": false,
    "manifests": [".env.example", "package.json", "pnpm-lock.yaml"],
    "package_manager": "pnpm",
    "railpack": {}
  }
}
```

Field stabil Sakala berada di tingkat atas `result`. Field `railpack` menyimpan raw JSON untuk audit dan evolusi adapter; console tidak boleh bergantung langsung pada struktur raw tersebut tanpa schema/normalization.

## Heartbeat Payload

`agent_id` tidak diulang di body karena identitas node berasal dari header `X-Agent-Id` yang telah diautentikasi.

`metadata.protocol_version` adalah revision contract Agent/API dan terpisah dari
semantic version binary. Lihat [Compatibility and Release Policy](COMPATIBILITY.md)
untuk aturan rollout dan command yang belum didukung.

```json
{
  "status": "ready",
  "hostname": "runtime-01",
  "runtime_network": "sakala-runtime",
  "capabilities": [
    "docker-runtime",
    "project-inspection",
    "dockerfile-build",
    "railpack-info",
    "railpack-build",
    "caddy-file-routing"
  ],
  "metadata": {
    "version": "0.1.0",
    "protocol_version": 1
  },
  "sent_at": "2026-06-23T08:00:00Z"
}
```

## Event and Log Payloads

Command identifier berada di URL endpoint dan tidak diulang pada body. `sakala-api` bertanggung jawab menghubungkan record ke command/deployment serta menetapkan sequence secara atomik.

```json
{
  "type": "deployment.runtime.ready",
  "level": "info",
  "message": "Application container and route are ready.",
  "metadata": {
    "builder": "dockerfile",
    "domain": "portfolio.run.sakala.localhost"
  },
  "occurred_at": "2026-06-23T08:00:01Z"
}
```

```json
{
  "stream": "system",
  "message": "[docker-build] exporting to image",
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

Completion selalu memakai envelope result. Deployment berhasil mengembalikan request dan limit aktual:

```json
{
  "result": {
    "requested_resources": {
      "memory_mb": 256,
      "cpu_millis": 500,
      "pids_limit": 128
    },
    "applied_resources": {
      "memory_mb": 256,
      "cpu_millis": 500,
      "pids_limit": 128
    }
  }
}
```

Command tanpa output mengirim `null`:

```json
{
  "result": null
}
```

## Error/Retry Direction

Client memakai timeout 10 detik, memperlakukan HTTP status failure sebagai request error, dan melanjutkan worker loop pada tick berikutnya. Build output dikirim per baris selama subprocess berjalan. Atomic claim merupakan requirement MVP. Policy retry, backoff per endpoint, idempotency key, lease expiry, dan recovery command yang terputus harus disepakati dengan kontrak `sakala-api` sebelum connected Docker mode dipakai pada node pilot.
