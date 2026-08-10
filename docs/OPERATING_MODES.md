# Operating Modes

Sakala Agent memiliki dua konfigurasi yang sengaja dipisahkan:

- **agent mode** menentukan apakah agent berkomunikasi dengan `sakala-api`;
- **runtime driver** menentukan apakah command hanya disimulasikan atau benar-benar mengubah runtime node.

Karena keduanya independen, `connected` tidak otomatis berarti Docker aktif dan `noop` tidak berarti agent terputus dari API.

## Matriks Mode

| Agent mode | Runtime driver | Perilaku | Kegunaan |
| --- | --- | --- | --- |
| `local` | `noop` | Menulis heartbeat dan polling tick ke log tanpa request API atau perubahan host. | Default development dan smoke test binary. |
| `connected` | `noop` | Mengirim heartbeat, polling, claim, event, log, dan completion ke API; eksekusi command tidak mengubah host. | Integration test kontrak API dan lifecycle command. |
| `local` | `docker` | Tidak mengambil command dari API. Adapter Docker hanya tersedia untuk startup reconciliation dan test yang dipanggil eksplisit. | Pemeriksaan kesiapan node; bukan flow deployment normal. |
| `connected` | `docker` | Mengambil command dari API dan menjalankan inspection/deployment nyata melalui Git, Docker Buildx, Railpack, dan Caddy. | Runtime node MVP yang sudah dikonfigurasi dan direview. |

Konfigurasi paling aman tetap:

```dotenv
SAKALA_AGENT_MODE=local
SAKALA_RUNTIME_DRIVER=noop
```

Untuk menguji integrasi API tanpa menyentuh Docker:

```dotenv
SAKALA_AGENT_MODE=connected
SAKALA_RUNTIME_DRIVER=noop
```

`connected + noop` berguna karena seluruh alur control plane tetap nyata. Agent tetap mengautentikasi, mengirim heartbeat, mengambil dan mengklaim command, serta melaporkan hasil. Hanya operasi privileged pada host yang diganti dengan hasil dummy yang terkontrol.

## Agent Bukan HTTP Server

Agent tidak membuka port untuk menerima command. Arah komunikasinya selalu outbound:

```txt
sakala-api menyimpan Pending command
        ^
        | GET /api/agent/v1/commands
        | POST claim/events/logs/complete/fail
        |
sakala-agent pada runtime node
```

`SAKALA_API_URL=http://localhost:8000` berarti API Laravel diharapkan tersedia pada alamat tersebut. Alamat itu bukan URL milik agent.

Polling hanya menemukan command yang eligible untuk node. Agent tetap wajib melakukan claim sebelum eksekusi. Claim adalah batas ownership atomik: bila command sudah diklaim agent lain atau tidak lagi eligible, API harus menolak claim dan agent tidak boleh mengeksekusinya.

Setelah command berstatus `Claimed`, `Running`, `Succeeded`, `Failed`, `Cancelled`, atau `Expired`, command tersebut tidak boleh terus dikembalikan sebagai command pending. Detail kontraknya ada di [AGENT_API.md](AGENT_API.md).

## Runtime Driver

### `noop`

`NoopRuntimeExecutor` memenuhi kontrak runtime tanpa menjalankan Git, Docker, Railpack, atau Caddy. Driver ini dipakai untuk:

- memastikan worker, authentication, polling, claim, dan reporting benar;
- menjalankan CI/integration test tanpa akses host privileged;
- mencegah perubahan host karena salah konfigurasi awal.

Noop bukan runtime aplikasi. Completion dari noop hanya membuktikan lifecycle command, bukan keberhasilan deployment nyata.

### `docker`

`DockerRuntimeExecutor` adalah driver nyata yang opt-in. Untuk command yang didukung, driver ini dapat:

- checkout commit repository yang immutable;
- menjalankan inspection ringan dengan scanner Sakala dan `railpack info`;
- membangun image melalui Dockerfile atau Railpack;
- menjalankan candidate container dengan resource limit;
- memeriksa readiness;
- mengaktifkan route Caddy;
- melaporkan event, log, dan hasil ke API.

Aktifkan hanya pada node yang memenuhi requirement di [DOCKER_RUNTIME.md](DOCKER_RUNTIME.md) dan [RUNTIME_HARDENING.md](RUNTIME_HARDENING.md).

## Topologi Caddy MVP

Implementasi runtime saat ini memakai topologi referensi `sakala-infra`:

```txt
sakala-caddy container
  -> attached to sakala-runtime
  -> resolves application container by Docker DNS name
  -> reads generated *.Caddyfile
  -> validate/reload through docker exec
```

Binary Caddy yang terpasang langsung pada host, misalnya `/usr/bin/caddy`, **belum digunakan oleh adapter saat ini**. Caddy host tidak otomatis dapat me-resolve nama container pada Docker network. Mengganti perintah reload saja akan menghasilkan konfigurasi yang valid secara sintaks tetapi upstream tidak terjangkau.

Untuk MVP saat ini, gunakan Caddy container dari `sakala-infra` dan pastikan service Caddy host tidak berebut port yang sama.

Jika Caddy host harus tetap berjalan, gunakan sebagai **outer edge**, bukan pengganti router container:

```txt
internet/client
-> host Caddy :80/:443
-> 127.0.0.1:8080 (sakala-caddy)
-> application container melalui sakala-runtime
```

Host Caddy memakai konfigurasi statis untuk meneruskan domain `*.run.staging.sakala.dev` atau `*.run.sakala.dev` ke port loopback `sakala-caddy`. Header `Host` harus dipertahankan agar route per-project tetap dipilih oleh Caddy container. Agent hanya mengelola dan me-reload router container; TLS dan policy edge luar tetap menjadi tanggung jawab operator. Port container Caddy sebaiknya bind ke loopback, bukan seluruh interface publik.

Topologi host Caddy membutuhkan implementasi utuh yang berbeda:

1. agent mengalokasikan host port per container;
2. container hanya mem-publish port ke loopback, misalnya `127.0.0.1:31001:3000`;
3. route Caddy mengarah ke `127.0.0.1:31001`;
4. alokasi port dipersist dan dibersihkan dengan lifecycle deployment;
5. adapter host memvalidasi dan me-reload Caddy dengan permission yang terbatas.

Jangan memberi agent akses `sudo` arbitrer hanya untuk reload Caddy. Adapter host harus memakai ownership file dan mekanisme service yang dibatasi secara eksplisit.
