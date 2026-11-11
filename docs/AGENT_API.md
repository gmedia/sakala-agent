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

### Field build opsional

Payload `DeployProject` boleh membawa `root_directory`, `build_command`, dan
`start_command`, contohnya di `examples/commands/deploy-project-build-config.json`.
Ketiganya tidak pernah dijalankan di shell host. Payload tanpa field ini berperilaku
seperti sebelumnya, dan nilai kosong berarti tidak diisi.

| Field | Dockerfile | Railpack |
| --- | --- | --- |
| `root_directory` | Menjadi konteks build, dan `Dockerfile` dicari di sana. | Menjadi direktori `railpack prepare` dan konteks build. |
| `build_command` | Tidak bisa dipakai, karena langkah build ditentukan Dockerfile. Agent mencatatnya di log. | Diteruskan sebagai `RAILPACK_BUILD_CMD`, dan berjalan di dalam build. |
| `start_command` | Menggantikan entrypoint dan command image: `docker run --entrypoint sh <image> -c <command>`. Image tanpa `sh` gagal dengan `runtime_app_crashed`. | Diteruskan sebagai `RAILPACK_START_CMD`, dan menjadi command image. |

`container_port` adalah port efektif pilihan API. Agent mengekspornya sebagai `PORT`,
memprobe-nya, dan membuat route ke sana.

Agent memvalidasi ulang `root_directory`, karena nilai ini memilih path di host.
Path absolut, `..`, backslash, dan control character ditolak sebelum proses apa pun
berjalan. Setelah checkout, path di-resolve: symlink yang keluar dari repository,
path yang tidak ada, dan yang bukan direktori ditolak dengan
`runtime_root_directory_invalid`. Symlink yang tetap di dalam repository diterima, dan
path hasil resolve itulah yang dipakai. `Dockerfile` berupa symlink ke luar checkout
gagal dengan `runtime_build_failed`.

`InspectProject` juga menerima `root_directory` opsional dengan aturan yang sama. Bila
API mengirimnya, `railpack info` dan scanner manifest bekerja di direktori itu,
termasuk `dockerfile_found`. Tanpa field ini, inspection tetap memeriksa root
repository.

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
lifecycle.

Satu-satunya pengecualian adalah `StopProject` untuk project yang dihapus. Payload-nya
boleh membawa `remove_images: true` (boolean, opsional, default `false`):

```json
{
  "type": "StopProject",
  "project_id": "ff66ed4a-6303-4be6-8ef4-63c28b112680",
  "deployment_id": "4f1f21ef-730d-42d5-a46d-d965353cb993",
  "payload": { "remove_images": true }
}
```

Setelah container dihapus, Agent menghapus semua image `sakala/project-<project_id>`
yang tidak dipakai container mana pun, berapa pun umurnya. Result menambah
`removed_images` dan `reclaimed_image_bytes` dengan arti yang sama seperti pada
`CleanupRuntime`. Bila Docker gagal di tahap ini, stop tetap sukses, Agent mengirim
event `workload.images.retained`, dan image GC menghapusnya kemudian. Payload `{}`
atau `null` tetap mempertahankan image, dan nilai yang bukan boolean ditolak dengan
`invalid_runtime_command`. Agent versi lama mengabaikan field ini, jadi image project
tersebut baru hilang lewat image GC. Semantik lengkap `Stop` versus `Sleep`, hasil health, dan route
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

## Pembatalan command

`GET /api/agent/v1/commands` boleh membawa `cancel_requested` di samping `data`, contohnya
di `examples/commands/poll-cancel-requested.json`:

```json
{ "data": [], "cancel_requested": ["b3c8cb55-3bc8-4725-a004-e69d9917d40b"] }
```

Agent tetap melakukan poll selama command berjalan, termasuk saat slot penuh atau node
draining, karena eksekusi berjalan di task terpisah dari loop polling. Untuk tiap ID yang
sedang dijalankannya, Agent membatalkan cancellation token command itu. ID yang sama
boleh berulang, dan ID lain diabaikan.

| Saat pembatalan tiba | Yang terjadi | Report |
| --- | --- | --- |
| Checkout, build, start, atau readiness | Process group dimatikan. Candidate, image, dan workspace dihapus. | `fail` dengan `runtime_cancelled` |
| Aktivasi route sedang berjalan | Aktivasi dibiarkan selesai, karena menghentikannya di tengah bisa merusak route deployment yang sedang live. | `complete`, atau `fail` dengan `runtime_routing_failed` bila aktivasinya sendiri gagal |
| Setelah cutover | Deployment diselesaikan. | `complete` |
| Command yang tidak memeriksa token, seperti `RestartProject` | Berjalan sampai selesai. | Hasil aslinya |

Command yang dibatalkan tidak mengirim output aplikasi, karena tidak ada kegagalan
aplikasi yang perlu dijelaskan.

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
UUID owned Agent, sedangkan route harus memiliki nama UUID serta marker ownership
yang cocok sebelum dihapus dan Caddy di-reload.

Image hanya dipilih bila berlabel `dev.sakala.managed=true`, punya label
`dev.sakala.project-id` yang valid, dan semua namanya `sakala/project-<project_id>:<tag>`.
Image dangling atau yang di-tag ulang dengan nama lain tidak pernah dipilih.

Per project yang masih punya container (berlabel project itu, atau memakai salah satu
image-nya), Agent mempertahankan setiap image yang dipakai container mana pun (berjalan
atau berhenti), image deployment sebelumnya (image terbaru yang lebih tua dari image
aktif terbaru, atau image terbaru bila tidak ada yang aktif), dan semua image yang lebih
muda dari `SAKALA_IMAGE_GC_MAX_AGE_SECONDS`. Project tanpa container sama sekali, misalnya
project yang sudah dihapus, hanya mempertahankan image yang lebih muda dari
`SAKALA_ORPHANED_IMAGE_GRACE_SECONDS` (default 24 jam, dihitung dari waktu image dibuat);
image terbarunya ikut dihapus karena tidak ada yang akan di-rollback.

Sisanya dihapus dengan `docker image rm` tanpa `--force`, jadi Docker tetap menolak
image yang mulai dipakai container setelah pemilihan. Selain lewat command ini, Agent
menjalankan aturan yang sama sendiri setiap `SAKALA_IMAGE_GC_INTERVAL_SECONDS`, kecuali
saat ada command aktif. `startup_reconciliation.stale_images` pada heartbeat memuat
daftar yang sama, tanpa menghapus apa pun.

Result menambah `removed_images`, jumlah image yang terhapus. `reclaimed_image_bytes`
adalah jumlah ukuran image itu menurut Docker, termasuk layer yang masih dipakai image
lain, jadi angka ini batas atas ruang disk yang kembali.

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
    "exposed_ports": [],
    "railpack": {}
  }
}
```

`exposed_ports` berisi port TCP yang dideklarasikan `EXPOSE` di stage terakhir
`Dockerfile` pada direktori yang diinspeksi, terurut dan tanpa duplikat, paling banyak
32 entri. Field ini selalu dikirim, dan berupa `[]` bila tidak ada Dockerfile, tidak
ada `EXPOSE`, atau tidak ada port yang bisa dipastikan. Aturannya mengikuti BuildKit:

- Hanya stage terakhir yang dihitung, karena Agent tidak memakai `--target`. Stage
  yang dibangun dari stage sebelumnya (`FROM build`) mewarisi port dan variabelnya.
- Bila stage terakhir, termasuk stage yang menjadi dasarnya, tidak punya `EXPOSE` sama
  sekali, port diambil dari `Config.ExposedPorts` base image-nya di registry. Contohnya
  `FROM nginx:alpine` tanpa `EXPOSE` menghasilkan `[80]` karena image nginx
  mendeklarasikan `EXPOSE 80`. Lookup ini hanya untuk registry publik tertentu, tanpa
  credential, tanpa menarik image, dan dibatasi 10 detik. Bila gagal atau dilewati,
  hasilnya `[]`. Begitu Dockerfile punya `EXPOSE`, port base image tidak ditambahkan.
  Detail dan batasnya ada di [Docker Runtime](DOCKER_RUNTIME.md#port-dari-base-image).
- `80`, `80/tcp`, dan rentang `8000-8002` dihitung. Port `/udp` dan `/sctp` tidak.
- `$NAME`, `${NAME}`, `${NAME:-word}`, dan `${NAME:+word}` diisi dari `ARG` dan `ENV`
  di stage itu, dengan yang belakangan menang seperti di BuildKit. `ARG` global perlu
  dideklarasikan ulang di stage. Nilai yang tidak bisa dipastikan, misalnya variabel
  dari base image atau `ARG` tanpa default, diabaikan beserta port-nya. Agent tidak
  mengisi `ARG` dari environment project, jadi default di Dockerfile adalah nilainya.
- Komentar, continuation, directive `# escape=`, dan isi heredoc diperlakukan seperti
  BuildKit. Instruction lain diabaikan, jadi Dockerfile yang tidak bisa dibaca
  sepenuhnya tetap menghasilkan port yang bisa dibaca.
- `Dockerfile` berupa symlink hanya dibaca bila tujuannya tetap di dalam direktori yang
  diinspeksi, dan file di atas 1 MiB tidak dibaca.

API memakai `exposed_ports` untuk memilih port default; lihat
[Compatibility](COMPATIBILITY.md#revision-4-additions-after-v040). `container_port`
pada `DeployProject` tetap ditentukan API, dan `PORT` tetap kontraknya.

Untuk project dengan `Dockerfile`, kegagalan `railpack info` tidak menggagalkan
inspection, karena build Dockerfile tidak memakai Railpack. Railpack gagal, misalnya,
pada repository yang hanya berisi Dockerfile. `railpack` lalu bernilai `null`, dan
output Railpack tetap ada di log command. Tanpa Dockerfile, kegagalan itu tetap
menjadi `fail` dengan `runtime_build_failed`.

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
    "version": "0.5.0",
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
      "compatibility_issues": 0,
      "crashed_details": 0
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
      "unhealthy_details": [],
      "crashed_details": []
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
      "railpack": "ghcr.io/example/sakala-railpack@sha256:abc",
      "container_runtime": "runsc",
      "container_runtime_limits_enforced": false
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

`workloads.crashed_details` melaporkan workload managed yang sedang tidak berjalan
karena process-nya keluar sendiri, atau yang crash berulang.
Field ini opsional bagi API, bersifat aditif pada revision 4, dan contohnya ada di
`examples/heartbeat/crashed-workloads.json`:

```json
{
  "project_id": "ff66ed4a-6303-4be6-8ef4-63c28b112680",
  "deployment_id": "4f1f21ef-730d-42d5-a46d-d965353cb993",
  "exit_code": 137,
  "oom_killed": true,
  "finished_at": "2026-11-05T14:28:41Z",
  "restart_count": 8,
  "running": false
}
```

| Field | Tipe | Arti |
| --- | --- | --- |
| `project_id` | UUID | Project pemilik workload, dari label container. |
| `deployment_id` | UUID | Deployment pemilik workload, dari label container. |
| `exit_code` | integer atau `null` | Exit code run terakhir. Bisa `0`, karena aplikasi yang selesai dengan normal pun di-restart oleh policy `unless-stopped`. `null` berarti Docker tidak lagi punya nilainya. |
| `oom_killed` | boolean | Apakah run terakhir dimatikan kernel karena memory. `false` juga dipakai bila riwayatnya tidak tersedia, jadi `exit_code: null` berarti nilai ini tidak terbukti. |
| `finished_at` | RFC 3339 atau `null` | Waktu run terakhir berakhir, tanpa pecahan detik. |
| `restart_count` | integer | Jumlah restart oleh Docker sejak terakhir di-start dengan sengaja. |
| `running` | boolean | Apakah ada process yang berjalan saat ini. `false` berarti aplikasi sedang tidak berjalan. `true` berarti ia sedang hidup di antara dua crash, jadi UI sebaiknya menyebutnya crash berulang, bukan mati. |

Sebuah workload masuk list bila salah satu kondisi ini terpenuhi:

- Docker menunjukkan status `restarting` saat ini.
- Crash berulang: statusnya `running`, `RestartCount` minimal 3, dan `State.StartedAt`
  kurang dari 5 menit yang lalu.

Aplikasi yang crash satu atau dua kali lalu kembali melayani tidak masuk list, karena
ia memang sedang melayani. Agent mencatatnya di log operator. Satu-satunya saat ia
terlihat adalah bila heartbeat jatuh tepat pada jeda `restarting` Docker, yang pada
restart pertama hanya sepersekian detik. Aplikasi yang crash berulang tetap masuk list,
termasuk saat ia sebentar `running` di antara dua crash, jadi API tidak perlu debounce.
Begitu ia bertahan hidup 5 menit, ia hilang dari list. `RestartCount` tetap di atas 0
setelah itu, tetapi tidak dipakai lagi.

Cara membaca list ini:

- **Restart dan wake yang disengaja bukan crash.** Docker mengembalikan
  `RestartCount` ke 0 pada `docker restart` dan `docker start`, sudah diverifikasi
  pada Docker 29.9.2, sehingga `RestartProject` dan `WakeProject` langsung
  menghapus workload dari list.
- **Container `exited` tidak pernah dilaporkan.** `docker stop`, `docker kill`,
  dan `SleepProject` semuanya berakhir di `exited` dengan exit code apa pun
  (stop saat `restarting` terlihat sebagai `Exited (1)`), dan Docker tidak
  mengekspos apakah stop itu disengaja. Kasus langka yang ikut terlewat: Docker
  menyerah me-restart karena start gagal.
- **List yang hadir dan tidak memuat suatu workload** berarti workload itu tidak
  dianggap crash. **List yang tidak hadir** (`crashed_details` dan
  `detail_counts.crashed_details` sama-sama hilang) berarti Agent tidak tahu,
  karena query Docker gagal. API tidak boleh mengubah status dari heartbeat
  seperti itu; `workloads.unhealthy` juga `null` pada kondisi ini.
- **Candidate yang sedang gagal bisa ikut muncul** sampai cleanup selesai. API
  harus mencocokkan `deployment_id` dengan deployment aktif project.
- Workload berstatus `restarting` juga muncul di `unhealthy_details`, karena
  keduanya menjawab pertanyaan berbeda. Jangan menjumlahkan keduanya. Workload
  crash berulang yang sedang `running` hanya muncul di `crashed_details`.

Docker mengosongkan `State.ExitCode` dan `State.OOMKilled` begitu restart berhasil,
jadi nilai itu tidak bisa dibaca dari container yang sedang `running`. Agent
membacanya dari event `die` dan `oom` milik Docker, dan event `oom` hanya dihitung
untuk `die` berikutnya, sehingga worker yang mati karena memory pada run sekarang
tidak dianggap sebagai sebab exit terakhir. Riwayat event Docker terbatas dan hilang
saat daemon restart. Dalam kasus itu `exit_code` bernilai `null`, bukan `0`.

Satu heartbeat memakai listing batch `docker ps` yang sama dengan
`unhealthy_details`, lalu satu `docker inspect` batch atas container yang terdaftar,
dan satu `docker events` batch hanya bila ada exit yang perlu dibaca. Tidak ada query
per container, dan Agent tidak membaca environment atau secret container.

`startup_reconciliation` adalah snapshot recovery saat process Agent dimulai,
bukan inventaris runtime live. `captured_at` menjelaskan umur snapshot secara
eksplisit; status workload terkini berada pada bagian `workloads` heartbeat dan
hasil command reconciliation eksplisit.

Collection detail yang berpotensi membesar (`unhealthy_details`,
`crashed_details`, `recovered_workloads`, `orphans`, `stale_routes`, `stale_images`, dan
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

Tahap `DeployProject` dikirim sebagai event, tepat saat tahap itu dimulai, dalam
urutan berikut. API yang belum mengenal suatu `type` tetap menyimpannya di timeline
dan tidak mengubah status deployment, sehingga event baru aman untuk API lama.

| `type` | Kapan | Status deployment yang cocok |
| --- | --- | --- |
| `deployment.checkout.started` | Clone repository dimulai. | `cloning` |
| `deployment.analysis.started` | `railpack prepare` dimulai. Hanya pada build Railpack, karena memilih Dockerfile hanyalah memeriksa satu file. | `analyzing` |
| `deployment.build.started` | Build image dimulai. Pada Railpack, event ini kini datang setelah analisis. | `building` |
| `deployment.container.started` | `docker run` candidate akan dijalankan. | `deploying` |
| `deployment.health.started` | Container sudah jalan, probe readiness dimulai. | `health_checking` |
| `deployment.route.started` | Aplikasi sudah menjawab, route akan diaktifkan. | `routing` |
| `deployment.runtime.ready` | Route aktif dan cleanup selesai. Bukan penanda routing. | tidak ada; `succeeded` hanya dari `complete` |

Health check selalu terjadi sebelum routing. Route tidak boleh aktif sebelum
aplikasi terbukti menjawab.

Semua message dan metadata event, baris log, dan pesan `fail` milik `DeployProject`
melewati redaksi Agent: nama key, prefix token, dan nilai environment command itu.
Nilai di bawah 8 karakter dan kata umum seperti `production` atau `localhost` tidak
diredaksi agar log tetap terbaca. Pencocokan bersifat persis, jadi bentuk terenkode
seperti base64 atau URL-encoded tidak tertangkap, dan API tetap harus meredaksi
ulang. Follower yang dipulihkan setelah Agent restart membaca nilai itu dari
`docker inspect` container-nya, karena Agent tidak menyimpan payload command.

API menjawab `200` dengan acknowledgement berikut. `accepted_count` adalah
jumlah seluruh item pada request dan `duplicate_count` adalah subset yang
sudah pernah tersimpan di bawah `Idempotency-Key` yang sama. Semua field wajib
ada dan `accepted_count` harus sama dengan jumlah item yang dikirim;
acknowledgement parsial dianggap tidak terkirim. `200` yang body-nya tidak
dapat dibaca di-retry dengan key yang sama,
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
dan zero-width character lalu dipotong pada 1000 karakter. Pemotongan ini tidak
memberi tanda apa pun, jadi fakta penting ditaruh di awal pesan. Semua pesan pada
tabel di bawah jauh lebih pendek dari batas itu.

### Kegagalan yang disebabkan aplikasi

Code berikut dikirim saat aplikasi user yang bermasalah, bukan node-nya, sehingga
pesannya aman ditampilkan ke user dan menyebut fakta konkret. Pesan persisnya ada
di `examples/failures/deploy-failures.json`, yang dicocokkan dengan kode oleh test.

| `error_code` | Kapan | Fakta di pesan |
| --- | --- | --- |
| `runtime_app_not_listening` | Sampai deadline start tidak ada yang menjawab pada port. Termasuk aplikasi yang hanya listen di `127.0.0.1`. | Port, lama menunggu, dan kewajiban listen di `0.0.0.0:$PORT`. |
| `runtime_app_unhealthy` | Aplikasi hanya menjawab 5xx sampai deadline. | Port dan status terakhir. |
| `runtime_app_crashed` | Process keluar selama start. Satu restart saja sudah dihitung crash. | Exit code, atau keterangan bahwa Docker tidak lagi melaporkannya. |
| `runtime_app_exec_denied` | Kernel menolak menjalankan program di image dengan `Operation not permitted`: exit code 126 dari shell, atau 255 dari runc, disertai baris itu di output terakhir aplikasi. Di container Sakala penyebabnya file capability di luar [allowlist workload](DOCKER_RUNTIME.md#capability-workload), misalnya `cap_net_raw`. | Exit code, contoh `NET_RAW`, dan saran `setcap -r` atau image lain. Tidak menyebut environment variable. |
| `runtime_app_out_of_memory` | Container keluar dengan `State.OOMKilled` atau event `oom`, atau masih hidup dengan flag itu tetapi tidak pernah menjawab. | Batas memory yang diterapkan. |
| `runtime_port_conflict` | `PORT` dari environment berbeda dari `container_port`. Menggantikan `invalid_runtime_command` untuk kasus ini. | Kedua nilai. `PORT` yang bukan nomor port tidak diulang, hanya disebut tidak valid. |
| `runtime_root_directory_invalid` | `root_directory` tidak aman atau tidak menunjuk direktori di dalam repository. | Alasannya, tanpa mengulang path. |

`runtime_app_exec_denied` dideteksi dari output aplikasi sendiri, jadi aplikasi bisa
memicunya dengan mencetak baris yang sama. Akibatnya hanya pesan untuk deployment itu
sendiri yang berubah. Code ini belum dikenal classifier API saat ditulis; saran
pemetaannya: kategori start, bisa diperbaiki user, dengan hint bahwa program di image
meminta capability Linux yang tidak diberikan Sakala, tanpa menyebut environment variable.

Pesan tidak pernah memuat nilai environment selain nomor port yang valid, token,
atau path secret, dan tidak memuat output probe, karena sebagian output itu
ditulis aplikasi. Output aplikasi dikirim terpisah melalui endpoint logs.

### Kegagalan registry atau jaringan saat build

`runtime_registry_unavailable` dikirim saat build gagal menarik image karena registry
atau jaringan tidak menjawab, misalnya DNS gagal (`server misbehaving`), `i/o timeout`,
TLS handshake timeout, koneksi diputus, rate limit `429 Too Many Requests`, atau 502,
503, dan 504. Kegagalan apa pun saat menarik image Railpack sendiri
(`ghcr.io/railwayapp/railpack-*`) juga masuk ke sini, karena image itu milik platform.
Ini gangguan platform, bukan repository, jadi API sebaiknya menyarankan deploy ulang.
Classifier API sudah memetakan prefix `runtime_registry` ke kategori node sebagai
gangguan platform.

Yang tetap `runtime_build_failed`: image yang tidak ada (`not found`), image privat atau
salah nama (`pull access denied`), host registry yang tidak ada (`no such host`), dan
kegagalan jaringan di langkah build milik repository seperti `RUN curl` atau `npm
install`. Semua itu bisa diperbaiki di repository. Pesan code ini hanya menyebut tahap
dan exit code, tanpa nama registry atau image; detailnya ada di log build. Agent membaca
1 MiB pertama stderr build, tempat penarikan image terjadi.

Dua kegagalan readiness sengaja tidak memakai code di atas:

- `HEALTHCHECK` image yang `unhealthy`, atau masih `starting` sampai deadline,
  tetap memakai `runtime_health_check_failed`.
- Probe yang tidak bisa berjalan memakai `runtime_dependency_failed`, karena
  kesalahannya ada pada node: `wget` tidak ada di container Caddy, container Caddy
  mati, atau nama workload tidak ter-resolve dari Caddy.

Kegagalan sebelum route aktif pada fase start, readiness, atau routing juga
mengirim hingga 100 baris terakhir aplikasi dari stdout dan stderr, sebelum
candidate dihapus dan sebelum `fail`. Jumlah barisnya mengikuti
`log_bounds.max_batch_lines` bila lebih kecil, dan seluruh baris melewati redaksi
serta batas `log_bounds` pada reporter. Kegagalan mengirim log tidak mengubah
`error_code`. Command yang dibatalkan tidak mengambil log ini.

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
