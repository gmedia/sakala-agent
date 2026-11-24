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
| `root_directory` | Menjadi konteks build, dan `Dockerfile` dicari di sana. | Menjadi direktori `railpack prepare` dan konteks build, kecuali bila ia package workspace JavaScript: build lalu berjalan dari root workspace ([Monorepo JavaScript](DOCKER_RUNTIME.md#monorepo-javascript)). |
| `build_command` | Tidak bisa dipakai, karena langkah build ditentukan Dockerfile. Agent mencatatnya di log. | Diteruskan sebagai `RAILPACK_BUILD_CMD`, dan berjalan di dalam build. Di package workspace menjadi `cd '<package>' && <command>`. |
| `start_command` | Menggantikan entrypoint dan command image: `docker run --entrypoint sh <image> -c <command>`. Image tanpa `sh` gagal dengan `runtime_app_crashed`. | Diteruskan sebagai `RAILPACK_START_CMD`, dan menjadi command image. Di package workspace menjadi `cd '<package>' && <command>`. |

`container_port` adalah port efektif pilihan API. Agent mengekspornya sebagai `PORT`,
memprobe-nya, dan membuat route ke sana.

`domain` harus berupa tepat satu label DNS huruf kecil ditambah salah satu suffix pada
`SAKALA_PROJECT_DOMAIN_SUFFIXES` node, misalnya `portfolio.sakala.app` atau
`portfolio.staging.sakala.app`. Domain lain, termasuk subdomain bertingkat, ditolak dengan
`invalid_runtime_command` sebelum checkout. Lihat [Domain project](CONFIGURATION.md#domain-project).

Agent menyimpan `domain` sebagai label `dev.sakala.domain` di container. `WakeProject` dan
`RestartProject` membangun ulang route dari label itu, bukan dari payload baru, jadi setelah
domain project berubah di API, project harus dideploy ulang (termasuk yang sedang tidur)
supaya route-nya memakai domain baru.

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
repository. Bila direktori itu package workspace JavaScript, `railpack info` berjalan di
root workspace dan result membawa `workspace`; lihat
[Project inspection](#project-inspection-command).

`sakala-api` menentukan `resources`, `timeouts`, dan `log_bounds` berdasarkan policy project/workspace/plan. `cpu_millis=500` berarti `0.5` vCPU. Agent menjalankan build, start/health, dan seluruh command menggunakan deadline payload, tetapi menolak timeout yang nol atau melebihi hard maximum node. Agent juga merahasiakan secret, membatasi panjang baris dan total byte log sebelum report; API tetap memvalidasi ulang log sebagai trust boundary terakhir. Network Docker tidak boleh berasal dari payload karena merupakan konfigurasi lokal runtime node.

### Replika

Payload `DeployProject` boleh membawa `replicas`, yaitu jumlah container yang melayani
deployment itu di node yang sama. Router Caddy node membagi request ke semua replika.
Contohnya ada di `examples/commands/deploy-project-replicas.json`.

| Nilai | Perilaku Agent |
| --- | --- |
| Tidak ada atau `null` | Satu replika, sama seperti sebelum field ini ada. |
| Bilangan bulat 1 sampai batas node | Sebanyak itu replika. |
| Di atas batas node | Deploy gagal dengan `runtime_replicas_exceeded` sebelum checkout. Agent tidak diam-diam menjalankan lebih sedikit. |
| Nilai lain: `0`, angka negatif, pecahan, string seperti `"3"`, atau tipe lain | Satu replika, dan Agent mencatat peringatan di log operator. Deploy tidak pernah ditolak karena nilai yang tidak terbaca. |

Batas node berasal dari `SAKALA_MAX_REPLICAS_PER_PROJECT` (default 4) dan dikirim di
setiap heartbeat sebagai `metadata.workloads.maximum_replicas_per_project`. API hanya
boleh mengirim `replicas` di atas 1 ke node yang mengiklankan capability `replicas`;
Agent lama mengabaikan field ini dan menjalankan satu container.

Setiap replika adalah container sendiri dengan image, environment, resource, dan label
yang sama, plus label `dev.sakala.replica` (indeks, mulai 0) dan `dev.sakala.replicas`
(jumlah). Deployment satu replika memakai nama container yang sama seperti Agent
sebelumnya, `sakala-app-<project8>-<deployment-id>`; deployment beberapa replika
menambahkan `-r<indeks>`. `resources` berlaku per replika, dan setiap replika dihitung
terhadap `SAKALA_MAX_ACTIVE_CONTAINERS`.

Deploy berjalan bergulir: Agent menyalakan semua replika baru, menunggu setiap replika
menjawab HTTP dalam satu batas `start_timeout_seconds`, lalu mengganti route ke set baru
dalam satu rename atomik, baru kemudian menghentikan set lama. Bila satu replika saja
gagal ready, deploy gagal, semua replika baru dihapus, output replika yang gagal
dikirim sebagai log, dan route lama tetap melayani. Aplikasi dengan replika harus
stateless: file atau sesi yang disimpan di disk satu container tidak terlihat oleh
replika lain.

Replika yang berhenti sementara replika lain dari deployment yang sama masih
melayani, misalnya karena `docker kill`, dinyalakan lagi oleh Agent sendiri dengan
`docker start` pada interval health worker berikutnya, dengan backoff per replika dan
batas 5 kali percobaan. Project yang tidur, deployment satu replika, dan project yang
sedang menjalankan command tidak disentuh. Aturan lengkapnya ada di
[Runtime Hardening](RUNTIME_HARDENING.md#pemulihan-replika).

### Release command

Payload `DeployProject` boleh membawa `release_command`, yaitu perintah yang dijalankan
Agent satu kali per deployment sebelum versi baru dinyalakan, misalnya
`php artisan migrate --force`. Contohnya ada di
`examples/commands/deploy-project-release-command.json`. Polanya sama dengan release phase
Heroku dan pre-deploy command Railway.

| Nilai | Perilaku Agent |
| --- | --- |
| Tidak ada, `null`, string kosong, atau hanya spasi | Tidak ada release command. Deploy berjalan seperti sebelumnya. |
| String 1 sampai 1000 karakter tanpa control character | Dijalankan satu kali, setelah build dan sebelum replika pertama. |
| Lebih dari 1000 karakter, atau memuat control character ASCII (`\x00` sampai `\x1F` dan `\x7F`, termasuk newline dan tab) | Ditolak dengan `invalid_runtime_command` sebelum checkout. Aturannya sama dengan validasi `start_command` di API. |
| Bukan string, misalnya array | Payload ditolak dengan `invalid_runtime_command`. |

Panjang dihitung dalam karakter, bukan byte. API hanya boleh mengirim field ini ke node
yang mengiklankan capability `release-command`. Agent lama mengabaikannya dan langsung
menyalakan versi baru tanpa menjalankan perintah itu, jadi migrasi tidak akan berjalan.

Urutannya di dalam satu `DeployProject`:

1. Build image, tidak berubah.
2. Agent menjalankan satu container release dari image baru dan menunggu container itu
   selesai, paling lama `SAKALA_RELEASE_COMMAND_TIMEOUT_SECONDS` (default 600 detik).
3. Container release selalu dihapus, apa pun hasilnya.
4. Exit code 0: deploy berlanjut seperti biasa, yaitu menyalakan replika baru, menunggu
   readiness, lalu memindahkan route. Selain itu deploy gagal, dan tidak ada bagian versi
   baru yang dinyalakan. Deployment sebelumnya tetap melayani.

Container release mendapat environment (termasuk secret dan `PORT`), network project,
batas resource, runtime container (`runsc` bila dikonfigurasi), dan opsi keamanan yang
sama dengan replika. Bedanya, container ini tidak punya route, tidak punya restart
policy, dan tidak dihitung sebagai workload. Detailnya ada di
[Release command](DOCKER_RUNTIME.md#release-command).

Output perintah dikirim sebagai log deployment dengan awalan `[release]`, setara `[build]`
dan `[runtime]`, melewati redaksi yang sama. Agent juga mengirim event
`deployment.release.started` (metadata `container` dan `timeout_seconds`) sebelum
container release dijalankan, dan `deployment.release.completed` (metadata `duration_ms`)
setelah exit code 0. Kedua event ini hanya muncul bila ada release command.

Release command hanya dijalankan oleh `DeployProject`. `WakeProject`, `RestartProject`,
`RefreshRoute`, `ReconcileWorkload`, dan pemulihan replika tidak pernah menjalankannya,
karena semuanya memakai container deployment yang sudah ada.

### Memakai ulang image (tanpa build)

Payload `DeployProject` boleh membawa `reuse_image_from_deployment_id`, yaitu deployment
yang membangun image yang ingin dipakai ulang. Gunanya untuk redeploy yang hanya
mengubah konfigurasi, misalnya variabel, rotasi password database, atau attach database:
commit-nya sama dengan deployment aktif, jadi image-nya pun sama, dan build 3 sampai 6
menit tidak perlu diulang. Contohnya ada di
`examples/commands/deploy-project-image-reuse.json`.

| Nilai | Perilaku Agent |
| --- | --- |
| Tidak ada atau `null` | Build seperti biasa, sama seperti sebelum field ini ada. |
| String UUID | Agent mencari image deployment itu di node. Bila ketemu dan terbukti milik project ini, deployment memakai image itu tanpa checkout dan tanpa build. Bila tidak, Agent build seperti biasa dan melaporkan alasannya. |
| Nilai lain: string yang bukan UUID, angka, array, atau tipe lain | Build seperti biasa dengan alasan `invalid_reference`. Payload tidak pernah ditolak karena field ini. |

UUID itu harus deployment yang benar-benar membangun image-nya, bukan deployment yang
sendirinya memakai ulang image. API yang menelusuri rantai itu. API hanya boleh mengirim
field ini ke node yang mengiklankan capability `image-reuse`; Agent lama mengabaikannya
dan build.

Agent tidak pernah menerima referensi image dari payload. Referensinya dihitung sendiri
dari project yang meminta, yaitu `sakala/project-<project_id>:<commit12>-<source8>`
(`project_id` dari command, commit dari `commit_sha` payload, dan 8 karakter pertama UUID
deployment sumber). Siapa pun yang memegang Docker socket bisa men-tag image apa saja di
bawah nama itu, jadi Agent hanya memakainya bila label image-nya sendiri cocok:

- `dev.sakala.project-id` sama dengan `project_id` command;
- `dev.sakala.deployment-id` sama dengan `reuse_image_from_deployment_id` (UUID lengkap);
- `dev.sakala.builder` bernilai `dockerfile` atau `railpack`.

Label `dev.sakala.builder` dan `dev.sakala.package-directory` mengembalikan hasil build
aslinya, jadi `start_command` dan `release_command` berperilaku persis seperti pada
deployment yang membangunnya. Image Dockerfile menerima `start_command` saat container
dijalankan. Image Railpack sudah membawa start command-nya sejak build, jadi
`start_command` tidak diteruskan. Release command image dari root workspace JavaScript
dijalankan dengan `cd '<package>' && ...`.

Pada deployment yang memakai ulang image, Agent mengirim `deployment.resources.resolved`
dan `deployment.timeouts.resolved` seperti biasa, lalu `deployment.image.reused`
(level `info`) sebagai ganti `deployment.checkout.started`, `deployment.analysis.started`,
`deployment.workspace.detected`, dan `deployment.build.started`:

```json
{
  "type": "deployment.image.reused",
  "level": "info",
  "metadata": {
    "image": "sakala/project-ff66ed4a-6303-4be6-8ef4-63c28b112680:0123456789ab-4f1f21ef",
    "source_deployment_id": "4f1f21ef-730d-42d5-a46d-d965353cb993",
    "builder": "dockerfile"
  }
}
```

Setelah itu urutannya sama seperti deployment yang build: release command bila ada,
`deployment.container.started`, `deployment.health.started`, `deployment.route.started`,
dan `deployment.runtime.ready` dengan `image` berisi referensi yang dipakai ulang dan
`builder` dari label. Checkout, pemeriksaan ruang disk workspace, dan antrean build
(`SAKALA_MAX_CONCURRENT_BUILDS`) dilewati. Pemeriksaan kapasitas container, readiness,
replika, dan perpindahan route tanpa downtime tidak berubah.

Bila image tidak bisa dipakai ulang, Agent mengirim `deployment.image.reuse_skipped`
(level `warning`) sebelum `deployment.checkout.started`, lalu build seperti biasa.
Ini bukan kegagalan, dan tidak ada kode error atau result completion baru.

```json
{
  "type": "deployment.image.reuse_skipped",
  "level": "warning",
  "metadata": {
    "source_deployment_id": "4f1f21ef-730d-42d5-a46d-d965353cb993",
    "reason": "image_missing"
  }
}
```

| `reason` | Arti |
| --- | --- |
| `image_missing` | Node tidak punya image di bawah referensi itu: sudah dibersihkan image GC, deployment sumbernya di node lain, atau tidak pernah ada. Image yang gagal di-inspect Docker juga dihitung begini. |
| `image_not_owned` | Label image menyebut project atau deployment lain, atau label kepemilikannya tidak ada. |
| `image_metadata_missing` | Label `dev.sakala.builder` tidak ada atau nilainya tidak dikenal. Image ini dibangun Agent sebelum fitur ini ada, jadi redeploy konfigurasi pertama setelah upgrade build satu kali. |
| `invalid_reference` | `reuse_image_from_deployment_id` bukan string UUID. `source_deployment_id` bernilai `null`. |

Release command tetap dijalankan pada deployment yang memakai ulang image. Database
yang baru di-attach butuh migrasinya dijalankan ke database itu, dan perubahan
environment bisa mengubah apa yang dilakukan migrasi.

Deployment yang memakai ulang image dan gagal sebelum route pindah tidak pernah
menghapus image itu, karena image tersebut masih dipakai deployment yang sedang
melayani. Container kandidatnya tetap dihapus seperti biasa. Detail runtime ada di
[Memakai ulang image](DOCKER_RUNTIME.md#memakai-ulang-image).

### Rollback dan image sebelumnya

Rollback tidak punya command atau field sendiri. API mengirim `DeployProject` biasa
dengan `commit_sha` milik deployment tujuan dan `reuse_image_from_deployment_id` berisi
deployment yang membangun image tujuan itu. Agent memperlakukannya persis seperti
redeploy konfigurasi di atas: image dicari di bawah nama yang dihitung sendiri, label
diperiksa, dan bila image sudah tidak ada Agent build commit itu lagi dengan
`deployment.image.reuse_skipped` (`image_missing`).

Node yang mengiklankan capability `image-rollback` (hanya driver `docker`) menjanjikan
image mana yang masih ada, sehingga API tahu kapan rollback bisa dijanjikan "tanpa
build":

- Setelah `DeployProject` sukses, yaitu workload baru melayani dan workload lama sudah
  dihapus, Agent menyimpan tepat dua image project itu: image yang sedang melayani
  (*live*) dan image sebelumnya (*previous*). Image `sakala/project-<project_id>:*` lain
  yang tidak dipakai container mana pun dihapus, masih di bawah lock project yang sama
  dengan deployment itu.
- *Previous* adalah image workload yang digantikan deployment sukses terakhir, bila
  berbeda dari image live yang baru. Redeploy yang memakai ulang image live (redeploy
  konfigurasi) tidak menggeser previous. Setelah rollback dari I5 ke I3, previous
  menjadi I5, jadi kembali maju ke I5 juga tanpa build.
- Penandanya tag Docker kedua di repository yang sama:
  `sakala/project-<project_id>:previous`. Image itu tetap punya tag aslinya, jadi
  pencarian reuse lewat nama yang dihitung tetap menemukannya. Memindahkan tag tidak
  pernah menghapus image dengan sendirinya.
- Deployment yang gagal tidak pernah menggeser previous dan tetap hanya menghapus image
  kandidat yang dibangunnya sendiri.
- Hanya image yang semua namanya `sakala/project-<project_id>:<tag>` dan berlabel
  `dev.sakala.project-id` project itu yang pernah diberi tag. Image GC juga menghapus
  image tanpa label project sama sekali yang semua namanya ada di repository satu
  project, yaitu image Railpack dari Agent sebelum label Railpack diperbaiki; lihat
  [Approved Runtime Cleanup](#approved-runtime-cleanup).

Jadi rollback tanpa build hanya bisa ke deployment yang membangun image live atau image
previous di node yang sama. Rollback ke deployment lain tetap jalan, hanya lewat build.
Bila langkah tag atau penghapusan ini gagal, deployment tetap sukses: Agent mengirim
event `deployment.images.retained` (level `warning`, metadata `project_id`), dan image GC
berkala merapikannya kemudian. Agent yang tidak mengiklankan `image-rollback` tetap
menerima rollback lewat `reuse_image_from_deployment_id`, tetapi image-nya diatur aturan
GC lama, jadi API sebaiknya tidak menjanjikan "tanpa build".

### Proses worker

Payload `DeployProject` boleh membawa `processes`, yaitu daftar proses worker yang berjalan
di samping proses web (ADR-022), misalnya `php artisan queue:work`. Contohnya ada di
`examples/commands/deploy-project-processes.json`. API hanya boleh mengirim field ini ke
node yang mengiklankan capability `worker-processes`. Agent lama mengabaikannya dan hanya
menjalankan proses web.

```json
"processes": [
  {
    "name": "queue",
    "command": "php artisan queue:work --sleep=3 --tries=3 --max-time=3600",
    "replicas": 1,
    "resources": { "memory_mb": 192, "cpu_millis": 250, "pids_limit": null }
  }
]
```

| Field | Tipe | Wajib | Arti |
| --- | --- | --- | --- |
| `processes` | array atau `null` | tidak | Definisi worker, paling banyak 5. Tidak ada atau `null` berarti hanya proses web, persis seperti sebelum field ini ada. Array kosong juga berarti tanpa worker. |
| `processes[].name` | string | ya | Huruf kecil, angka, dan `-`, diawali huruf, paling panjang 30 karakter, unik di dalam daftar. `web`, `release`, dan `job` dicadangkan. |
| `processes[].command` | string | ya | 1 sampai 1000 karakter tanpa control character ASCII, aturan yang sama dengan `release_command`. Spasi di awal dan akhir dibuang; string kosong ditolak. Dijalankan sebagai `sh -c <command>` di dalam container. |
| `processes[].replicas` | integer 0 sampai `metadata.workloads.maximum_replicas_per_project` | tidak, default 1 | Jumlah container proses ini. `0` menyimpan definisi tanpa menjalankan apa pun. |
| `processes[].resources` | object `{ memory_mb, cpu_millis, pids_limit }`, setiap field integer atau `null` | tidak | Batas per container. Field yang tidak ada atau `null` memakai nilai `resources` deployment, lalu default node bila itu juga kosong. Nilai di atas batas node gagal seperti `resources` deployment. |

Membaca field ini dibuat longgar seperti `replicas`: nilai yang bukan array definisi
(misalnya object, string, atau item tanpa `name`/`command`, `replicas` negatif atau
berupa string) tidak menggagalkan deploy. Proses web tetap dideploy tanpa worker, Agent
mengirim event `deployment.processes.ignored` (level `warning`, metadata `{}`), dan
mencatat peringatan di log operator. Daftar yang terbaca tetapi melanggar aturan di atas
(nama dicadangkan atau dobel, lebih dari 5 definisi, perintah kosong atau multi-baris)
gagal sebelum checkout dengan `invalid_runtime_command`. `replicas` di atas batas node
gagal dengan `runtime_replicas_exceeded`, sama seperti replika web.

Setiap worker memakai image, variabel (termasuk `PORT`), network project, runtime
container, opsi keamanan, dan filter egress yang sama dengan replika web, plus
`--restart unless-stopped`. Bedanya, worker tidak punya route, tidak diprobe HTTP, tidak
masuk traffic, dan tidak dipulihkan oleh pemulihan replika. Container-nya bernama
`sakala-worker-<project8>-<deployment-id>-<name>-r<indeks>` dan membawa label
`dev.sakala.workload-kind=worker` serta `dev.sakala.process=<name>` (replika web tetap
`dev.sakala.workload-kind=web`). Label `dev.sakala.replica` dan `dev.sakala.replicas`
menghitung container proses itu sendiri. Untuk image Railpack dari workspace JavaScript,
perintah dijalankan di direktori package, sama seperti release command.

Urutan di dalam satu `DeployProject`: build atau reuse, release command satu kali, lalu
replika web dan worker dinyalakan di bawah satu pemeriksaan kapasitas. Setiap container
worker dihitung terhadap `SAKALA_MAX_ACTIVE_CONTAINERS` seperti replika. Replika web
harus lolos cek HTTP seperti biasa; worker dianggap siap bila masih berjalan, tanpa
restart, `SAKALA_WORKER_STARTUP_SECONDS` (default 10) setelah dinyalakan. Keduanya
ditunggu bersamaan. Worker yang keluar sebelum titik itu, apa pun exit code-nya termasuk
0, menggagalkan deploy dengan `worker_start_failed`: semua container baru dihapus,
output worker yang gagal dikirim sebagai log dengan `process` worker itu, route tidak
berpindah, dan versi lama tetap melayani bersama worker-nya. Setelah route berpindah,
container lama dihentikan: replika web dengan grace 10 detik, worker dengan
`SAKALA_WORKER_STOP_GRACE_SECONDS` (default 30), keduanya bersamaan. Selama beberapa
detik dua versi worker bisa sama-sama mengambil job antrean, jadi job harus kompatibel
dengan versi sebelumnya, seperti migrasi release command.

Event deployment yang menyebut container menambah `processes` bila deployment punya
worker: `deployment.container.started` dan `deployment.runtime.ready` mendapat
`processes: [{ "process": "queue", "container": "<nama>" }]` (satu item per container), dan
`deployment.health.started` mendapat `worker_startup_seconds`. Deployment tanpa worker
mengirim event yang sama persis seperti sebelumnya.

Setelah deploy, worker diperlakukan sebagai bagian dari deployment:

| Command | Perilaku untuk worker |
| --- | --- |
| `RestartProject` | Semua replika web dan worker di-restart; route dibangun ulang dari replika web saja. Pemeriksaan "ada yang berjalan" dan halaman tidur memakai replika web. |
| `SleepProject` | Web dan worker dihentikan bersama, masing-masing dengan grace-nya. Container dan image disimpan. |
| `WakeProject` | Web dan worker dinyalakan setelah pemeriksaan kapasitas untuk semuanya. Route ditulis untuk replika web. |
| `StopProject` | Web dan worker dihentikan lalu dihapus. Job project itu yang sedang berjalan di node ini juga dibatalkan (`runtime_cancelled`). |
| `HealthCheck`, `RefreshRoute`, `ReconcileWorkload.restore_route` | Hanya replika web. `ReconcileWorkload.actual_state` juga dihitung dari replika web. |
| `ReconcileWorkload.restart_log_follower` | Follower untuk semua container yang berjalan; baris worker ditandai `process`. |
| Rollback | `DeployProject` biasa dengan `processes` rekaman deployment tujuan. Worker yang ditambahkan setelah deployment tujuan tidak ikut berjalan. |

Worker yang keluar dengan code 0 setelah deploy (misalnya `--max-time` Laravel) dinyalakan
lagi oleh Docker dan tidak dilaporkan sebagai crash. Exit lain mengikuti aturan crash loop
yang sama dengan web, tetapi dilaporkan di `crashed_details` dengan `process`, terpisah dari
proses web; lihat [Heartbeat Payload](#heartbeat-payload).

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

`RestartProject` menyalakan ulang semua replika deployment itu dari container yang ada,
tanpa build dan tanpa deployment baru. Bila minimal satu replika berjalan, semua replika
di-restart seperti sebelumnya. Bila tidak ada yang berjalan, karena container-nya exited
atau Docker sedang di antara dua restart crash loop (`Restarting`), Agent memeriksa
kapasitas node seperti wake lalu me-restart semuanya. Deployment yang sedang tidur, yaitu
yang route project-nya halaman tidur untuk deployment itu, tetap ditolak dengan
`runtime_workload_not_running`; project seperti itu dibangunkan lewat `WakeProject`.
Route tidur adalah satu-satunya catatan lokal sebuah sleep, sama dengan yang dipakai
deteksi orphan. Bila halaman tidur gagal dipasang saat sleep dan Agent menghapus route
sebagai gantinya, Agent tidak bisa membedakan project itu dari yang crash, jadi API hanya
boleh mengirim `RestartProject` untuk project yang `runtime_status`-nya `running` atau
`crashed`. Agent v0.15.0 dan sebelumnya juga menolak project yang crash dengan
`runtime_workload_not_running`.

Ada dua pengecualian. Yang pertama `StopProject` untuk project yang dihapus. Payload-nya
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
tersebut baru hilang lewat image GC.

Yang kedua `SleepProject`. Payload-nya boleh membawa `page`, yaitu halaman yang
ditampilkan domain project selama tidur. Field ini string, opsional, dan bukan
`null` bila dikirim:

| Nilai | Arti |
| --- | --- |
| `waking` | Default bila `page` tidak ada. Halaman "Proyek sedang dinyalakan" meminta API membangunkan project lewat endpoint wake publik pada pemuatan pertama sebuah kunjungan, lalu memuat ulang sendiri setiap 3 detik selama paling lama 2 menit. Setelah itu halaman berhenti memuat ulang dan menawarkan tombol "Coba lagi". Dipakai untuk sleep karena sepi. |
| `unavailable` | Halaman statis "Proyek tidak tersedia" tanpa script, tanpa permintaan wake, dan tanpa muat ulang. Dipakai untuk project yang tidak boleh bangun saat dikunjungi, misalnya yang di-suspend. Halaman ini tidak menyebut alasannya. |

```json
{
  "type": "SleepProject",
  "project_id": "ff66ed4a-6303-4be6-8ef4-63c28b112680",
  "deployment_id": "4f1f21ef-730d-42d5-a46d-d965353cb993",
  "payload": { "page": "unavailable" }
}
```

Contoh lengkapnya ada di `examples/commands/sleep-project-unavailable.json`. Payload
`{}` atau `null` menampilkan halaman `waking`, sama seperti sebelum field ini ada.
Agent tidak pernah menolak `SleepProject` karena payload-nya: nilai `page` yang tidak
dikenal, `page: null`, atau payload yang bukan object dibaca sebagai `waking` dan
dicatat sebagai peringatan di log operator, karena menolak sleep berarti project yang
sepi tetap berjalan. Keduanya menjawab HTTP 503 dengan `Cache-Control: no-store` dan
`X-Robots-Tag: noindex`; hanya `waking` yang mengirim `Retry-After: 5`. Keduanya
memakai penanda kepemilikan route `(sleeping)` yang sama, jadi `WakeProject`,
`StopProject`, redeploy, dan cleanup route basi memperlakukannya sama. `WakeProject`
tetap menulis route biasa di atas halaman `unavailable`; API yang memutuskan apakah
sebuah wake boleh dikirim. API hanya boleh mengirim `page` ke node yang mengiklankan
capability `idle-sleep`. Agent v0.7.0 mengiklankan capability itu tetapi belum
membaca `page`, sehingga tetap menampilkan halaman `waking`.

Semantik lengkap `Stop` versus `Sleep`, hasil health, dan route refresh berada di
[Command Lifecycle](COMMAND_LIFECYCLE.md).

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

## Job (`RunJob`)

`RunJob` menjalankan satu perintah project satu kali di container dari image deployment
yang sedang live (ADR-022). API adalah satu-satunya penjadwal: setiap slot jadwal dan
setiap "Run now" menjadi satu command `RunJob` yang dipin ke node yang melayani project.
Agent lama gagal membaca respons poll yang berisi tipe command yang tidak dikenal, jadi
API hanya boleh memberikan `RunJob` ke node yang mengiklankan capability `scheduled-jobs`.
Contoh lengkap ada di `examples/commands/run-job.json`.

```json
{
  "id": "8a6c1e57-2f4b-4b3d-9e0a-5c7d1f2e3a4b",
  "type": "RunJob",
  "status": "Pending",
  "project_id": "ff66ed4a-6303-4be6-8ef4-63c28b112680",
  "deployment_id": "4f1f21ef-730d-42d5-a46d-d965353cb993",
  "payload": {
    "command": "php artisan schedule:run",
    "timeout_seconds": 300,
    "commit_sha": "0123456789abcdef0123456789abcdef01234567",
    "reuse_image_from_deployment_id": "0b5c3f9e-1d2a-4e8b-9c7d-6a5f4e3d2c1b",
    "container_port": 8000,
    "environment": { "APP_ENV": "production" },
    "resources": { "memory_mb": 256, "cpu_millis": 500, "pids_limit": 128 },
    "log_bounds": { "max_line_length": 4096, "max_batch_lines": 500, "max_total_bytes": 10485760 }
  }
}
```

`project_id` dan `deployment_id` wajib; `deployment_id` adalah deployment live project.
Semua field selain `command`, `timeout_seconds`, dan `commit_sha` disalin API dari payload
`DeployProject` deployment live itu, sehingga job memakai persis apa yang dipakai versi live.

| Field | Tipe | Wajib | Arti |
| --- | --- | --- | --- |
| `command` | string | ya | 1 sampai 1000 karakter tanpa control character ASCII; spasi di awal dan akhir dibuang, string kosong ditolak. Dijalankan sebagai `sh -c <command>`. |
| `timeout_seconds` | integer 1 sampai 600 atau `null` | tidak, default 300 | Batas waktu job, dalam detik, dihitung sejak container dinyalakan. |
| `commit_sha` | string, 40 karakter heksadesimal | ya | Commit deployment live. |
| `reuse_image_from_deployment_id` | UUID string atau `null` | tidak | Deployment yang membangun image live, bila deployment live memakai ulang image (redeploy konfigurasi atau rollback). Tidak ada berarti `deployment_id` sendiri yang membangunnya. Nilai yang bukan UUID ditolak dengan `invalid_runtime_command`. |
| `container_port` | integer 1 sampai 65535 | tidak, default 3000 | Diekspor sebagai `PORT`, seperti untuk web. Tidak ada yang listen di sana. |
| `environment` | object string ke string | tidak | Variabel deployment live, aturan dan redaksi sama dengan `DeployProject`. `PORT` yang berbeda dari `container_port` gagal dengan `runtime_port_conflict`. |
| `resources` | object `{ memory_mb, cpu_millis, pids_limit }` | tidak | Batas container job; kosong memakai default node, di atas batas node ditolak. |
| `log_bounds` | object | tidak | Batas log command ini, seperti pada `DeployProject`. |

Agent mencari image di bawah nama yang dihitung sendiri dari project job,
`sakala/project-<project_id>:<commit12>-<deployment8>` dengan deployment pembangun image,
lalu memeriksa labelnya seperti reuse image. Jadi job hanya pernah bisa menjalankan image
project-nya sendiri.

Cara Agent menjalankan job:

- Container `sakala-job-<project8>-<command-id>` dari image itu, dengan network project,
  runtime container (`runsc` bila dikonfigurasi), opsi keamanan, filter egress, dan batas
  resource yang sama dengan web, `--pull never`, tanpa restart policy, tanpa route, dan
  tanpa label `dev.sakala.managed`. Label: `dev.sakala.role=job`, `dev.sakala.project-id`,
  `dev.sakala.deployment-id`, `dev.sakala.command-id`, `dev.sakala.agent-id`.
- Job berjalan di pool sendiri, `SAKALA_MAX_CONCURRENT_JOBS` (default 2), di luar
  `SAKALA_MAX_CONCURRENT_COMMANDS` dan di luar antrean per project: job tidak menunggu
  deploy, restart, atau sleep project yang sama, dan tidak menahannya. Job tidak pernah
  menyalakan, menghentikan, atau me-route container web atau worker, jadi project yang
  tidur tetap tidur dan job tidak dihitung sebagai traffic.
- Job diterima di bawah pemeriksaan kapasitas node yang sama dengan deploy dan wake,
  sebagai satu container. Semua container yang berjalan dihitung, termasuk milik project
  job itu. Selama berjalan, job juga dihitung oleh deploy project lain dan oleh deploy
  project-nya sendiri, karena deploy tidak menggantikan job.
- Satu command tidak pernah menjalankan dua container: nama container diturunkan dari ID
  command, dan scheduler Agent tidak memulai ID command yang sedang berjalan. `idempotency_key`
  per slot tetap tanggung jawab API pada record command.
- Container selalu dihapus setelah job selesai, gagal, melewati batas waktu, atau
  dibatalkan. Saat Agent start, container job yang tertinggal milik Agent ini dihapus
  bersama container release; command-nya sendiri berakhir lewat lease API.
- Output container dikirim lewat endpoint logs command ini dengan awalan `[job]` dan
  `process: "job"`, melewati redaksi yang sama.

Event job:

| Event | Kapan | Metadata |
| --- | --- | --- |
| `job.started` | Container job sudah dinyalakan. | `container` (string), `image` (string), `timeout_seconds` (integer). |
| `job.finished` | Container berakhir dengan cara apa pun selain pembatalan, sebelum `complete`/`fail`. | `exit_code` (integer, `null` setelah timeout), `duration_ms` (integer), `oom_killed` (boolean), `timed_out` (boolean). |

Hasil:

| Hasil | Report | Isi |
| --- | --- | --- |
| Exit code 0 | `complete` | `result`: `{ "exit_code": 0, "duration_ms": 1570, "oom_killed": false }`. `duration_ms` dihitung sejak container dinyalakan sampai keluar. |
| Exit code lain, atau `sh` tidak bisa dijalankan (126/127) | `fail` `job_failed` | Pesan menyebut exit code, dan batas memori bila Docker melaporkan OOM. Exit code terstruktur ada di `job.finished`. |
| Melewati `timeout_seconds` | `fail` `job_timeout` | Container dimatikan dan dihapus. |
| Image tidak ada di node, atau label image tidak menyebut project dan deployment itu | `fail` `job_image_missing` | Tidak ada container yang dibuat. Pesan menyarankan deploy ulang. |
| Node penuh | `fail` `runtime_capacity_exceeded` | Tidak ada container yang dibuat. |
| `cancel_requested`, `StopProject` project itu, atau Agent dimatikan | `fail` `runtime_cancelled` | Container dihapus. |
| Payload tidak valid | `fail` `invalid_runtime_command` atau `runtime_port_conflict` | Sebelum container apa pun. |

Batas waktu command `RunJob` di Agent adalah `timeout_seconds` (paling lama 600) ditambah
30 detik untuk menghentikan, menghapus, dan melaporkan container, bukan
`SAKALA_COMMAND_TIMEOUT_SECONDS`. API perlu memberi lease setidaknya sepanjang itu
ditambah grace lease-nya. Contoh body `fail` ada di `examples/failures/job-failures.json`.

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
Satu pengecualian: image yang tidak punya label `dev.sakala.project-id` sama sekali
dianggap milik project yang namanya dipakai, bila semua namanya
`sakala/project-<project_id>:<tag>` untuk satu project yang sama. Image Railpack yang
dibangun Agent sampai v0.16.0 tidak membawa label apa pun, karena frontend Railpack
mengabaikan `--label`, sehingga tanpa pengecualian ini image itu, termasuk milik project
yang sudah dihapus, tidak pernah dibersihkan. Image seperti itu mengikuti aturan simpan
yang sama di bawah, tetapi tidak pernah diberi tag `previous`, dan tidak pernah dipakai
ulang karena reuse tetap mensyaratkan label. Nama-nama image dibaca dari
`docker image ls` tanpa filter, karena filter `reference` menyembunyikan nama lain
sebuah image. Image dangling, image berlabel project yang tidak valid, dan image yang
punya nama di luar repository project itu tidak pernah dipilih.

Per project yang masih punya container (berlabel project itu, atau memakai salah satu
image-nya), Agent mempertahankan setiap image yang dipakai container mana pun, dalam
state apa pun (jadi container yang tidur dan semua replika ikut dihitung), ditambah image
previous: image bertag `sakala/project-<project_id>:previous`, atau, bila belum ada image
dengan tag itu (image dari Agent lama), image terbaru yang lebih tua dari image aktif
terbaru, atau image terbaru bila tidak ada yang aktif. Umur image tidak lagi melindungi
image project seperti ini; `SAKALA_IMAGE_GC_MAX_AGE_SECONDS` sudah tidak dipakai. Project
tanpa container sama sekali, misalnya project yang sudah dihapus, hanya mempertahankan
image yang lebih muda dari `SAKALA_ORPHANED_IMAGE_GRACE_SECONDS` (default 24 jam,
dihitung dari waktu image dibuat); image terbaru dan image previous-nya ikut dihapus
karena tidak ada yang akan di-rollback.

Sisanya dihapus dengan `docker image rm <nama>...` tanpa `--force`, per nama, dengan tag
`previous` lebih dulu. Docker tetap menolak image yang mulai dipakai container setelah
pemilihan, dan image yang ditolak tetap memegang nama aslinya. Image sebuah project hanya
dihapus selama tidak ada command project itu yang berjalan: GC memegang lock project yang
sama dengan command, melewati project yang sedang dipegang, dan memilih ulang image
project itu setelah lock didapat. Selain lewat command ini, Agent menjalankan aturan yang
sama sendiri setiap `SAKALA_IMAGE_GC_INTERVAL_SECONDS`, kecuali saat ada command aktif,
dan setelah setiap `DeployProject` yang sukses (lihat
[Rollback dan image sebelumnya](#rollback-dan-image-sebelumnya)).
`startup_reconciliation.stale_images` pada heartbeat memuat daftar yang sama, tanpa
menghapus apa pun.

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
    "workspace": null,
    "env_example": {
      "file": ".env.example",
      "entries": [
        { "key": "APP_NAME", "example_value": "Buku tamu Sakala" },
        { "key": "APP_KEY", "example_value": null }
      ],
      "truncated": false
    },
    "database_hint": {
      "likely": true,
      "engine": "postgresql",
      "signals": [
        { "kind": "env_variable", "file": ".env.example", "name": "DATABASE_URL" },
        { "kind": "prisma_schema", "file": "prisma/schema.prisma", "name": "postgresql" },
        { "kind": "package_dependency", "file": "package.json", "name": "@prisma/client" }
      ],
      "suggested_release_command": "npx prisma migrate deploy"
    },
    "frameworks": ["next"],
    "railpack": {}
  }
}
```

`workspace` selalu dikirim. Nilainya `null`, kecuali `root_directory` adalah package
workspace pnpm, npm, Yarn, atau Bun yang root-nya ada di atasnya. Field yang tidak ada
berarti Agent sebelum field ini. Contoh untuk `root_directory: "apps/web"`:

```json
{
  "package_manager": "pnpm",
  "workspace": {
    "manager": "pnpm",
    "root_directory": ".",
    "package_directory": "apps/web",
    "package_name": "@facility-reporting/web",
    "build_command": "pnpm --filter '@facility-reporting/web...' run build",
    "start_command": null,
    "static_output_directory": "apps/web/dist"
  }
}
```

| Field | Tipe | Arti |
| --- | --- | --- |
| `manager` | `"npm"` \| `"pnpm"` \| `"yarn"` \| `"bun"` | Package manager yang dipasang Railpack, dipilih dengan urutan Railpack. Pada inspection seperti ini `package_manager` juga bernilai sama. |
| `root_directory` | string | Root workspace relatif terhadap root repository, `.` untuk root repository. Inilah konteks build. |
| `package_directory` | string | Root directory project, relatif terhadap root workspace. |
| `package_name` | string \| `null` | `name` di `package.json` package, `null` bila tidak ada. |
| `build_command` | string \| `null` | Perintah build yang berjalan di root workspace. Pada inspection, ini perintah turunan dari script `build` package, karena payload inspection tidak membawa `build_command` project. `null` bila package tidak punya script `build`. |
| `start_command` | string \| `null` | Perintah start image. `null` bila image menyajikan `static_output_directory`, atau bila tidak ada script `start` maupun perintah. |
| `static_output_directory` | string \| `null` | Hasil build SPA Vite, relatif terhadap root workspace, yang disajikan Railpack dengan Caddy-nya sendiri. |

Aturan deteksi dan perintahnya ada di [Docker Runtime](DOCKER_RUNTIME.md#monorepo-javascript).
Console bisa memakai field ini untuk menjelaskan bahwa build berjalan dari root workspace
dan perintah apa yang akan dipakai.

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

### Variabel dari `.env.example`

`env_example` selalu dikirim. Isinya daftar variabel yang dicantumkan file contoh
environment di direktori yang diinspeksi, supaya Console bisa menyarankannya sebelum
deployment pertama. Field yang tidak ada berarti Agent sebelum field ini.

| Field | Tipe | Arti |
| --- | --- | --- |
| `env_example` | object \| `null` | `null` bila tidak ada file kandidat, atau file itu tidak bisa dibaca dengan aman (lihat di bawah). |
| `env_example.file` | `".env.example"` \| `".env.sample"` \| `".env.dist"` | File yang dibaca. Kandidat dicari dengan urutan ini dan yang pertama ada yang dipakai. |
| `env_example.entries` | array | Paling banyak 200 entri, urut seperti di file. |
| `env_example.entries[].key` | string | Nama variabel, cocok dengan `^[A-Za-z_][A-Za-z0-9_]{0,127}$`. Nama lain dibuang dan tidak dihitung. |
| `env_example.entries[].example_value` | string \| `null` | Nilai contoh tanpa tanda kutip, paling banyak 200 karakter Unicode (dipotong di batas karakter). `null` bila kosong, bila nilainya berlanjut ke baris berikutnya atau kutipnya tidak ditutup, atau bila nilainya mirip credential: memuat `-----BEGIN`, atau URL yang userinfo-nya berisi password seperti `postgres://u:p@h/db`. |
| `env_example.truncated` | boolean | `true` bila file memuat lebih dari 200 key yang valid. |

Aturan membaca file:

- Kandidat hanya dicari di direktori yang diinspeksi (`root_directory`, jadi folder
  package pada monorepo), tidak pernah di atasnya. `.env`, `.env.local`,
  `.env.production`, dan nama lain tidak pernah dibuka.
- Kandidat pertama yang ada menang. Bila file itu symlink yang keluar dari direktori
  yang diinspeksi, bukan file biasa, atau lebih dari 64 KiB (65536 byte), hasilnya
  `null`; kandidat berikutnya tidak dibaca sebagai gantinya.
- Baris berbentuk `KEY=VALUE`, boleh berawalan `export `. Spasi di sekitar key dan `=`
  diabaikan. Baris kosong, baris komentar `#`, dan baris tanpa `=` dilewati. Pada nilai
  tanpa kutip, `#` yang didahului spasi memulai komentar.
- Kutip tunggal, ganda, dan backtick dibuang. Di dalam kutip ganda hanya `\"` yang
  diartikan (sebagai `"`); `\n` dan escape lain tetap teks apa adanya. Teks setelah
  kutip penutup diabaikan.
- Tidak ada interpolasi: `${VAR}` dan `$VAR` tetap teks apa adanya, tidak ada perintah
  yang dijalankan, dan environment Agent sendiri tidak pernah dibaca.
- Kutip yang tidak ditutup di baris yang sama: entrinya tetap ada dengan
  `example_value: null`, dan baris berikutnya sampai kutip penutup dilewati.
- Key yang muncul dua kali memakai nilai terakhir di posisi kemunculan pertama.

### Petunjuk database

`database_hint` selalu dikirim oleh Agent yang mengenal field ini, berupa object; field
yang tidak ada berarti Agent sebelum field ini. Tanpa bukti apa pun nilainya
`{ "likely": false, "engine": "unknown", "signals": [], "suggested_release_command": null }`.

| Field | Tipe | Arti |
| --- | --- | --- |
| `likely` | boolean | `true` tepat ketika `signals` tidak kosong. |
| `engine` | `"postgresql"` \| `"mysql"` \| `"sqlite"` \| `"unknown"` | Engine yang paling mungkin, dari sumber pertama yang tegas (urutan di bawah). |
| `signals` | array | Bukti yang ditemukan, paling banyak 16, tanpa duplikat. Bukti dari variabel dan framework datang lebih dulu daripada nama package, jadi batas 16 memotong nama package dulu. |
| `signals[].kind` | string | Salah satu nilai di tabel berikut. Nilai baru bisa ditambahkan; consumer mengabaikan nilai yang tidak dikenalnya. |
| `signals[].file` | string | File atau direktori bukti, relatif terhadap direktori yang diinspeksi. |
| `signals[].name` | string \| `null` | Lihat tabel berikut. |
| `suggested_release_command` | string \| `null` | Salah satu dari empat perintah di bawah, hanya bila terdeteksi dengan yakin. |

| `kind` | Kapan | `name` |
| --- | --- | --- |
| `env_variable` | Key tanpa komentar di `env_example` salah satu dari `DATABASE_URL`, `DB_URL`, `DB_CONNECTION`, `DB_HOST`, `POSTGRES_URL`, `POSTGRES_HOST`, `MYSQL_URL`, `MYSQL_HOST`, `PGHOST`. | Key itu. |
| `laravel_migrations` | `require` di `composer.json` memuat `laravel/framework` dan `database/migrations/` berisi setidaknya satu file `*.php`. `file` bernilai `database/migrations`. | `null` |
| `prisma_schema` | `prisma/schema.prisma` (atau `schema.prisma` di root) punya blok `datasource` dengan `provider = "..."`. | Nilai provider, atau `null` bila bukan identifier pendek. |
| `django_settings` | Ada `manage.py`, dan `settings.py` atau `settings/*.py` satu level di bawahnya menyebut `dj_database_url` atau `DATABASES`. | `"dj_database_url"` bila disebut, selain itu `"DATABASES"`. |
| `rails_database_config` | `config/database.yml` ada. | Nilai `adapter:` pertama, `null` bila berupa ERB atau tidak ada. |
| `package_dependency` | Nama package database di `package.json` (`dependencies`, `devDependencies`), `composer.json` (`require`, `require-dev`), `requirements.txt` atau `pyproject.toml`, dan `Gemfile`. Daftarnya ada di [Docker Runtime](DOCKER_RUNTIME.md#saran-variabel-dan-database). | Nama package. |

Urutan sumber `engine`: nilai `DB_CONNECTION` (`pgsql`, `postgres`, `postgresql`,
`mysql`, `mariadb`, `sqlite`), scheme `DATABASE_URL` lalu `DB_URL` (`postgres`,
`postgresql`, `mysql`, `mariadb`, `sqlite`, `file`; akhiran driver seperti
`postgresql+asyncpg` ikut dikenali), provider Prisma, adapter Rails (`postgresql`,
`mysql2`, `trilogy`, `sqlite3`), string `django.db.backends.*` di settings Django, lalu
nama package. Scheme dibaca juga ketika `example_value`-nya disembunyikan sebagai
credential, karena scheme sendiri bukan rahasia.

`suggested_release_command` diambil dari baris pertama yang cocok:

| Perintah | Syarat |
| --- | --- |
| `php artisan migrate --force` | File `artisan` dan sinyal `laravel_migrations`. |
| `python manage.py migrate` | File `manage.py` dan `django` di `requirements.txt` atau `pyproject.toml`. |
| `npx prisma migrate deploy` | Sinyal `prisma_schema`, direktori `prisma/migrations/`, dan `prisma` atau `@prisma/client` di `package.json`. |
| `bin/rails db:migrate` | File `bin/rails`, `config/database.yml`, dan direktori `db/migrate/`. |

Semua file dibaca dengan aturan yang sama dengan `Dockerfile`: harus tetap di dalam
direktori yang diinspeksi setelah symlink diikuti, harus file biasa, paling besar 1 MiB
(64 KiB untuk file contoh environment), dan listing direktori berhenti setelah 200 entri.
Tidak ada yang dijalankan, file yang tidak terbaca dilewati, dan isi repository tidak
ditulis ke log.

### Framework yang merender saat build

`frameworks` selalu dikirim oleh Agent yang mengenal field ini. Isinya framework
JavaScript yang bisa merender halaman saat build, sehingga variabel yang dibacanya saat
itu ikut tertanam di image. API memakainya untuk memutuskan bahwa perubahan variabel
butuh build baru, karena Railpack melaporkan beberapa di antaranya (Gatsby, Astro mode
server, Eleventy, Docusaurus, VitePress, SvelteKit tanpa config Vite) sebagai `node`
biasa. Field yang tidak ada berarti Agent sebelum field ini, dan API lalu kembali ke
sinyal Railpack.

| Field | Tipe | Arti |
| --- | --- | --- |
| `frameworks` | array of string | Nilai dari tabel berikut, terurut menurut string-nya dan tanpa duplikat. `[]` bila tidak ada yang cocok atau tidak ada `package.json` yang bisa dibaca. Tidak pernah `null`. |

Nilainya himpunan tertutup. Sebuah nilai dilaporkan bila salah satu nama package-nya
persis menjadi key di `dependencies` atau `devDependencies`:

| Nilai | Nama package |
| --- | --- |
| `astro` | `astro` |
| `docusaurus` | `@docusaurus/core` |
| `eleventy` | `@11ty/eleventy` |
| `gatsby` | `gatsby` |
| `next` | `next` |
| `nuxt` | `nuxt`, `nuxt3` |
| `react-router` | `@react-router/dev` (framework mode) |
| `remix` | `@remix-run/dev` |
| `sveltekit` | `@sveltejs/kit` |
| `tanstack-start` | `@tanstack/react-start`, `@tanstack/solid-start`, `@tanstack/start` |
| `vitepress` | `vitepress` |

Aturan membaca:

- Hanya `package.json` di direktori yang diinspeksi, yaitu direktori yang sama dengan
  yang dibaca `database_hint`: root directory project, atau folder package pada
  monorepo. `package.json` root workspace di atasnya tidak dibaca.
- Hanya keberadaan nama package. Versi tidak dibaca, section lain seperti
  `peerDependencies` dan `scripts` tidak dihitung, dan tidak ada yang dijalankan.
  Library yang juga dipakai tanpa build framework-nya tidak dihitung: `react-router`
  saja tidak menghasilkan `react-router`, dan `@remix-run/react` saja tidak
  menghasilkan `remix`.
- File dibaca dengan aturan yang sama dengan `database_hint`: symlink hanya diikuti
  selama tetap di dalam direktori yang diinspeksi, harus file biasa, dan paling besar
  1 MiB. File yang tidak ada, tidak bisa dibaca, terlalu besar, atau bukan JSON yang
  valid menghasilkan `[]`, tidak pernah menggagalkan inspection.

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
    "caddy-file-routing",
    "idle-sleep",
    "replicas",
    "release-command",
    "workload-metrics",
    "image-reuse",
    "image-rollback"
  ],
  "metadata": {
    "version": "0.17.0",
    "protocol_version": 4,
    "runtime_driver": "docker",
    "lifecycle_state": "active",
    "uptime_seconds": 86400,
    "runtime": {
      "region": "id-jog-1",
      "edge_address": "10.0.47.201:8180"
    },
    "detail_counts": {
      "unhealthy_details": 0,
      "recovered_workloads": 0,
      "sleeping_workloads": 0,
      "orphans": 0,
      "stale_routes": 0,
      "stale_images": 0,
      "compatibility_issues": 0,
      "crashed_details": 0,
      "traffic": 1
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
      "maximum_replicas_per_project": 4,
      "maximum_active_workloads": 20,
      "unhealthy_details": [],
      "crashed_details": [],
      "traffic": [
        {
          "project_id": "ff66ed4a-6303-4be6-8ef4-63c28b112680",
          "deployment_id": "4f1f21ef-730d-42d5-a46d-d965353cb993",
          "last_request_at": "2026-06-23T07:41:09Z"
        }
      ]
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
      "sleeping_workloads": [],
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
  "sleeping_workloads": [{ "container_id": "…", "project_id": "…", "deployment_id": "…", "status": "Exited (143) 2 hours ago" }],
  "orphans": [{ "container_id": "…", "project_id": "…", "reason": "stale stopped deployment container" }],
  "stale_routes": [{ "path": "/var/lib/sakala/caddy/sites/….Caddyfile", "project_id": "…", "deployment_id": "…" }],
  "stale_images": [{ "image_id": "…", "project_id": "…", "deployment_id": "…" }],
  "compatibility_issues": [{ "container_id": "…", "project_id": "…", "deployment_id": "…", "reason": "…" }]
}
```

`sleeping_workloads` berisi container yang berhenti karena project-nya tidur:
container itu milik deployment yang disebut route tidur project (baris pertama
route file berakhiran `(sleeping)`), dan disimpan supaya `WakeProject` bisa
menyalakannya lagi. Container seperti itu tidak masuk `orphans`. Container berhenti
yang tidak ditunjuk route tidur, misalnya deployment lama project yang sama atau
container tanpa route, tetap dilaporkan sebagai orphan dengan alasan
`stale stopped deployment container`. `workloads.stopped` tetap menghitung keduanya.
Field ini aditif pada revision 4; Agent v0.15.0 dan sebelumnya melaporkan container
project yang tidur sebagai orphan.

`project_id`/`deployment_id` bernilai `null` bila label tidak dapat dibaca;
`stale_routes.deployment_id` juga `null` untuk route generasi legacy tanpa
deployment marker. `workloads.unhealthy_details` memakai bentuk
`{ container_id, project_id, deployment_id, status, reason }`, ditambah `process`
(string, nama worker) hanya untuk container worker. Item tanpa `process` adalah replika
web. Worker yang tidak sehat tidak berarti project-nya mati.

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
| `process` | string, hanya ada untuk worker | Nama proses worker yang crash. Tidak ada berarti proses web. Worker yang crash tidak menandai project sebagai crash dan tidak menyentuh route; tampilkan di proses itu. Contoh di `examples/heartbeat/worker-processes.json`. |

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
- **Deployment dengan beberapa replika** hanya masuk list bila semua replikanya yang
  sedang berjalan crash, dan hanya sekali, dengan data replika yang terakhir berhenti.
  Selama satu replika masih melayani, deployment itu tidak dianggap crash; replika
  yang crash terlihat sebagai `ready` yang lebih kecil dari `desired` di
  [`workloads.replicas`](#replika-workload).
- **Setiap proses dinilai sendiri.** Replika web dan setiap worker dikelompokkan
  terpisah per deployment, jadi worker yang crash muncul sebagai item dengan `process`
  walaupun web masih melayani, dan sebaliknya. Worker yang keluar dengan code 0 tidak
  pernah masuk list, karena itu akhir yang disengaja (misalnya `queue:work --max-time`)
  dan Docker langsung menyalakannya lagi. Bila exit terakhirnya tidak diketahui, worker
  tetap dinilai dengan aturan crash biasa.
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

### Traffic workload

Capability `idle-sleep` hanya diiklankan driver `docker`. Node dengan capability ini
memasang route tidur saat `SleepProject`, mengecek kapasitas saat `WakeProject`, dan
mengirim `workloads.traffic`, yaitu kapan router terakhir melayani request untuk
setiap workload yang sedang berjalan. Control plane memakainya untuk memutuskan
kapan project ditidurkan; Agent sendiri tidak pernah menidurkan project. Field ini
opsional, aditif pada revision 4, dan contohnya ada di
`examples/heartbeat/workload-traffic.json`:

```json
{
  "project_id": "ff66ed4a-6303-4be6-8ef4-63c28b112680",
  "deployment_id": "4f1f21ef-730d-42d5-a46d-d965353cb993",
  "last_request_at": "2026-11-13T05:58:12Z"
}
```

| Field | Tipe | Arti |
| --- | --- | --- |
| `project_id` | UUID, wajib | Project pemilik workload, dari label container. |
| `deployment_id` | UUID, wajib | Deployment pemilik workload, dari label container. Saat redeploy, workload lama dan baru bisa sama-sama muncul dengan domain yang sama; API mencocokkannya dengan deployment aktif project. |
| `last_request_at` | RFC 3339 UTC, wajib, tidak pernah `null` | Waktu Agent pertama kali melihat counter request host ini berubah. Presisinya satu interval heartbeat (`SAKALA_HEARTBEAT_INTERVAL_SECONDS`, default 10 detik), bukan waktu request yang sebenarnya. |

Asal nilainya counter `caddy_http_requests_total` milik Caddy runtime dengan
`metrics { per_host }`, dijumlahkan per host lalu dipetakan ke workload yang berjalan
lewat label `dev.sakala.domain`. Agent membandingkan setiap bacaan dengan bacaan
sebelumnya:

- **Bacaan pertama setelah Agent start** memberi setiap host waktu start Agent, bukan
  "tidak pernah". Restart Agent tidak boleh membuat semua project tampak sepi.
- **Counter yang berubah**, termasuk yang turun karena container Caddy di-restart dan
  menghitung dari nol, berarti ada request sejak bacaan sebelumnya. Waktunya diisi
  waktu bacaan sekarang.
- **Host yang baru muncul** setelah bacaan pertama juga berarti ada request, karena
  Caddy baru membuat counter host saat request pertamanya.
- **Counter yang sama** mempertahankan waktu sebelumnya.

Cara membaca list ini:

- **List yang hadir** hanya bisa memajukan aktivitas. Workload yang tidak ada di list
  belum punya request yang dihitung Caddy sejak Caddy start, jadi API memakai waktu
  yang sudah disimpannya, atau waktu deployment selesai, sebagai gantinya. API
  sebaiknya hanya memperbarui nilainya bila `last_request_at` lebih baru.
- **List yang tidak hadir** (`traffic` dan `detail_counts.traffic` sama-sama hilang)
  berarti Agent tidak tahu: counter tidak bisa dibaca, Caddy belum memakai
  `metrics { per_host }`, atau Caddy belum melayani request apa pun sejak start. API
  tidak boleh menganggap heartbeat seperti itu sebagai tanda sepi.
- **Project yang sedang tidur tidak masuk list**, karena containernya tidak berjalan.
  Kunjungan ke halaman tidurnya membangunkan project lewat endpoint wake, bukan lewat
  heartbeat.
- Request ke host tanpa blok sendiri, termasuk header `Host` dengan port, dihitung
  Caddy sebagai `_other` dan diabaikan.
- Bila tepat jumlah request yang sama terjadi di antara restart Caddy dan bacaan
  berikutnya, perubahan itu tidak terlihat sampai request berikutnya. Kasus ini
  langka dan hanya menunda, tidak pernah memajukan, waktu tidur.

List dibatasi `SAKALA_MAX_ACTIVE_CONTAINERS` item, bukan 50, karena sebanyak itulah
workload yang bisa berjalan bersamaan; `detail_counts.traffic` menyimpan jumlah asli.
Satu heartbeat menambah satu `docker exec` ke container Caddy dan satu listing
`docker ps`. Pembacaan dibatasi 2 detik, dan kegagalannya hanya menghilangkan list
ini, tidak mengubah `status` node.

### Replika workload

Node dengan capability `replicas` mengirim `workloads.replicas`, yaitu berapa replika
yang sehat untuk setiap deployment berjalan yang punya lebih dari satu replika. Field
ini opsional, aditif pada revision 4, dan contoh heartbeat lengkapnya ada di
`examples/heartbeat/workload-replicas.json`:

```json
{
  "workloads": {
    "maximum_replicas_per_project": 4,
    "replicas": [
      {
        "project_id": "ff66ed4a-6303-4be6-8ef4-63c28b112680",
        "deployment_id": "4f1f21ef-730d-42d5-a46d-d965353cb993",
        "desired": 3,
        "ready": 2
      }
    ]
  },
  "detail_counts": { "replicas": 1 }
}
```

| Field | Tipe | Arti |
| --- | --- | --- |
| `project_id` | UUID, wajib | Project pemilik deployment, dari label container. |
| `deployment_id` | UUID, wajib | Deployment yang dihitung. Saat deploy bergulir, set lama dan baru bisa sama-sama muncul; API mencocokkannya dengan deployment aktif project. |
| `desired` | integer, wajib, minimal 2 | Jumlah replika saat deployment dijalankan, dari label `dev.sakala.replicas`. |
| `ready` | integer, wajib, 0 sampai `desired` | Replika yang menurut Docker sedang berjalan dan tidak `unhealthy`. Ini bukan probe HTTP: replika yang crash berulang bisa sebentar terhitung di antara dua crash. |

Cara membaca list ini:

- **Deployment satu replika tidak masuk list**, karena container tunggalnya sudah
  terwakili di `unhealthy_details` dan `crashed_details`.
- **Project yang sedang tidur tidak masuk list**, karena tidak ada replikanya yang
  berjalan. Deployment yang semua replikanya `exited` juga tidak muncul.
- **List yang tidak hadir** (`replicas` dan `detail_counts.replicas` sama-sama hilang)
  berarti tidak ada deployment beberapa replika yang berjalan, atau snapshot kesehatan
  gagal. API tidak boleh mengubah angka replika yang tersimpan dari heartbeat seperti
  itu.
- **`ready` di atas 0** berarti deployment masih melayani. Router berhenti mengirim
  request ke replika yang tidak bisa dihubungi selama 10 detik, lalu mencobanya lagi.
- **`ready` di bawah `desired`** biasanya sementara: Agent menyalakan lagi replika
  `exited` dari deployment yang masih melayani dalam satu atau dua interval health
  worker. Angka yang bertahan berarti replika itu gagal start 5 kali, container-nya
  hilang, atau sedang crash berulang di bawah restart policy Docker; lihat
  [Pemulihan replika](RUNTIME_HARDENING.md#pemulihan-replika).
- `maximum_replicas_per_project` selalu dikirim; `null` berarti driver node tidak
  mendukung replika.

List dibatasi `SAKALA_MAX_ACTIVE_CONTAINERS` item seperti `traffic`, dan
`detail_counts.replicas` menyimpan jumlah asli. Angkanya dihitung dari listing
`docker ps` yang sama dengan `unhealthy_details`, tanpa query tambahan.

### Metrik workload

Node dengan capability `workload-metrics` mengirim `workloads.metrics`, yaitu pemakaian
CPU, memori, dan jumlah restart setiap container workload yang sedang berjalan. Setiap
replika punya entri sendiri. Capability ini hanya diiklankan driver `docker`. Field-nya
opsional, aditif pada revision 4, dan contoh heartbeat lengkapnya ada di
`examples/heartbeat/workload-metrics.json`:

```json
{
  "workloads": {
    "metrics": [
      {
        "project_id": "ff66ed4a-6303-4be6-8ef4-63c28b112680",
        "deployment_id": "4f1f21ef-730d-42d5-a46d-d965353cb993",
        "replica": 1,
        "sampled_at": "2026-11-18T09:15:02Z",
        "cpu_percent": 112.5,
        "memory_bytes": 209715200,
        "memory_limit_bytes": 268435456,
        "restart_count": 0
      }
    ]
  },
  "detail_counts": { "metrics": 1 }
}
```

| Field | Tipe | Arti |
| --- | --- | --- |
| `project_id` | UUID, wajib | Project pemilik container, dari label `dev.sakala.project-id`. |
| `deployment_id` | UUID, wajib | Deployment pemilik container. Saat redeploy, container lama dan baru bisa sama-sama muncul; API mencocokkannya dengan deployment aktif project. |
| `replica` | integer ≥ 0, wajib | Indeks container di antara replika deployment-nya, dari label `dev.sakala.replica`. Deployment tanpa replika, termasuk container dari Agent lama, selalu `0`. |
| `sampled_at` | RFC 3339 UTC, wajib | Waktu `docker stats` selesai membaca. Semua entri dalam satu sampel memakai nilai yang sama. |
| `cpu_percent` | number ≥ 0, wajib | Pemakaian CPU dalam **persen dari satu core**, seperti dihitung `docker stats`: selisih waktu CPU container dibagi selisih waktu CPU host selama sekitar satu detik, dikali jumlah core online. Container yang sibuk di dua core melaporkan sekitar 200, jadi nilainya bisa di atas 100. Ini bacaan sesaat saat sampel diambil, bukan rata-rata satu menit. |
| `memory_bytes` | integer ≥ 0, wajib | Memori yang dipakai cgroup container menurut `docker stats`, dalam byte. Docker mencetaknya dengan empat angka penting (misalnya `35.8MiB`), jadi nilainya bisa meleset sampai sekitar 0,05%. |
| `memory_limit_bytes` | integer > 0 atau `null`, wajib | Batas memori yang diberikan ke Docker (`HostConfig.Memory`), dalam byte dan tepat. `null` berarti container berjalan tanpa batas memori. |
| `restart_count` | integer ≥ 0, wajib | `RestartCount` Docker: berapa kali restart policy Docker menyalakan ulang container sejak terakhir di-start dengan sengaja. Docker mengembalikannya ke 0 pada `docker start` dan `docker restart`, yang dipakai wake, `RestartProject`, dan pemulihan replika. Deployment baru selalu mulai dari 0 karena container-nya baru. |
| `process` | string, hanya ada untuk worker | Nama proses worker yang dijalankan container ini; `replica` lalu menghitung di antara container proses itu. Tidak ada berarti replika web, sama seperti sebelum worker ada. |

Cara membaca list ini:

- **Satu sampel dikirim satu kali.** Agent mengambil sampel paling sering sekali per
  `SAKALA_METRICS_INTERVAL_SECONDS` (default 60 detik) dan menyertakannya hanya di
  heartbeat yang mengikuti sampel itu. Heartbeat di antaranya tidak membawa list ini.
  Jadi setiap entri adalah bacaan baru dan API tidak perlu membuang duplikat. Sampel di
  heartbeat yang gagal terkirim hilang; Agent tidak mengirimnya ulang.
- **List yang tidak hadir** (`metrics` dan `detail_counts.metrics` sama-sama hilang)
  berarti tidak ada sampel baru: interval belum lewat, atau sampel gagal. API tidak
  boleh membacanya sebagai pemakaian nol atau sebagai tanda workload berhenti.
- **List kosong** berarti sampel berhasil dan tidak ada container workload yang
  berjalan, misalnya karena semua project sedang tidur.
- **Sampel gagal utuh, tidak pernah sebagian.** Bila Docker tidak bisa dibaca, sampel
  melewati batas waktu, keluaran `docker stats` tidak dikenali, atau tidak satu pun
  container yang berjalan punya nilai yang bisa dibaca, list ini dihilangkan. Agent
  mencatat satu WARN di awal rentetan kegagalan dan satu INFO saat sampel kembali
  berhasil. Kegagalan sampel tidak mengubah `status` node.
- **Container yang berhenti atau dihapus di tengah sampel dilewati**, karena Docker
  tidak lagi punya angka untuknya. Container yang sedang `restarting` juga dilewati;
  crash-nya terlihat di `crashed_details`.
- **Container release tidak masuk list.** Container itu tidak membawa label
  `dev.sakala.managed`, dan listing metrik juga membuang container apa pun yang punya
  label `dev.sakala.role`. Router dan container lain di node juga tidak masuk.

Satu sampel memakai tiga panggilan Docker: `docker ps` untuk container berlabel
`dev.sakala.managed=true`, satu `docker stats --no-stream --format '{{json .}}'`, dan
satu `docker inspect` untuk `RestartCount` serta `HostConfig.Memory`. `docker stats`
sengaja dipanggil tanpa ID container sehingga membaca semua container yang berjalan:
bila diberi satu ID yang sudah hilang, ia gagal seluruhnya tanpa mencetak apa pun.
`docker stats --no-stream` butuh sekitar 2 detik karena menunggu bacaan kedua untuk
menghitung CPU. Sampel dibatasi 12 detik dan berjalan bersamaan dengan probe kesehatan
heartbeat, bukan setelahnya.

Container job tidak masuk list, sama seperti container release, karena membawa label
`dev.sakala.role`.

List diurutkan menurut `project_id`, `deployment_id`, proses (web dulu, lalu worker
menurut nama), lalu `replica`, dan dibatasi
`SAKALA_MAX_ACTIVE_CONTAINERS` × `SAKALA_MAX_REPLICAS_PER_PROJECT` item (default 20 × 4,
yaitu 80 item, sekitar 25 KiB). `detail_counts.metrics` menyimpan jumlah asli.

### Lokasi dan kapasitas node

Tiga field membantu API menempatkan project berdasarkan region dan kapasitas, serta
menyusun tabel route Sakala Edge. Ketiganya opsional dan aditif pada revision 4. Field
yang tidak dikonfigurasi atau tidak diketahui tidak dikirim sama sekali, bukan dikirim
kosong atau `null`. Contoh heartbeat lengkapnya ada di
`examples/heartbeat/node-placement.json`:

```json
{
  "runtime": {
    "region": "id-jog-1",
    "edge_address": "10.0.47.201:8180"
  },
  "workloads": {
    "maximum_active_workloads": 20
  }
}
```

| Field | Tipe | Arti |
| --- | --- | --- |
| `runtime` | object, opsional | Hanya dikirim bila minimal satu dari dua key di bawah dikonfigurasi. |
| `runtime.region` | string, opsional | `SAKALA_AGENT_REGION`. Cocok dengan `^[a-z0-9]+(?:-[a-z0-9]+)*$`, paling panjang 32 karakter, misalnya `jkt` atau `id-jog-1`. |
| `runtime.edge_address` | string, opsional | `SAKALA_AGENT_EDGE_ADDRESS`. Alamat router runtime node yang bisa dijangkau Sakala Edge, berbentuk `host:port` tanpa skema atau path. Host berupa IPv4, IPv6 dalam kurung siku, atau nama DNS; port 1 sampai 65535 tanpa nol di depan; paling panjang 255 karakter. Contoh: `10.0.47.201:8180`, `[fd00::21]:8180`, `runtime-01.internal:8180`. Edge memakainya apa adanya sebagai upstream. |
| `workloads.maximum_active_workloads` | integer ≥ 0, opsional | `SAKALA_MAX_ACTIVE_CONTAINERS`, yaitu jumlah container terbanyak yang dijalankan node sekaligus. Dikirim setiap kali runtime melaporkan kapasitasnya; driver `noop` tidak punya batas ini, jadi tidak mengirimnya, begitu juga saat pembacaan kapasitas gagal. Bandingkan dengan `workloads.active`, yang juga menghitung setiap replika. |

API menolak seluruh heartbeat dengan `422` bila `region` atau `edge_address` tidak valid.
Karena itu Agent memeriksa kedua env var dengan aturan yang sama saat start dan menolak
berjalan bila salah satunya tidak valid, sehingga node tidak pernah mengirim heartbeat
yang pasti ditolak. Env var yang kosong dianggap tidak dikonfigurasi. Agent sedikit lebih
ketat daripada API di satu hal: nilai yang diakhiri newline ditolak, sedangkan regex PHP
di API menerimanya.

Override admin di API untuk region dan alamat Edge node menang atas nilai heartbeat.
Bila node tidak mengirim `edge_address` dan tidak punya override, project di node itu
tidak masuk tabel route Edge (`node_without_edge_address`, lihat `docs/EDGE_API.md`
di `sakala-api`).

`startup_reconciliation` adalah snapshot recovery saat process Agent dimulai,
bukan inventaris runtime live. `captured_at` menjelaskan umur snapshot secara
eksplisit; status workload terkini berada pada bagian `workloads` heartbeat dan
hasil command reconciliation eksplisit.

Collection detail yang berpotensi membesar (`unhealthy_details`,
`crashed_details`, `recovered_workloads`, `sleeping_workloads`, `orphans`, `stale_routes`, `stale_images`, dan
`compatibility_issues`) dibatasi maksimal 50 item per heartbeat. `traffic` dan
`replicas` memakai batas `SAKALA_MAX_ACTIVE_CONTAINERS`, dan `metrics` memakai batas
`SAKALA_MAX_ACTIVE_CONTAINERS` × `SAKALA_MAX_REPLICAS_PER_PROJECT`, seperti dijelaskan
di atas. Agent
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

## Database host

Node dengan `SAKALA_DATABASE_DRIVER=postgres` mengelola PostgreSQL terkelola
(ADR-020) dan mengiklankan capability `managed-postgres`. API hanya boleh mengirim
command di bagian ini ke node dengan capability itu: Agent yang lebih lama tidak
mengenal tipe command ini, dan nilai `type` yang tidak dikenal menggagalkan
deserialisasi seluruh respons polling. Node tanpa peran database yang menerimanya
gagal dengan `unsupported_runtime_command`. Hardening dan konfigurasi host ada di
[Database Host](DATABASE_HOST.md).

### Command database

Command database tidak membawa identitas workload: `project_id` dan
`deployment_id` bernilai `null`. Contoh lengkapnya ada di
`examples/commands/database-commands.json`.

| `type` | Payload | Result saat `complete` |
| --- | --- | --- |
| `CreateDatabase` | `{ database, role, password }` | `{ database, role, created }` |
| `DropDatabase` | `{ database, role }` | `{ database, role, dropped }` |
| `RotateDatabasePassword` | `{ role, password }` | `{ role }` |
| `LockDatabase` | `{ database }` | `{ database, role, locked: true, terminated_connections }` |
| `UnlockDatabase` | `{ database }` | `{ database, role, locked: false, terminated_connections: 0 }` |
| `InstallDatabaseExtension` | `{ database, extension }` | `{ database, extension, created }` |

Aturan field payload, semuanya string dan wajib ada:

| Field | Aturan |
| --- | --- |
| `database`, `role` | Cocok persis dengan `^p_[a-z0-9]{16,40}$`: awalan `p_` lalu 16 sampai 40 huruf kecil atau angka, misalnya `p_8k2m4q7x9z1c3v5b6n0d`. Nama lain, termasuk `postgres` dan template, ditolak sebelum SQL apa pun dibuat. API sebaiknya memakai nilai acak yang berbeda untuk `database` dan `role`. |
| `password` | 24 sampai 128 karakter ASCII yang bisa dicetak, tanpa spasi (byte `0x21` sampai `0x7E`). API tetap harus meng-encode-nya saat menyusun `DATABASE_URL`. |
| `extension` | Salah satu dari `pgcrypto`, `citext`, `pg_trgm`, `unaccent`, `hstore`, `vector`. `pgvector` diterima sebagai alias `vector`, dan result selalu menulis `vector`. |

Field lain di payload diabaikan, jadi API boleh menambah field sebelum Agent
membacanya. Result tidak pernah memuat password, dan password di payload diredaksi
dari pesan `fail`.

Semua command idempotent. Mengulang command yang sudah selesai berhasil tanpa
perubahan yang terlihat tenant:

- `CreateDatabase` membuat yang belum ada dan menyamakan lagi yang sudah ada: atribut role, batas koneksi, default role, privilege, dan password diset ulang ke nilai payload. `created` bernilai `false` bila database sudah ada. Role yang sudah ada tidak diubah atribut `LOGIN`-nya, jadi mengulang create tidak membuka database yang di-lock.
- `DropDatabase` mematikan login role, menghapus database beserta sesinya (`DROP DATABASE ... WITH (FORCE)`), lalu menghapus role. `dropped` bernilai `false` bila keduanya sudah tidak ada.
- `LockDatabase` dan `UnlockDatabase` mengubah `LOGIN` role owner database. Lock juga mengakhiri semua sesi role itu dan melaporkan jumlahnya di `terminated_connections`. Sesi superuser, misalnya backup, tidak diakhiri, dan superuser tetap bisa terhubung ke database yang di-lock. Alasan tidak memakai `ALLOW_CONNECTIONS` ada di [Database Host](DATABASE_HOST.md#lock).
- `RotateDatabasePassword` mengganti password role. Sesi yang sudah terbuka tetap berjalan sampai ditutup; container project memakai password baru setelah redeploy.
- `InstallDatabaseExtension` memasang extension ke schema `public` database itu sebagai superuser. `created` bernilai `false` bila extension sudah terpasang.

Agent menjalankan command database satu per satu per node, termasuk pembacaan ukuran
untuk heartbeat.

### Kode kegagalan database

| `error_code` | Arti | Tindakan API |
| --- | --- | --- |
| `invalid_runtime_command` | Payload bukan object, atau field wajib tidak ada atau bukan string. | Bug di API. |
| `database_name_invalid` | `database` atau `role` tidak cocok dengan pola nama. | Bug di API. |
| `database_password_invalid` | Password terlalu pendek, terlalu panjang, atau memuat karakter di luar ASCII yang bisa dicetak. | Bug di API. |
| `database_extension_not_allowed` | Extension di luar allow-list. | Tolak permintaan user lebih awal. |
| `database_extension_unavailable` | Extension diizinkan tetapi file-nya tidak ada di host database, misalnya `vector` di `postgres:18-alpine`. | Tampilkan sebagai belum tersedia; urusan operator. |
| `database_not_found` | `RotateDatabasePassword` untuk role yang tidak ada, atau lock, unlock, atau extension untuk database yang tidak ada. | Rekonsiliasi status resource. |
| `database_conflict` | Database ada tetapi dimiliki role lain, role sudah memiliki database lain, owner database bukan role terkelola, atau role masih memiliki objek di luar database-nya saat drop. Tidak ada yang diubah. | Periksa manual; jangan di-retry otomatis. |
| `database_admin_unavailable` | Agent tidak bisa terhubung ke instance, user DSN bukan superuser, versi PostgreSQL di bawah 15, atau instance sedang start, shutdown, atau kehabisan koneksi. | Retry nanti; node juga melaporkan `degraded`. |
| `database_operation_failed` | PostgreSQL menolak statement karena alasan lain. Pesan berisi pesan server dan SQLSTATE-nya. | Periksa manual. |
| `unsupported_runtime_command` | Node tidak punya peran database. | Bug penempatan di API. |

### Heartbeat database host

Node dengan peran database menambahkan dua key di `metadata`. Contoh lengkapnya ada
di `examples/heartbeat/database-host.json`:

```json
{
  "database": {
    "endpoint": { "host": "10.0.47.10", "port": 5432 },
    "available": true
  },
  "databases": [
    { "name": "p_8k2m4q7x9z1c3v5b6n0d", "size_bytes": 7952063 }
  ],
  "detail_counts": {
    "databases": 1
  }
}
```

| Field | Tipe | Arti |
| --- | --- | --- |
| `database` | object | Dikirim di setiap heartbeat node database, tidak pernah oleh node lain. |
| `database.endpoint.host` | string | `SAKALA_DATABASE_PUBLIC_HOST`: IPv4, IPv6 tanpa kurung siku, atau nama DNS tempat runtime node menjangkau instance lewat jaringan privat. Bahan `DB_HOST` dan `DATABASE_URL`; IPv6 perlu kurung siku di URL. |
| `database.endpoint.port` | integer 1 sampai 65535 | `SAKALA_DATABASE_PUBLIC_PORT`, default 5432. |
| `database.available` | boolean | Apakah pembacaan ukuran terakhir berhasil. Saat `false`, `status` node menjadi `degraded` dan API sebaiknya tidak membuat database baru di node ini. |
| `databases` | array, opsional | Setiap database terkelola, urut nama, paling banyak 1000 item. Hanya dikirim di heartbeat pertama setelah pembacaan baru yang berhasil, paling sering sekali per 60 detik. Heartbeat tanpa key ini tidak membawa bacaan baru, dan tidak boleh dibaca sebagai "database hilang" atau ukuran nol. List kosong berarti instance tidak punya database terkelola. |
| `databases[].name` | string | Nama database, selalu cocok dengan `^p_[a-z0-9]{16,40}$`. Database lain tidak pernah dilaporkan. |
| `databases[].size_bytes` | integer ≥ 0 | `pg_database_size()`: semua file database di disk, dalam byte. Termasuk katalog sistem, jadi database kosong sekitar 7,5 MiB. |
| `detail_counts.databases` | integer ≥ 0 | Jumlah database terkelola sebelum dibatasi; dikirim bersama `databases`. |

Region database host memakai `metadata.runtime.region` yang sudah ada
(`SAKALA_AGENT_REGION`).

## Object storage host

Node dengan `SAKALA_OBJECT_STORAGE_DRIVER=rustfs` mengelola bucket terkelola
(ADR-021) di satu instance RustFS dan mengiklankan capability `managed-s3`. API
hanya boleh mengirim command di bagian ini ke node dengan capability itu: Agent yang
lebih lama tidak mengenal tipe command ini, dan nilai `type` yang tidak dikenal
menggagalkan deserialisasi seluruh respons polling. Node tanpa peran object storage
yang menerimanya gagal dengan `unsupported_runtime_command`. Konfigurasi dan hasil
uji di RustFS sungguhan ada di [Object Storage Host](OBJECT_STORAGE_HOST.md).

### Command bucket

Command bucket tidak membawa identitas workload: `project_id` dan `deployment_id`
bernilai `null`. Contoh lengkapnya ada di `examples/commands/bucket-commands.json`.

| `type` | Payload | Result saat `complete` |
| --- | --- | --- |
| `CreateBucket` | `{ bucket, access_key_id, secret_access_key, quota_mb }` | `{ bucket, access_key_id, quota_mb, created }` |
| `DeleteBucket` | `{ bucket, access_key_id }` | `{ bucket, deleted }` |
| `RotateBucketKey` | `{ bucket, access_key_id, secret_access_key }` | `{ bucket, access_key_id }` |
| `SetBucketQuota` | `{ bucket, quota_mb }` | `{ bucket, quota_mb }` |
| `LockBucket` | `{ bucket, access_key_id }` | `{ bucket, access_key_id, locked: true }` |
| `UnlockBucket` | `{ bucket, access_key_id }` | `{ bucket, access_key_id, locked: false }` |

Aturan field payload, semuanya wajib ada:

| Field | Tipe | Aturan |
| --- | --- | --- |
| `bucket` | string | Cocok persis dengan `^b-[a-z0-9]{16,40}$`, misalnya `b-8k2m4q7x9z1c3v5b6n0d`. Juga nama policy RustFS bucket itu. |
| `access_key_id` | string | Cocok persis dengan `^SK[A-Z0-9]{16,38}$`, misalnya `SK7Q2M9X4K1C8V5B3N0D`. Juga nama user RustFS. Bukan rahasia. |
| `secret_access_key` | string | Cocok persis dengan `^[A-Za-z0-9]{32,128}$`. Hanya huruf dan angka, jadi tidak perlu escape di env file atau URL. |
| `quota_mb` | integer | 1 sampai 1048576 (MiB). Angka negatif, pecahan, atau string adalah tipe yang salah (`invalid_runtime_command`); 0 dan nilai di atas batas adalah `bucket_quota_invalid`. |

Field lain di payload diabaikan, jadi API boleh menambah field sebelum Agent
membacanya. Result tidak pernah memuat secret, dan `secret_access_key` di payload
diredaksi dari pesan `fail`.

Semua command idempotent. Mengulang command yang sudah selesai berhasil tanpa
perubahan yang terlihat tenant:

- `CreateBucket` membuat yang belum ada dan menyamakan lagi yang sudah ada: kuota, policy, dan secret diset ulang ke nilai payload, lalu akses anonim ditutup. `created` bernilai `false` bila bucket sudah ada. Status aktif atau nonaktif user tidak pernah diubah, jadi mengulang create (misalnya `buckets:reconcile`) tidak membuka bucket yang di-lock. Bila user sudah membawa policy lain, command gagal dengan `bucket_conflict` sebelum ada yang berubah. Kuota bucket ini sendiri dihitung sebagai sudah terpakai saat memeriksa kapasitas.
- `DeleteBucket` menonaktifkan lalu menghapus user (key langsung berhenti), menghapus semua object termasuk upload multipart yang belum selesai, menghapus bucket, lalu menghapus policy. Bagian yang sudah tidak ada dilewati. `deleted` bernilai `false` bila bucket, user, dan policy semuanya sudah tidak ada. Bucket yang sangat besar bisa butuh waktu lama (batas 600 detik untuk langkah ini).
- `RotateBucketKey` mengganti secret; access key ID tetap. `complete` berarti RustFS sudah menerima secret baru, dan secret lama langsung ditolak. `fail` dengan kode selain `bucket_outcome_unknown` berarti secret lama masih berlaku: bila `rc` gagal atau terlambat, Agent memeriksa dengan request S3 bertanda tangan secret baru sebelum melapor. Koneksi S3 yang sudah terbuka tidak terputus; container project memakai secret baru setelah redeploy. User yang tidak ada memberi `bucket_not_found`.
- `SetBucketQuota` mengganti kuota. Hanya kenaikan yang melewati pemeriksaan kapasitas; penurunan selalu diterima, juga di bawah pemakaian saat ini. Bucket yang tidak ada memberi `bucket_not_found`.
- `LockBucket` dan `UnlockBucket` menonaktifkan dan mengaktifkan user. Selama di-lock, key dan presigned URL yang ditandatanganinya ditolak `403`. User atau bucket yang tidak ada memberi `bucket_not_found`; key yang membawa policy bucket lain memberi `bucket_conflict`.

Pemeriksaan kapasitas untuk `CreateBucket` dan kenaikan kuota: setiap bucket di
instance harus punya kuota, jumlah kuota setelah perubahan tidak boleh melewati
`SAKALA_OBJECT_STORAGE_TOTAL_QUOTA_MB`, dan ruang disk bebas setelah semua bucket
penuh tidak boleh di bawah `SAKALA_OBJECT_STORAGE_MIN_FREE_MB`. Rinciannya di
[Object Storage Host](OBJECT_STORAGE_HOST.md#pemeriksaan-kapasitas).

Agent menjalankan command bucket satu per satu per node, termasuk pembacaan
pemakaian untuk heartbeat, dengan lock yang terpisah dari command database. Setiap
command selesai di dalam `SAKALA_COMMAND_TIMEOUT_SECONDS`, dan setiap `rc` dihentikan
di dalam container sebelum Agent berhenti menunggu, jadi tidak ada perubahan yang
masih berjalan setelah `complete` atau `fail`.

Catatan untuk tenant, diamati di RustFS 1.0.6: tulisan pertama ke bucket setelah
`CreateBucket` atau `SetBucketQuota` dijawab `503 SlowDown` sekali, yang di-retry
otomatis oleh SDK S3.

### Kode kegagalan bucket

Semua kode ini sebaiknya `user_fixable: false` di API. Contoh body `fail` untuk
setiap kode ada di `examples/failures/bucket-failures.json` (`type` command dan
`body` berisi `error_code` dan `error_message`), yang dicocokkan dengan kode oleh
test. Kode dibandingkan sebagai string persis.

| `error_code` | Arti | Tindakan API |
| --- | --- | --- |
| `invalid_runtime_command` | Payload bukan object, atau field wajib tidak ada atau tipenya salah. | Bug di API. |
| `unsupported_runtime_command` | Node tidak punya peran object storage. | Bug penempatan di API. |
| `bucket_name_invalid` | `bucket` tidak cocok dengan `^b-[a-z0-9]{16,40}$`. | Bug di API. |
| `bucket_key_invalid` | `access_key_id` atau `secret_access_key` tidak cocok dengan polanya. Pesan tidak pernah memuat nilainya. | Bug di API. |
| `bucket_quota_invalid` | `quota_mb` bernilai 0 atau di atas 1048576. | Bug di API. |
| `bucket_capacity_exceeded` | Total kuota node atau batas ruang disk bebas akan terlampaui, atau ada bucket tanpa kuota di instance. Tidak ada yang diubah. | Operator: naikkan kapasitas atau pindahkan. Jangan di-retry otomatis. |
| `bucket_admin_unavailable` | `docker exec` gagal (container tidak ada atau berhenti, Docker tidak bisa dijangkau, exit 125 sampai 127), `rc` tidak bisa menjangkau RustFS atau root key-nya ditolak, `rc` dihentikan karena melewati batas waktunya, atau jatah waktu command habis sebelum langkah berikutnya. | Retry nanti; node juga melaporkan `degraded` bila pembacaan pemakaian ikut gagal. |
| `bucket_not_found` | Bucket (`SetBucketQuota`) atau user key (`RotateBucketKey`, `LockBucket`, `UnlockBucket`) tidak ada. | Rekonsiliasi. |
| `bucket_conflict` | User key membawa policy selain policy bucket ini. Tidak ada yang diubah. | Periksa manual. |
| `bucket_operation_failed` | `rc` menolak karena alasan lain, atau output-nya tidak dipahami. Pesan berisi ujung stderr `rc` yang sudah diredaksi. | Periksa manual. |
| `bucket_outcome_unknown` | Hanya `RotateBucketKey`: mengganti secret gagal dengan cara yang tidak membuktikan perubahan itu tidak diterapkan, dan Agent tidak bisa memeriksa secret mana yang berlaku. Secret lama mungkin sudah mati. | Kirim ulang `RotateBucketKey` yang sama (idempotent); jangan kembali ke secret lama. |

### Heartbeat object storage host

Node dengan peran object storage menambahkan dua key di `metadata`. Contoh lengkapnya
ada di `examples/heartbeat/object-storage-host.json`:

```json
{
  "object_storage": {
    "endpoint": "https://s3.sakala.app",
    "region": "us-east-1",
    "capacity_mb": 5120,
    "available": true
  },
  "buckets": [
    { "name": "b-8k2m4q7x9z1c3v5b6n0d", "size_bytes": 1048576 }
  ],
  "detail_counts": {
    "buckets": 1
  }
}
```

| Field | Tipe | Arti |
| --- | --- | --- |
| `object_storage` | object | Dikirim di setiap heartbeat node object storage, tidak pernah oleh node lain. |
| `object_storage.endpoint` | string | `SAKALA_OBJECT_STORAGE_PUBLIC_ENDPOINT` sebagai origin: `https://host[:port]` tanpa path, paling panjang 255 karakter (`http://` hanya untuk host loopback). Bahan `AWS_ENDPOINT_URL_S3` dan `AWS_ENDPOINT`. |
| `object_storage.region` | string | Region tanda tangan S3, `SAKALA_OBJECT_STORAGE_REGION`, cocok dengan `^[a-z0-9]+(-[a-z0-9]+)*$` dan paling panjang 32 karakter. Bahan `AWS_DEFAULT_REGION` dan `AWS_REGION`. Bukan region penempatan, yang tetap `metadata.runtime.region`. |
| `object_storage.capacity_mb` | integer ≥ 1 | `SAKALA_OBJECT_STORAGE_TOTAL_QUOTA_MB`: jumlah kuota bucket terbanyak di node ini. |
| `object_storage.available` | boolean | Apakah pembacaan pemakaian terakhir berhasil. Agent ini selalu mengirimnya; key yang tidak ada berarti tersedia. Saat `false`, `status` node menjadi `degraded` dan API sebaiknya tidak menempatkan bucket baru di node ini. Pembacaan yang dilewati karena command bucket sedang berjalan tidak mengubah nilai ini. |
| `buckets` | array, opsional | Setiap bucket terkelola, urut nama, paling banyak 1000 item. Hanya dikirim di heartbeat pertama setelah pembacaan baru yang berhasil, paling sering sekali per 60 detik. Heartbeat tanpa key ini tidak membawa bacaan baru, dan tidak boleh dibaca sebagai "bucket hilang" atau pemakaian nol. List kosong berarti instance tidak punya bucket terkelola. |
| `buckets[].name` | string | Nama bucket, selalu cocok dengan `^b-[a-z0-9]{16,40}$`. Bucket lain di instance, misalnya bucket operator `w-...`, tidak pernah dilaporkan. |
| `buckets[].size_bytes` | integer 0 sampai 1 PiB | `usage` RustFS: object yang sudah lengkap, dalam byte. Part dari upload multipart yang belum selesai tidak dihitung. |
| `detail_counts.buckets` | integer ≥ 0 | Jumlah bucket terkelola sebelum dibatasi; dikirim bersama `buckets`. |

Region penempatan host ini memakai `metadata.runtime.region` yang sudah ada
(`SAKALA_AGENT_REGION`).

## Event and Log Payloads

Command identifier berada di URL endpoint dan tidak diulang pada body.
`sakala-api` bertanggung jawab menghubungkan record ke command/deployment serta
menetapkan sequence secara atomik.

Agent mengirim report dalam bentuk batch. Event dikirim segera, satu request
per event; log di-buffer dan dikirim per batch (maksimum 100 baris atau 512 KiB
message per request, atau setelah 200 ms) dalam urutan produksi, satu batch
per command pada satu waktu. Setiap batch membawa header `Idempotency-Key`
(UUID v4 unik per batch) dan dikirim ulang dengan key dan isi yang sama sampai
di-ack, sehingga retry transport aman: API mendeduplikasi per item berdasarkan
key dan index batch, dan sequence yang diberikan API mengikuti urutan baris.

Event adalah telemetry timeline. Status deployment yang digerakkannya hanya
diselesaikan oleh `complete`/`fail`, yang punya jalur sendiri dan tetap ketat
(retry terbatas, lalu error). Karena itu event yang tidak bisa dikirim tidak
menggagalkan command:

- Satu event dikirim dengan satu `Idempotency-Key` dan body yang sama di setiap
  percobaan, jadi retry setelah response hilang dijawab sebagai duplikat dan
  tidak tersimpan dua kali.
- Kegagalan transient di-retry dengan backoff eksponensial 1–15 detik plus
  jitter, tidak kurang dari `Retry-After` (dibatasi 5 menit), selama paling
  lama 90 detik per event, termasuk waktu request. Angka ini melewati outage
  API 77 detik di staging 2026-11-19 dan jauh di bawah timeout command default
  900 detik. Bila jeda berikutnya akan melewati budget, event langsung
  dibuang tanpa menunggu.
- Event yang tetap tidak di-ack setelah budget itu, atau ditolak `413`, `422`,
  `4xx` lain, atau `200` tanpa acknowledgement valid, dibuang dengan warning
  `deployment event was not delivered; the command continues without it`
  (field `event_type`, `undelivered_events`, `error`), dan langkah yang
  mengirimnya berjalan terus.
- Setelah satu event dibuang, event berikutnya command itu hanya dicoba sekali
  sampai ada yang terkirim lagi, supaya outage panjang menahan command satu
  budget saja, bukan satu budget per event. `command.claimed` dan event runtime
  memakai state yang sama.
- Sebelum `complete`/`fail`, Agent mencatat total per command sebagai warning
  `deployment events could not be fully delivered` (`undelivered_events`).
  Event yang dibuang tidak dikirim ulang belakangan, karena API memberi
  sequence menurut urutan tiba dan event tahap yang datang terlambat akan
  tampil di urutan yang salah.
- `409` tetap error: API tidak lagi menerima report untuk command itu (terminal,
  kedaluwarsa, atau bukan node pemilik), jadi pekerjaan yang dijelaskan event
  itu harus berhenti. Sebelum cutover, ini menggagalkan deployment seperti
  sebelumnya.

Akibatnya bagi API: selama outage, status deployment bisa tertinggal di tahap
terakhir yang terkirim sampai `complete`/`fail` datang, dan bila
`deployment.runtime.ready` yang dibuang, `image_reference` deployment itu
tetap kosong.

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

Satu item log boleh membawa `process` (string, opsional). Agent mengisinya dengan nama
worker untuk output container worker, termasuk output startup, output kegagalan, dan
follower setelah cutover, dan dengan `job` untuk output `RunJob`. Baris milik deployment
sendiri (build, release command dengan awalan `[release]`, dan replika web dengan awalan
`[runtime]`) tetap tanpa `process`, persis seperti sebelumnya. Baris setiap worker
dikirim lewat reporter sendiri dengan budget `max_total_bytes` sendiri dan status
berhenti sendiri, jadi worker yang ramai log tidak menghabiskan budget proses web, dan
`422` untuk baris worker hanya menghentikan baris worker itu. API yang menegakkan
`max_total_bytes` per command perlu menghitungnya per `process` supaya pemisahan ini
juga berlaku di sisi API. API sekarang mengabaikan key yang tidak dikenal di item log,
jadi field ini aman untuk API lama.

Tahap `DeployProject` dikirim sebagai event, tepat saat tahap itu dimulai, dalam
urutan berikut. API yang belum mengenal suatu `type` tetap menyimpannya di timeline
dan tidak mengubah status deployment, sehingga event baru aman untuk API lama.

| `type` | Kapan | Status deployment yang cocok |
| --- | --- | --- |
| `deployment.image.reused` | Hanya bila payload membawa `reuse_image_from_deployment_id` dan image-nya bisa dipakai ulang. Level `info`, metadata `image`, `source_deployment_id`, `builder`. Menggantikan empat event berikutnya. Lihat [Memakai ulang image](#memakai-ulang-image-tanpa-build). | tidak ada |
| `deployment.image.reuse_skipped` | Hanya bila payload membawa `reuse_image_from_deployment_id` tetapi image-nya tidak bisa dipakai ulang, sebelum checkout. Level `warning`, metadata `source_deployment_id` (atau `null`) dan `reason`. Build lalu berjalan seperti biasa. | tidak ada |
| `deployment.checkout.started` | Clone repository dimulai. | `cloning` |
| `deployment.analysis.started` | `railpack prepare` dimulai. Hanya pada build Railpack, karena memilih Dockerfile hanyalah memeriksa satu file. | `analyzing` |
| `deployment.workspace.detected` | Hanya pada build Railpack yang root directory-nya package workspace JavaScript, setelah analisis dimulai. Metadata-nya berbentuk sama dengan `workspace` di hasil `InspectProject`, dengan perintah yang benar-benar dipakai. | tidak ada; tetap `analyzing` |
| `deployment.build.started` | Build image dimulai. Pada Railpack, event ini kini datang setelah analisis. | `building` |
| `deployment.container.started` | `docker run` candidate akan dijalankan. | `deploying` |
| `deployment.health.started` | Container sudah jalan, probe readiness dimulai. | `health_checking` |
| `deployment.route.started` | Aplikasi sudah menjawab, route akan diaktifkan. | `routing` |
| `deployment.runtime.ready` | Route aktif dan cleanup selesai. Bukan penanda routing. | tidak ada; `succeeded` hanya dari `complete` |
| `deployment.processes.ignored` | Hanya bila `processes` ada tetapi tidak terbaca, sebelum checkout. Level `warning`, metadata `{}`. Deploy berlanjut dengan proses web saja. Lihat [Proses worker](#proses-worker). | tidak ada |
| `deployment.images.retained` | Hanya bila deployment sudah melayani tetapi image lama project-nya tidak bisa ditandai atau dihapus sesudahnya. Level `warning`, metadata `project_id`. Datang setelah `deployment.runtime.ready`; deployment tetap sukses dan image GC berkala merapikannya. | tidak ada |

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
| `200` dengan body tidak sesuai kontrak | Tidak diketahui apakah tersimpan. | Log: hentikan delivery log command ini; batch tidak dianggap terkirim. Event: buang event itu dengan warning, command lanjut. |
| `408`, `429`, `5xx`, kegagalan transport | Transien. | Log: retry dengan key dan isi yang sama, backoff eksponensial 1–15 s plus jitter, tidak kurang dari `Retry-After` (dibatasi 5 menit), tanpa batas jumlah selama command berjalan; baris berikutnya menunggu di buffer berbatas (lihat [Logging](LOGGING.md)). Event: retry dengan key dan isi yang sama dan backoff yang sama, paling lama 90 detik per event, lalu dibuang dengan warning; command lanjut. |
| `4xx` lain | Tidak akan berhasil bila diulang. | Log: hentikan delivery log command ini tanpa retry. Event: buang event itu tanpa retry, command lanjut. |
| `409` | Command terminal, bukan node pemilik, atau `Idempotency-Key` dipakai untuk payload berbeda. | Log: hentikan delivery log command ini tanpa retry. Event: error tanpa retry; langkah yang mengirimnya gagal. |
| `422` | Budget `max_total_bytes` habis atau field tidak valid. | Log: hentikan delivery log command ini tanpa retry. Event: buang event itu tanpa retry, command lanjut. |
| `413` | Body melebihi `SAKALA_LOG_MAX_REQUEST_BYTES` (1 MiB). | Hentikan delivery log, atau buang event itu; batas lokal Agent sengaja jauh di bawah nilai ini. |

Log tetap dikirim **setelah** `complete` untuk `DeployProject`: setelah
cutover, Agent menjalankan `docker logs --follow` dan melaporkan output
container di bawah identitas command deploy yang sudah `Succeeded`, termasuk
setelah Agent restart (follower dipulihkan dari label container). Budget
kumulatif `max_total_bytes` tetap berlaku; `422` atau `409` menghentikan
follower tanpa retry. Event setelah command terminal selalu ditolak `409`.
Command `Failed`, `Cancelled`, atau `Expired` tidak menerima log lagi.

Sebelum `complete`/`fail`, Agent mem-flush batch log yang tersisa, menunggu
API pulih paling lama 120 detik tetapi tidak melewati `command_timeout_seconds`
(minimal 10 detik), agar lease tidak habis. Baris yang tidak terkirim dicatat
sebagai warning beserta jumlahnya dan tidak mengubah hasil command. Bila buffer
Agent penuh selama API tidak terjangkau, baris tertua dibuang dan timeline
menerima satu baris `system` biasa yang menyebut jumlah baris yang hilang di
titik itu; tidak ada field baru.

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
| `runtime_app_host_blocked` | Aplikasi menjawab, tetapi setting framework menolak domain project, jadi setiap pengunjung akan mendapat penolakan itu: server preview atau dev Vite (403 `Blocked request. This host (…) is not allowed`, setting `preview.allowedHosts` atau `server.allowedHosts`) atau halaman debug Django (400 `DisallowedHost`, `ALLOWED_HOSTS`). Hanya dicek saat deploy. | Domain project, framework, dan perbaikannya: menambah domain ke setting itu, atau mengosongkan start command supaya hasil build disajikan sebagai situs statis. |
| `runtime_app_crashed` | Process keluar selama start. Satu restart saja sudah dihitung crash. | Exit code, atau keterangan bahwa Docker tidak lagi melaporkannya. |
| `runtime_app_exec_denied` | Kernel menolak menjalankan program di image dengan `Operation not permitted`: exit code 126 dari shell, atau 255 dari runc, disertai baris itu di output terakhir aplikasi. Di container Sakala penyebabnya file capability di luar [allowlist workload](DOCKER_RUNTIME.md#capability-workload), misalnya `cap_net_raw`. | Exit code, contoh `NET_RAW`, dan saran `setcap -r` atau image lain. Tidak menyebut environment variable. |
| `runtime_app_out_of_memory` | Container keluar dengan `State.OOMKilled` atau event `oom`, atau masih hidup dengan flag itu tetapi tidak pernah menjawab. | Batas memory yang diterapkan. |
| `runtime_port_conflict` | `PORT` dari environment berbeda dari `container_port`. Menggantikan `invalid_runtime_command` untuk kasus ini. | Kedua nilai. `PORT` yang bukan nomor port tidak diulang, hanya disebut tidak valid. |
| `runtime_root_directory_invalid` | `root_directory` tidak aman atau tidak menunjuk direktori di dalam repository. | Alasannya, tanpa mengulang path. |
| `release_command_failed` | Release command keluar dengan exit code selain 0, termasuk 126 atau 127 saat Docker tidak bisa menjalankan `sh` di image. Tidak ada replika yang dinyalakan. | Exit code. Bila Docker melaporkan container dibunuh karena kehabisan memory, juga batas memory yang diterapkan. |
| `release_command_timeout` | Release command belum selesai sampai `SAKALA_RELEASE_COMMAND_TIMEOUT_SECONDS`. Container dibunuh dan dihapus, dan tidak ada replika yang dinyalakan. | Batas waktu dalam detik. |
| `worker_start_failed` | Sebuah worker keluar atau di-restart Docker sebelum route berpindah, apa pun exit code-nya termasuk 0, atau tidak masih berjalan `SAKALA_WORKER_STARTUP_SECONDS` setelah dinyalakan. Semua container baru dihapus, versi lama tetap melayani bersama worker-nya. | Nama worker dan exit code, atau batas memory bila Docker melaporkan OOM. |

`worker_start_failed` juga bisa diperbaiki user: perintah worker atau memorinya. Saran
pemetaan di API: kategori start, dengan hint membaca log yang `process`-nya worker itu.
Kode kegagalan `RunJob` (`job_failed`, `job_timeout`, `job_image_missing`) ada di
[Job (`RunJob`)](#job-runjob).

Kedua code release bisa diperbaiki user: perintahnya, migrasinya, atau batas waktunya.
Saran pemetaan di API: kategori tersendiri sebelum start, dengan hint untuk membaca log
`[release]`. Pesannya tidak menyebut perintah atau output-nya.

`runtime_app_host_blocked` bisa diperbaiki user. Saran pemetaan di API: kategori start,
dengan hint menambahkan domain project ke `allowedHosts` atau `ALLOWED_HOSTS`, atau
mengosongkan start command (misalnya `vite preview`) supaya Sakala menyajikan hasil build
sebagai situs statis. Deployment gagal, bukan berhasil dengan peringatan, karena setiap
pengunjung akan mendapat 403 atau 400, dan dengan gagal versi sebelumnya tetap melayani.
Aturannya ada di [Docker Runtime](DOCKER_RUNTIME.md#host-yang-ditolak-framework).

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
serta `complete`/`fail` di-retry hanya untuk kegagalan transient dan memakai
`Idempotency-Key` yang sama pada setiap percobaan: `complete`/`fail` dengan
backoff terbatas (3 percobaan), event sampai di-ack atau budget 90 detik per
event habis, log sampai di-ack, ditolak, atau batas flush akhir habis (lihat
tabel respons di atas). Lease
expiry, offline detection, dan recovery command yang terputus dimiliki
`sakala-api` dan dijelaskan pada
[Semantik control plane yang diandalkan Agent](#semantik-control-plane-yang-diandalkan-agent).
