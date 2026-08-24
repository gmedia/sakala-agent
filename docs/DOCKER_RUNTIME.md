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
Railpack. Image aktif dilindungi oleh referensi container. Image deployment
sebelumnya dipertahankan minimal selama `SAKALA_IMAGE_GC_MAX_AGE_SECONDS`
(default tujuh hari) dan baru eligible saat dangling; cleanup tetap dibatasi
label ownership Sakala dan tidak memakai `docker image prune -a`. Startup
reconciliation hanya melaporkan kandidat; prune baru dijalankan oleh command
`CleanupRuntime` yang membawa `approved: true`.

## Batas Keamanan MVP

- Repository memakai URL canonical `https://github.com` tanpa credential pada URL. Repository private memakai credential lease sementara melalui `GIT_ASKPASS` dan tidak menaruh token di arguments, remote URL, atau log.
- Checkout harus memakai full 40-character commit SHA.
- Tidak ada shell interpolation; executable dan arguments dikirim langsung ke process API.
- Runtime env ditulis sementara ke file mode `0600`, dibaca `docker run --env-file`, lalu dihapus.
- Container memakai memory, CPU, PID limit dari command control plane, `no-new-privileges`, dan drop seluruh capability.
- Tidak ada host volume atau Docker socket di aplikasi user.
- Caddy dan aplikasi user tidak menerima Docker socket.

Ini belum merupakan multi-tenant sandbox. Docker daemon access tetap privileged; pilot harus memakai node khusus dan workload terkontrol.

## Payload

Lihat `examples/commands/deploy-project.json`. `builder=auto` memilih root Dockerfile bila tersedia dan Railpack bila tidak tersedia. Dockerfile pada subdirectory dan manual command belum didukung.

Resource request berbentuk `memory_mb`, `cpu_millis`, dan `pids_limit`. API adalah source of truth untuk policy produk; agent hanya memakai fallback lokal saat field kosong dan menolak nilai yang melampaui hard maximum node. Completion result mengembalikan nilai `requested_resources` dan `applied_resources` agar API dapat menyimpan konfigurasi runtime aktual. Jika finalisasi setelah route cutover ditunda, result juga membawa `finalization_deferred=true`; API wajib menghentikan deployment sebelumnya dengan `StopProject` karena Agent tidak menghapus running superseded workload secara otomatis.

Kegagalan cepat saat menghentikan atau menghapus container deployment sebelumnya tidak diabaikan. Agent tetap mencoba ready event serta memasang log follower untuk deployment baru, kemudian mengembalikan post-commit error supaya completion membawa `finalization_deferred_reason=runtime_error`.

## Verifikasi

Test default memakai fake process runner dan tidak menyentuh daemon. Integration test nyata perlu dijalankan secara opt-in pada node disposable dengan `sakala-api` yang mendukung protocol revision 4.
