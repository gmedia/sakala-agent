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
| `GET` | `/api/agent/v1/node-state` | Ambil desired lifecycle node sebelum scheduler mulai polling. |
| `POST` | `/api/agent/v1/commands/{command}/claim` | Klaim command sebelum eksekusi. |
| `POST` | `/api/agent/v1/commands/{command}/repository-credential` | Lease credential sementara untuk repository private. |
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

### Private repository credential lease

Agent hanya memanggil endpoint berikut setelah command berhasil di-claim dan
payload memakai `repository_access: "temporary_credential"`:

```http
POST /api/agent/v1/commands/{command}/repository-credential
```

API wajib memverifikasi bahwa command dimiliki agent peminta, repository cocok
dengan command, authorization user/workspace masih valid, dan credential hanya
memiliki akses minimum ke satu repository (`contents:read`). Credential harus
short-lived, tidak disimpan pada record command, serta tidak dimasukkan ke log
atau response lain.

Response endpoint ini sengaja memakai object langsung, **bukan** envelope
Laravel `{ "data": ... }`:

```json
{
  "username": "x-access-token",
  "token": "ghs_ephemeral_installation_token"
}
```

Nilai kosong ditolak Agent. Token hanya diteruskan melalui environment
`GIT_ASKPASS`, tidak pernah dimasukkan ke repository URL atau process arguments.

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

Connected Agent wajib memanggil `GET /api/agent/v1/node-state` saat bootstrap,
sebelum scheduler dapat mengambil atau mengklaim command. Respons menggunakan
envelope berikut:

```json
{
  "data": {
    "desired_state": "drained"
  }
}
```

Nilai yang valid adalah `active`, `draining`, `drained`, dan `maintenance`.
Control plane merupakan sumber kebenaran state ini: API harus menyimpan desired
state secara atomik sebelum menawarkan `DrainNode`/`ResumeNode`. Bila bootstrap
state gagal diambil, connected Agent berhenti secara fail-closed dan tidak
mengklaim pekerjaan baru. Local mode selalu mulai sebagai `active`.

Sebelum kembali `active`, `ResumeNode` menjalankan preflight dan mengambil
snapshot kapasitas lokal. Completion result menyertakan
`capacity.active_workloads`, `maximum_active_workloads`, dan
`available_workload_slots`; nilai `null` berarti driver tidak dapat menentukan
nilai tersebut dengan aman. API harus memperlakukannya sebagai telemetry, bukan
izin untuk melewati safety limit node.

## Polling and Claim Semantics

Polling bukan pemberian ownership. Endpoint `GET /api/agent/v1/commands` hanya
mengembalikan command `Pending` yang eligible untuk agent/node terautentikasi.
Agent wajib memanggil endpoint claim sebelum melakukan inspection atau
perubahan runtime.

Claim harus dilakukan atomik di `sakala-api`, misalnya melalui conditional
update atau transaction yang memastikan status masih `Pending`. Hanya satu
agent boleh menerima claim sukses. Bila claim mendapat conflict karena command
sudah diklaim, dibatalkan, kedaluwarsa, atau tidak lagi eligible, agent harus
melewati command tersebut tanpa menjalankannya. Claim sengaja tidak di-retry
oleh Agent: retry setelah kegagalan jaringan yang ambigu akan menerima `409`
untuk claim pertamanya sendiri dan melewati command tersebut.

Respons claim sukses membawa resource command penuh (termasuk payload yang
sudah dimaterialisasi). Agent saat ini mengabaikan body tersebut dan tetap
memakai record hasil polling sebagai sumber payload; lihat
[Compatibility and Release Policy](COMPATIBILITY.md).

```txt
Pending in database
-> returned by poll when eligible
-> atomic claim succeeds for one agent
-> execute and report
-> complete or fail
-> no longer returned by normal polling
```

Command yang sudah `Claimed`, `Running`, `Succeeded`, `Failed`, `Cancelled`,
atau `Expired` tidak boleh dikembalikan lagi sebagai pekerjaan pending. Polling
berikutnya hanya mengembalikan pekerjaan baru atau pekerjaan yang secara
eksplisit dikembalikan ke antrean oleh policy lease/recovery API.

Agent tidak menyediakan HTTP server. Semua endpoint dalam dokumen ini dimiliki
oleh `sakala-api`; agent bertindak sebagai HTTP client outbound.

### Semantik control plane yang diandalkan Agent

- **`Claimed -> Running`.** Transisi ini dilakukan API pada report pertama dari
  node pemilik; event `command.claimed` yang dikirim Agent segera setelah claim
  sudah cukup. Tidak ada endpoint yang perlu dipanggil.
- **Lease.** Claim memberi lease `lease_expires_at = claimed_at +
  command_timeout_seconds + SAKALA_AGENT_LEASE_GRACE_SECONDS` (default API 60
  detik). Setelah lewat, API menandai command `Expired`; `complete`/`fail`
  berikutnya dijawab `409 {"status":"Expired"}` dan Agent memperlakukannya
  sebagai konflik terminal. Karena Agent menegakkan `command_timeout_seconds`
  sendiri, lease normalnya hanya kedaluwarsa bila node mati atau kehilangan
  koneksi.
- **Offline.** Node diturunkan `offline` oleh API bila tidak mengirim heartbeat
  selama `SAKALA_AGENT_OFFLINE_AFTER_SECONDS` (default 60 detik) dan tidak
  ditawari command apa pun sampai heartbeat berikutnya. Interval heartbeat
  default Agent (10 detik) aman; jangan menaikkannya mendekati batas tersebut
  tanpa koordinasi.
- **Pinning.** `DeployProject` dan `InspectProject` adalah *pinned command
  type*: API menentukan node target deterministik sebelum command ditawarkan
  atau diklaim. Bila belum ada node eligible, command menunggu tanpa target dan
  tidak terlihat oleh node mana pun. Payload sensitif (`environment`
  plaintext) hanya dimaterialisasi untuk node target. Project yang sudah
  berjalan di sebuah node hanya dideploy ulang ke node itu; tidak ada migrasi
  lintas node implisit.
- **Policy.** `ReconcileWorkload` diblokir untuk project suspended dan
  dibatalkan (`Cancelled`, `reconcile_target_superseded`) saat claim bila
  deployment target sudah disusul. `CleanupRuntime` hanya dibuat atas
  persetujuan admin; `approved: true` ditulis oleh API, bukan client.

### Idempotensi terminal command

`claim`, `complete`, dan `fail` wajib bersifat aman terhadap retry jaringan.
API harus memakai transisi atomik dan tidak boleh mengubah terminal state:

| Request | State saat ini | Respons yang diharapkan | Perilaku Agent |
| --- | --- | --- | --- |
| `claim` | bukan `Pending` | `409 Conflict` dengan state saat ini | Jangan eksekusi command. |
| `complete` | sudah `Succeeded` dengan command yang sama | `204 No Content` | Anggap sukses idempoten. |
| `fail` | sudah `Failed` dengan command yang sama | `204 No Content` | Anggap gagal yang sama telah tercatat. |
| `complete` | `Failed`/`Cancelled`/`Expired` | `409 Conflict` | Jangan menimpa state terminal. |
| `fail` | `Succeeded`/`Cancelled`/`Expired` | `409 Conflict` | Jangan menimpa state terminal. |

`complete` dan `fail` di-retry Agent dengan backoff terbatas hanya untuk
kegagalan transport dan respons `408`/`429`/`5xx`; respons lain diproses apa
adanya.

Respons conflict harus menyertakan state command yang aman untuk ditampilkan
(`status` dan, bila relevan, `terminal_at`), tetapi tidak boleh memantulkan
payload deployment atau credential. Retry command polling tidak memberi izin
untuk menjalankan ulang command yang tidak berhasil di-claim.

Bentuk respons `409` yang ditetapkan adalah:

```json
{
  "status": "Succeeded",
  "terminal_at": "2026-09-14T10:00:00+00:00"
}
```

`terminal_at` adalah `completed_at` untuk `Succeeded`, `failed_at` untuk
`Failed`, dan `null` untuk state lain. Agent menyertakannya pada pesan konflik
terminal bila ada.

## Desired versus actual workload state

Control plane tetap menjadi pemilik desired state. Untuk reconciliation,
command `ReconcileWorkload` mengirim identity workload, desired state, dan aksi
lokal yang memang diotorisasi:

```json
{
  "type": "ReconcileWorkload",
  "project_id": "ff66ed4a-6303-4be6-8ef4-63c28b112680",
  "deployment_id": "4f1f21ef-730d-42d5-a46d-d965353cb993",
  "payload": {
    "desired_state": "running",
    "actions": ["restart_log_follower", "restore_route"]
  }
}
```

Agent menyelesaikan command dengan completion result berikut (identity
project/deployment sudah ada pada record command sehingga tidak diulang):

```json
{
  "result": {
    "desired_state": "running",
    "actual_state": "missing",
    "in_sync": false,
    "drift_reason": "workload_state_mismatch",
    "container_id": null,
    "actions_applied": []
  }
}
```

Nilai `actual_state` adalah `running`, `stopped`, atau `missing`; kesehatan
workload running dilaporkan terpisah melalui `workloads.unhealthy_details` pada
heartbeat, bukan sebagai state reconciliation. `drift_reason` bernilai
`workload_state_mismatch` bila desired dan actual berbeda, selain itu `null`.
`actions_applied` memuat satu objek per aksi yang dijalankan, misalnya
`{"action":"restart_log_follower","started":true,"command_id":"<deploy command id>"}`,
`{"action":"cleanup_failed_candidate","container_id":"..."}`, atau
`{"action":"restore_route"}`.

`restart_log_follower` memasang follower di bawah identitas command
`DeployProject` asli milik workload (label `dev.sakala.command-id` dan batas
log pada container), bukan command `ReconcileWorkload` yang memerintahkannya:
API hanya menerima log setelah terminal untuk `DeployProject`, sehingga
follower yang terikat ke command reconcile akan ditolak `409` begitu command
itu selesai. Workload tanpa label command-id (generasi lama) ditolak dengan
`invalid_runtime_command` dan perlu redeploy.

Bila desired dan actual berbeda, Agent melaporkan drift tetapi tidak melakukan
restart, deploy, atau delete secara otomatis. Control plane memutuskan policy
recovery dan bila perlu mengirim command lifecycle eksplisit. Tanpa `actions`,
`ReconcileWorkload` selalu read-only. Aksi `cleanup_failed_candidate` ditolak
kecuali workload Sakala ditemukan dalam state `Created`, `Exited`, atau `Dead`.

### Approved runtime cleanup

Cleanup node menggunakan command terpisah agar authorization destruktif mudah
diaudit dan tidak tersirat dari heartbeat:

```json
{
  "id": "b3c8cb55-3bc8-4725-a004-e69d9917d40b",
  "type": "CleanupRuntime",
  "status": "Pending",
  "project_id": null,
  "deployment_id": null,
  "payload": {
    "approved": true,
    "targets": ["stale_workspaces", "stale_images", "stale_routes"]
  }
}
```

Agent menolak `approved: false` dan target kosong. Workspace dibatasi direktori
UUID owned Agent, image memakai label `dev.sakala.managed=true` dan hanya
dangling image yang melewati umur minimum, sedangkan route harus memiliki nama
UUID serta marker ownership yang cocok sebelum dihapus dan Caddy di-reload.

## Project Inspection Command

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

Nilai `status` heartbeat yang valid adalah `ready`, `busy`, `degraded`,
`draining`, `drained`, dan `maintenance`. Builder heartbeat saat ini hanya
menghasilkan `ready`, `degraded`, `draining`, `drained`, dan `maintenance`;
`busy` dicadangkan pada enum tetapi belum pernah dikirim karena tekanan
kapasitas dilaporkan melalui `execution` dan `workloads`. Status `offline`
hanya diturunkan control plane dari heartbeat yang terlambat, bukan dikirim
Agent. Heartbeat adalah summary kesehatan dan
telemetry operasional node, bukan endpoint full inventory runtime. Karena itu,
telemetry yang tidak dapat diketahui runtime dengan aman boleh bernilai `null`;
`null` tidak boleh ditafsirkan sebagai angka nol atau kapasitas tanpa batas.

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
    "protocol_version": 4,
    "runtime_driver": "docker",
    "lifecycle_state": "active",
    "uptime_seconds": 86400,
    "detail_counts": {
      "unhealthy_details": 0,
      "recovered_workloads": 0,
      "orphans": 0,
      "stale_routes": 0,
      "stale_images": 0,
      "compatibility_issues": 0
    },
    "resources": {
      "cpu_total": 4,
      "cpu_load_1m": 0.42,
      "memory_total_bytes": 8589934592,
      "memory_available_bytes": 4294967296,
      "disk_total_bytes": 107374182400,
      "disk_available_bytes": 53687091200,
      "workspace_used_bytes": 104857600
    },
    "workloads": {
      "active": 2,
      "starting": 0,
      "unhealthy": 0,
      "stopped": 1,
      "unhealthy_details": []
    },
    "disk_pressure": {
      "state": "normal",
      "minimum_workspace_free_bytes": 2147483648,
      "available_workspace_bytes": 53687091200
    },
    "runtime_dependencies": {
      "git": "git version 2.47.0",
      "docker": "27.3.1",
      "buildx": "github.com/docker/buildx v0.17.1",
      "railpack": "railpack 0.23.0"
    },
    "execution": {
      "active_commands": 1,
      "queued_local_commands": 0,
      "capacity_waiting_commands": 1,
      "active_builds": 1,
      "maximum_concurrent_builds": 2
    },
    "startup_reconciliation": {
      "captured_at": "2026-06-23T07:59:58Z",
      "inspected_containers": 2,
      "cleaned_workspaces": 0,
      "reattached_log_followers": 1,
      "recovered_execution_records": 2,
      "recovered_workloads": [],
      "orphans": [],
      "stale_routes": [],
      "stale_images": [],
      "compatibility_issues": []
    }
  },
  "sent_at": "2026-06-23T08:00:00Z"
}
```

Bentuk item pada collection detail `startup_reconciliation`:

```json
{
  "recovered_workloads": [{ "container_id": "…", "project_id": "…", "deployment_id": "…", "status": "Up 3 minutes" }],
  "orphans": [{ "container_id": "…", "project_id": "…", "reason": "stale stopped deployment container" }],
  "stale_routes": [{ "path": "/var/lib/sakala/caddy/sites/….Caddyfile", "project_id": "…", "deployment_id": "…" }],
  "stale_images": [{ "image_id": "…", "project_id": "…", "deployment_id": "…" }],
  "compatibility_issues": [{ "container_id": "…", "project_id": "…", "deployment_id": "…", "reason": "…" }]
}
```

`project_id`/`deployment_id` bernilai `null` bila label tidak dapat dibaca;
`stale_routes.deployment_id` juga `null` untuk route generasi legacy tanpa
deployment marker. `workloads.unhealthy_details` memakai bentuk
`{ container_id, project_id, deployment_id, status, reason }`.

`startup_reconciliation` adalah snapshot recovery saat process Agent dimulai,
bukan inventaris runtime live. `captured_at` menjelaskan umur snapshot secara
eksplisit; status workload terkini berada pada bagian `workloads` heartbeat dan
hasil command reconciliation eksplisit.

Collection detail yang berpotensi membesar (`unhealthy_details`,
`recovered_workloads`, `orphans`, `stale_routes`, `stale_images`, dan
`compatibility_issues`) dibatasi maksimal 50 item per heartbeat. Agent
mempertahankan urutan hasil runtime dan mengirim 50 item pertama secara
deterministik, bukan memilih item secara acak. `metadata.detail_counts` selalu
menyimpan jumlah asli sebelum pembatasan; nilai `0` berarti collection kosong,
sedangkan count yang lebih besar dari jumlah item yang dikirim menandakan ada
detail yang dipotong. `compatibility_issues` adalah temuan container yang
metadata/label-nya belum kompatibel untuk recovery saat ini, bukan inventaris
lengkap atau izin mutasi runtime.

Batas ukuran serialized heartbeat 256 KiB tetap merupakan enforcement terakhir di
API. Agent membatasi collection sumber seperti di atas dan tidak menggagalkan
heartbeat hanya karena perkiraan ukuran JSON.

## Event and Log Payloads

Command identifier berada di URL endpoint dan tidak diulang pada body.
`sakala-api` bertanggung jawab menghubungkan record ke command/deployment serta
menetapkan sequence secara atomik.

Agent mengirim report dalam bentuk batch. Event dikirim segera, satu request
per event; log di-buffer dan dikirim per batch (maksimum 100 baris atau 512 KiB
message per request, atau setelah 200 ms) dalam urutan produksi. Setiap request
membawa header `Idempotency-Key` (UUID v4 unik per request) sehingga retry
transport aman: API mendeduplikasi per item berdasarkan key dan index batch.

```http
POST /api/agent/v1/commands/{command}/events
Idempotency-Key: 8b4a0d3e-6f7c-4a3e-9c3f-3f2f1c1c0a11
```

```json
{
  "events": [
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
  ]
}
```

```json
{
  "logs": [
    {
      "stream": "system",
      "message": "[docker-build] exporting to image",
      "recorded_at": "2026-06-23T08:00:02Z"
    },
    {
      "stream": "stdout",
      "message": "[docker-build] naming to sakala/project-…",
      "recorded_at": "2026-06-23T08:00:02Z"
    }
  ]
}
```

API menjawab `200` dengan acknowledgement berikut. `accepted_count` adalah
jumlah seluruh item pada request dan `duplicate_count` adalah subset yang
sudah pernah tersimpan di bawah `Idempotency-Key` yang sama. Semua field wajib
ada; `200` yang body-nya tidak dapat dibaca di-retry dengan key yang sama,
sedangkan `200` yang body-nya tidak sesuai kontrak dianggap **tidak
terkirim** (bukan sukses diam-diam) dan menghentikan delivery. `204` tanpa
body dari control plane lama tetap diterima sebagai sukses:

```json
{
  "data": {
    "accepted_count": 2,
    "duplicate_count": 0,
    "first_sequence": 1,
    "last_sequence": 2
  }
}
```

Respons yang diperlakukan Agent:

| Respons | Arti | Perilaku Agent |
| --- | --- | --- |
| `200` dengan acknowledgement valid, atau `204` | Diterima (duplikat tetap di-ack). | Lanjut. |
| `200` dengan body tidak sesuai kontrak | Tidak diketahui apakah tersimpan. | Hentikan delivery log command ini; batch tidak dianggap terkirim. |
| `408`, `429`, `5xx`, kegagalan transport | Transien. | Retry dengan key yang sama, backoff eksponensial 500 ms–5 s, maksimum 3 percobaan. |
| `409` | Command terminal, bukan node pemilik, atau `Idempotency-Key` dipakai untuk payload berbeda. | Hentikan delivery log command ini tanpa retry. |
| `422` | Budget `max_total_bytes` habis atau field tidak valid. | Hentikan delivery log command ini tanpa retry. |
| `413` | Body melebihi `SAKALA_LOG_MAX_REQUEST_BYTES` (1 MiB). | Hentikan delivery; batas lokal Agent sengaja jauh di bawah nilai ini. |

Log tetap dikirim **setelah** `complete` untuk `DeployProject`: setelah
cutover, Agent menjalankan `docker logs --follow` dan melaporkan output
container di bawah identitas command deploy yang sudah `Succeeded`, termasuk
setelah Agent restart (follower dipulihkan dari label container). Budget
kumulatif `max_total_bytes` tetap berlaku; `422` atau `409` menghentikan
follower tanpa retry. Event setelah command terminal selalu ditolak `409`.
Command `Failed`, `Cancelled`, atau `Expired` tidak menerima log lagi.

Sebelum `complete`/`fail`, Agent mem-flush batch log yang tersisa. Kegagalan
flush dicatat sebagai warning dan tidak mengubah hasil command.

Failure memakai field stabil yang sama dengan persistence model API:

```json
{
  "error_code": "runtime_execution_failed",
  "error_message": "runtime executor failed: deployment failed"
}
```

Agent menyanitasi body ini dengan aturan yang sama seperti API sebelum
mengirim: `error_code` dibatasi ke `[A-Za-z0-9._-]` (maksimum 64 karakter,
karakter lain menjadi `_`), `error_message` dibersihkan dari control, bidi,
dan zero-width character lalu dipotong pada 1000 karakter.

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

Jika route sudah committed tetapi finalisasi cleanup melewati grace Agent atau
mengembalikan error, command tetap sukses sesuai kondisi runtime dan result
membawa sinyal repair eksplisit:

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
    },
    "finalization_deferred": true,
    "finalization_deferred_reason": "grace_elapsed"
  }
}
```

`finalization_deferred_reason` bernilai `grace_elapsed` atau `runtime_error`.
Ketika `finalization_deferred=true`, API **wajib** mencari deployment sebelumnya
pada project/node yang sama dan mengirim `StopProject` idempoten untuk setiap
deployment lama tersebut. Target tidak boleh deployment baru pada command ini.
API tidak boleh menganggap startup reconciliation atau GC Agent akan menghapus
container lama yang masih running. Heartbeat/capacity dipakai untuk memastikan
repair selesai dan node kembali memiliki satu workload aktif yang diinginkan.

Command tanpa output mengirim `null`:

```json
{
  "result": null
}
```

## Error/Retry Direction

Client memakai timeout 10 detik per request dan memperlakukan HTTP status
failure sebagai request error. Polling dan heartbeat tidak di-retry; keduanya
melanjutkan worker loop pada tick berikutnya. Claim tidak di-retry (lihat
[Polling and Claim Semantics](#polling-and-claim-semantics)). Report event/log
serta `complete`/`fail` di-retry dengan backoff terbatas hanya untuk kegagalan
transient dan memakai `Idempotency-Key` yang sama pada setiap percobaan. Lease
expiry, offline detection, dan recovery command yang terputus dimiliki
`sakala-api` dan dijelaskan pada
[Semantik control plane yang diandalkan Agent](#semantik-control-plane-yang-diandalkan-agent).
