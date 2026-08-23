# Docker Runtime v1

`DockerRuntimeExecutor` adalah executor opt-in untuk deployment HTTP app stateless pada satu runtime node. Default binary tetap `noop`.

## Flow

```txt
validate command
-> init repository dan fetch immutable commit SHA
-> pilih Dockerfile atau Railpack
-> enforce build deadline
-> docker buildx build --load
-> docker run candidate pada sakala-runtime
-> basic Docker health/running check
-> tulis route Caddy secara atomik
-> validate dan reload Caddy
-> ambil startup logs
-> hapus container deployment sebelumnya
```

Startup agent juga menjalankan scan orphan secara detection-only. Detail guard Phase 9 ada di [Runtime hardening](RUNTIME_HARDENING.md).

Reconciliation juga mendiscover setiap container managed yang memiliki label
project/deployment valid dan menyimpan status Docker-nya dalam report lokal.
Container dengan label tidak lengkap atau status `Created`/`Exited`/`Dead`
tetap dilaporkan sebagai orphan, bukan dihapus otomatis.

Sebelum polling command, driver Docker menjalankan preflight fatal untuk Git, Docker daemon, Docker Buildx, Railpack, container Caddy, network runtime, filesystem workspace (termasuk akses direktori), disk workspace, serta direktori route. Node tidak mulai menerima command bila salah satunya gagal; log menyertakan nama check dan detail yang aman untuk operator.

Image ditag berdasarkan project, commit, dan deployment. Image dan container diberi ownership labels agar discovery dan cleanup tidak menyentuh workload di luar Sakala:

- `dev.sakala.managed=true`
- `dev.sakala.project-id=<uuid>`
- `dev.sakala.deployment-id=<uuid>`
- `dev.sakala.workload-kind=web`
- `dev.sakala.agent-id=<agent-id>`

Label `project-id` dan `deployment-id` juga dipasang pada image Dockerfile dan
Railpack. Label image belum menjadi izin untuk menghapus image secara otomatis:
retensi image aktif/previous tetap memerlukan policy dan desired state yang
belum tersedia dari control plane.

## Batas Keamanan MVP

- Repository hanya `https://github.com` publik tanpa credential URL.
- Checkout harus memakai full 40-character commit SHA.
- Tidak ada shell interpolation; executable dan arguments dikirim langsung ke process API.
- Runtime env ditulis sementara ke file mode `0600`, dibaca `docker run --env-file`, lalu dihapus.
- Container memakai memory, CPU, PID limit dari command control plane, `no-new-privileges`, dan drop seluruh capability.
- Tidak ada host volume atau Docker socket di aplikasi user.
- Caddy dan aplikasi user tidak menerima Docker socket.

Ini belum merupakan multi-tenant sandbox. Docker daemon access tetap privileged; pilot harus memakai node khusus dan workload terkontrol.

## Payload

Lihat `examples/commands/deploy-project.json`. `builder=auto` memilih root Dockerfile bila tersedia dan Railpack bila tidak tersedia. Dockerfile pada subdirectory dan manual command belum didukung.

Resource request berbentuk `memory_mb`, `cpu_millis`, dan `pids_limit`. API adalah source of truth untuk policy produk; agent hanya memakai fallback lokal saat field kosong dan menolak nilai yang melampaui hard maximum node. Completion result mengembalikan nilai `requested_resources` dan `applied_resources` agar API dapat menyimpan konfigurasi runtime aktual.

## Verifikasi

Test default memakai fake process runner dan tidak menyentuh daemon. Integration test nyata perlu dijalankan secara opt-in pada node disposable setelah `sakala-api` menyediakan command contract Phase 6.
