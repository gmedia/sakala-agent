# Runtime Hardening v1

Phase 9 menambah guard untuk pilot terbatas tanpa menganggap Docker host sebagai sandbox multi-tenant yang sempurna.

## Ownership Policy

```txt
sakala-api
  plan dan entitlement
  max project per user/workspace
  allowed active deployment
  admin stop/suspend decision
  requested resource profile

sakala-agent
  enforce command resource profile
  hard maximum dan capacity guard node
  process/build deadline
  detect actual orphaned container
  report requested/applied resource dan failure code
```

Agent tidak mengetahui plan berbayar, membership, atau kuota workspace. Jika user memperoleh resource lebih besar, API menghitung profile baru dan mengirim command redeploy. Replacement container memakai profile baru; route hanya berpindah setelah health check berhasil.

## Timeout dan Cancellation

- `SAKALA_BUILD_TIMEOUT_SECONDS` membatasi keseluruhan builder operation.
- `SAKALA_COMMAND_TIMEOUT_SECONDS` membatasi lifecycle runtime setelah command diklaim.
- Setiap subprocess juga memakai deadline command sebagai fallback.
- Child dijalankan sebagai process-group leader pada Unix. Timeout, reporting error, task cancellation, atau shutdown menjatuhkan seluruh process group.
- Shutdown lebih dulu membatalkan token semua command aktif, memberi waktu `SAKALA_SHUTDOWN_GRACE_SECONDS` agar candidate/workspace dibersihkan dan failure kritis dikirim, lalu hanya meng-abort task yang masih tersisa.
- Cancellation dikirim sebagai `runtime_cancelled`; timeout tetap memakai `runtime_timeout`. Build, container, health, routing, capacity, dan filesystem memiliki code terpisah.

Build timeout harus lebih pendek daripada command timeout agar runtime sempat menjalankan cleanup dan mengirim failure status.

## Continuous Workload Health

Saat deploy, route hanya aktif setelah `DockerHealthChecker` membuktikan aplikasi
menjawab HTTP pada port-nya. Detail probe ada di
[Docker Runtime](DOCKER_RUNTIME.md#readiness). Setelah deploy selesai, worker
health lokal menjalankan snapshot batch setiap
`SAKALA_RUNTIME_HEALTH_INTERVAL_SECONDS` (default 30 detik) terhadap container
aktif dengan label `dev.sakala.managed=true` dan identity project/deployment
yang valid.

Snapshot ini membaca status Docker saja dan tidak mem-probe HTTP. Ia membedakan
`healthy/running`, `health: starting`, `unhealthy`, dan `Restarting`. State
terakhir disimpan hanya di memori Agent; perubahan saja yang dicatat pada log
terstruktur beserta container, project, deployment, status, dan alasan aman.
Pengecekan mendapat jitter deterministik dari agent ID dan satu query batch per
interval. Tidak ada environment atau secret container yang dibaca.

`docker ps` tanpa `--all` tetap memuat container `Restarting`, yang merupakan
wujud crash di bawah policy `--restart unless-stopped`. Heartbeat melaporkan
container itu ke control plane lewat `workloads.crashed_details`. Setelah minimal 3
restart, Agent terus melaporkannya sampai ia bertahan hidup 5 menit. Restart yang lebih
sedikit dan berakhir dengan aplikasi berjalan hanya dicatat pada log operator. Container
`Exited` sengaja tidak dianggap crash, karena stop dan sleep juga berakhir di sana
dengan exit code apa pun. Semantik lengkapnya ada di
[Sakala Agent API](AGENT_API.md#heartbeat-payload).

Worker ini **tidak** melakukan restart otomatis, dan Agent tidak memutuskan
recovery. Docker sendiri yang terus me-restart container yang crash sesuai policy.
Keputusan produk atas laporan itu berada di `sakala-api`.

## Runtime Log Lifecycle

Setelah candidate sehat, route aktif, dan startup log terkirim, runtime memulai follower `docker logs --follow --tail 0`. Follower berjalan sebagai task milik `DockerContainerEngine`, sehingga bukan task yang terlepas dari lifecycle agent. Saat shutdown, seluruh task follower dibatalkan dan process group subprocess dihentikan sebelum binary keluar.

Follower memakai reporter deployment yang sama dan seluruh baris tetap melewati redaction core. Kegagalan reporting menghentikan follower dengan warning operator; retry/cursor belum diterapkan sampai API memiliki kontrak resume yang eksplisit.

## Capacity Guard

`SAKALA_MAX_ACTIVE_CONTAINERS` adalah hard admission guard untuk project aktif pada steady state. Agent melakukan pemeriksaan awal dan pemeriksaan authoritative yang di-serialize tepat sebelum `docker run`, sehingga deployment project baru yang concurrent tidak dapat melewati batas. Redeploy project aktif mengizinkan satu candidate surge sementara untuk cutover sehat, lalu container lama dihentikan dan dihapus.

Route activation adalah commit point. Sebelum titik ini cancellation/error
mempertahankan deployment lama dan membersihkan candidate. Setelah titik ini,
traffic sudah berpindah sehingga cancellation atau reporting failure tidak lagi
mengubah hasil runtime menjadi gagal. Finalisasi cleanup/log follower memiliki
grace 30 detik; ketika grace habis, hasil cutover tetap dilaporkan sukses dan
result membawa `finalization_deferred=true`. Control plane wajib menargetkan
deployment sebelumnya dengan `StopProject`; local reconciliation/GC tidak
menghapus superseded container yang masih running tanpa desired-state command.

Guard ini bukan aggregate scheduler dan belum menghitung total memory/CPU reservation. Penempatan lintas node tetap pekerjaan control plane setelah MVP.

Perubahan resource user dilakukan melalui redeploy dengan payload resource baru. Agent tidak memakai `docker update` untuk mengubah container aktif secara diam-diam karena deployment harus tetap memiliki desired state, applied state, image, health check, dan route cutover yang dapat diaudit.

## Orphan Detection

Saat startup, Docker executor memindai container dengan label `dev.sakala.managed=true`. Container ditandai orphan bila:

- identity label project/deployment hilang atau invalid; atau
- status container `Created`, `Exited`, atau `Dead`.

Phase 9 hanya melaporkan warning. Auto-removal sengaja tidak dilakukan karena agent belum memiliki desired-state snapshot dari API; menghapus berdasarkan observasi host saja berisiko menghapus workload yang masih dibutuhkan.

Workspace checkout berbeda: saat startup, agent dapat menghapus direktori terbengkalai yang namanya tepat UUID command Sakala dan umurnya melewati `SAKALA_WORKSPACE_GC_MAX_AGE_SECONDS`. GC tidak mengikuti symlink, tidak menyentuh nama lain, dan tidak dijalankan saat command aktif diproses.

Setelah reconciliation, aplikasi Agent juga mengambil snapshot health sekali untuk
seluruh workload aktif yang ditemukan. Workload tidak ready dicatat dengan
container/project/deployment/status/reason yang aman. Ini memberi operator
verifikasi ringan segera setelah restart tanpa menunggu interval health worker;
Agent tetap tidak menjalankan restart otomatis atau mutasi recovery.

Probe host heartbeat dimiliki adapter runtime dan memakai `ProcessRunner` dengan
deadline dua detik, process group termination, serta child reaping. Versi Git,
Versi Git, Docker, Buildx, dan Railpack di-cache sekali sebagai metadata. Status
operasional tidak berasal dari cache tersebut: setiap heartbeat memeriksa ulang
Docker daemon, container Caddy, network runtime, dan kemampuan menulis workspace.
Probe kapasitas, health runtime, `df`, dan `du` juga dibatasi agar satu command
host yang macet tidak menahan heartbeat maupun shutdown tanpa batas. Core hanya
memanggil port runtime.

Heartbeat `active` hanya menjadi `ready` ketika telemetry, capacity, health
snapshot, dependency runtime, dan disk guard operasional. Probe failure atau
critical disk pressure menghasilkan status `degraded`; lifecycle
draining/drained/maintenance tetap memiliki status khususnya sendiri.

Container yang dibuat oleh Agent revision lama mungkin belum memiliki label
domain, port, command ID, atau batas log yang dibutuhkan recovery modern. Agent
tetap menginventaris container tersebut dan melaporkan `compatibility_issues`
di heartbeat tanpa menggagalkan recovery workload lain. Container itu tidak
dipasangi log follower baru; operator harus melakukan redeploy untuk menulis
label canonical lengkap.

Sebelum checkout deployment baru, Agent juga membaca free space filesystem
workspace dengan `df -Pk`. Bila jumlahnya di bawah
`SAKALA_MIN_WORKSPACE_FREE_MB`, command gagal dengan
`runtime_disk_pressure` sebelum Git atau builder dimulai. Guard ini tidak
menjalankan `docker image prune -a` atau cleanup host generik; keputusan
retention dan pembersihan artifact lama tetap harus Sakala-owned.

## Isolation Decision

Maintainer memutuskan runtime node memakai Docker rootful dengan `runsc`, dengan alasan dan pembuktian di [gVisor](#gvisor). Untuk workload publik tidak tepercaya, target berikutnya adalah isolated builder yang terpisah dari runtime daemon, karena `runsc` hanya berlaku pada workload. Rootless mode sendiri mengurangi privilege, tetapi tidak menggantikan tenant isolation, network policy, cache isolation, dan secret boundary.

## Batas versi dan riset isolasi lanjut

Node pilot harus memakai Docker Engine dengan Docker Buildx yang tersedia melalui
`docker buildx version`, serta Railpack CLI yang kompatibel dengan frontend yang
dipin pada `SAKALA_RAILPACK_FRONTEND`. Agent tidak mengklaim kompatibilitas untuk
versi dependency yang tidak lolos preflight; versi aktual dicatat oleh preflight
operator dan harus diuji bersama image frontend sebelum upgrade node.

Pilihan berikut telah dievaluasi sebagai arah hardening, bukan fitur yang
diaktifkan diam-diam oleh Agent v1:

- **Rootless Docker/BuildKit:** mengurangi privilege daemon, namun memerlukan
  validasi storage, networking, dan cache pada node khusus.
- **User namespaces serta seccomp/AppArmor:** pertahanan tambahan yang perlu
  profile workload dan verifikasi kompatibilitas aplikasi sebelum diwajibkan.
- **gVisor:** arah yang dipilih untuk runtime node, lihat [gVisor](#gvisor). Fitur
  sudah siap, tetapi belum diaktifkan di production.
- **containerd:** kandidat backend runtime terpisah; kontrak `RuntimeExecutor`
  sengaja tidak mengikat core ke Docker.
- **microVM/Firecracker:** kandidat isolasi paling kuat, tetapi bukan pengganti
  langsung Docker tanpa control plane, image, networking, dan observability
  model baru.

Sampai evaluasi tersebut selesai, Agent hanya mendukung node Docker khusus
dengan workload terkontrol; ia tidak mengiklankan salah satu mekanisme isolasi
lanjutan tersebut sebagai aktif, kecuali `metadata.runtime_dependencies.container_runtime`
pada heartbeat yang menyebut runtime konfigurasi node.

## gVisor

`SAKALA_CONTAINER_RUNTIME=runsc` menjalankan workload di gVisor, yaitu kernel
user-space yang menangani syscall aplikasi sehingga kode tenant tidak langsung
berhadapan dengan kernel host (ADR-014). Agent menambahkan `--runtime runsc` pada
`docker run` workload saja. Build tidak memakai runtime ini.

Bila Docker menolak runtime atau workload gagal di bawahnya, deployment gagal dengan
pesan dari Docker dan tidak dicoba ulang dengan runtime lain. Workload yang sudah
berjalan sebelum konfigurasi berubah tetap di runtime lamanya sampai di-redeploy.

### Keputusan: Docker rootful dengan `runsc`

Per 2026-11-06 maintainer memutuskan **runtime node memakai Docker rootful dengan
`runsc`**. Keputusan ini tercatat di ADR-014. Fiturnya sudah siap, tetapi belum
diaktifkan di production. Cara memindahkan node ada di README `sakala-deployment`.

Alasannya, di Docker rootless `runsc` hanya bisa berjalan dengan `--ignore-cgroups`,
dan dengan flag itu limit memory, CPU, dan PID per workload tidak berlaku. Hasil
pengujian pada model node (systemd, Docker rootless 29.9.3, cgroup v2 dengan
delegasi):

| Runtime | Hasil |
| --- | --- |
| `runc` | Limit berlaku: alokasi 512 MiB dalam limit 256 MiB berakhir exit 137 dan `OOMKilled=true`. |
| `runsc` tanpa flag tambahan | Gagal start: `creating container: systemd error: Permission denied`. |
| `runsc --systemd-cgroup` | Gagal start dengan pesan yang sama. |
| `runsc --rootless` | Gagal start: `Rootless mode not supported with "create"`. |
| `runsc --ignore-cgroups` | Berjalan, tetapi alokasi 512 MiB dalam limit 256 MiB berhasil, `--cpus 0.5` memakai 196% CPU, dan `--pids-limit 32` terlewati dengan 117 PID. |

Pada Docker rootful dengan `runsc` tanpa `--ignore-cgroups`, ketiga limit berlaku:
alokasi berlebih berakhir exit 137 dengan `OOMKilled=true`, `--cpus 0.5` terukur 50%
CPU, dan Agent melaporkan `runtime_app_out_of_memory` seperti biasa. Pembuktian di node
staging ada di tabel `systrap` di bawah.

Harga keputusan ini, daemon berjalan sebagai root, sehingga akses ke socket-nya setara
root. Batas isolasi utamanya menjadi gVisor, dan ADR-014 sudah memisahkan runtime node
dari control plane, jadi dampak itu terbatas pada node tersebut.

Alternatif yang ditolak adalah Docker rootless dengan `--ignore-cgroups`. Daemon memang
tetap tanpa root, tetapi satu tenant bisa menghabiskan memory dan CPU seluruh node. Limit
gabungan pada unit `docker.service` user bisa melindungi host, tetapi tidak melindungi
tenant satu sama lain, dan limit gabungan itu belum diuji.

Node yang masih rootless tetap didukung Agent selama masa pindah, tetapi tidak dengan
limit per workload di bawah `runsc`.
Agent tidak menyembunyikan kondisi ini. Preflight mencatat peringatan non-fatal
`container-runtime-limits`, dan heartbeat mengirim
`container_runtime_limits_enforced: false`. Nilai `applied_resources` pada hasil
deployment tetap menyebut limit yang diminta, jadi control plane perlu membaca
field itu sebelum menjanjikan limit kepada user.

### Hal lain yang berubah di bawah gVisor

| Hal | Yang terukur |
| --- | --- |
| Health probe | Berjalan. Probe dijalankan dari container Caddy dan menjangkau workload lewat nama di `sakala-runtime`. |
| Log, stop, restart, sleep, wake | Berjalan. `docker stop` mengirim SIGTERM ke aplikasi, dan `docker exec` juga berjalan. |
| Crash saat start | `runtime_app_crashed` beserta exit code dan log terakhir, sama seperti di runc. |
| DNS keluar | **Rusak tanpa konfigurasi.** DNS bawaan Docker di `127.0.0.11` berada di sisi host network stack gVisor, dan `--dns` tidak mengubahnya. Isi `SAKALA_CONTAINER_DNS`. Tanpa itu preflight mencatat peringatan `container-dns`. |
| Nama container lain dari dalam workload | Tidak ter-resolve, bahkan dengan `SAKALA_CONTAINER_DNS`. Workload Sakala tidak seharusnya memakai jalur ini. |
| Overhead memory | Proses Sentry, kernel user-space gVisor, ikut dihitung dalam limit. Aplikasi Node kecil gagal di 64 MiB dan berjalan di 128 MiB. |
| PID | Thread Sentry ikut dihitung. Aplikasi Node kecil memakai sekitar 50 PID di platform `ptrace`, jadi `--pids-limit 32` gagal. Angka untuk `systrap` belum terukur. |

Tabel di atas memakai platform `ptrace` di WSL2, karena `systrap` gagal di sana dengan
`failed to install seccomp notify rules: device or resource busy`. Masalah itu milik
WSL2.

Pengujian `systrap` dilakukan pada 2026-11-06 di node staging: VM KVM tanpa nested
virtualization, kernel 7.0, Docker rootful, cgroup v2, `runsc` release-20261102.0.

| Hal | Yang terukur |
| --- | --- |
| Limit memory | Alokasi 200 MB dalam limit 64 MiB berakhir exit 137. |
| Limit CPU | `--cpus 0.5` terukur sekitar 55%. |
| Limit PID | Berlaku, tetapi berbeda dari `runc`: 300 proses dalam `--pids-limit 64` membuat seluruh sandbox berhenti dengan exit 2, sedangkan di `runc` hanya `fork` yang gagal. Hasil ini disimpulkan dari container yang berhenti, tanpa pesan di log. |
| Aplikasi Node dan PHP kecil | Berjalan. Node memakai 23 MiB dan 31 PID dalam limit 128 MiB. `php -S` 8.4 memakai 26 MiB dan 31 PID dalam limit 256 MiB. |
| Jaringan | Terjangkau lewat nama di network bridge, `docker exec` berjalan, stop selesai dalam 2 detik. |
| DNS keluar | Berjalan tanpa konfigurasi tambahan di network bridge bawaan, berbeda dari hasil WSL2. |
| Waktu start | Setara `runc`, sekitar 4 detik pada node tersebut. |

Performa dan kompatibilitas Laravel, Express, serta Next.js yang diminta ADR-014
belum diukur.

### Sandbox Railpack dan build

Sandbox Railpack tetap memakai runtime default Docker, begitu juga build. Sandbox sebenarnya kandidat bagus untuk gVisor, karena menjalankan kode repository langsung di atas kernel host. Pada daemon rootful dengan `runsc`, `railpack info` di sandbox selesai dalam 2 detik selama `/etc/resolv.conf` di-mount seperti `SAKALA_CONTAINER_DNS`. Tanpa mount itu, Railpack menggantung sampai deadline karena DNS tidak terjangkau. Perubahan ini belum dibuat dan perlu keputusan sendiri. Selama masih ada node rootless, limit memory, CPU, dan PID sandbox di node itu akan ikut hilang.

### Memasang di node rootful

Ini jalur yang dipilih. `scripts/install-gvisor.sh` di `sakala-deployment` memasang rilis
resmi beserta `gvisor-bin/`, memeriksa checksum `.sha512`, mendaftarkan `runsc` dengan
`--platform=systrap` di `/etc/docker/daemon.json` tanpa menghapus key lain, dan me-reload
daemon. Setelah itu, pada konfigurasi Agent:

```dotenv
SAKALA_CONTAINER_RUNTIME=runsc
SAKALA_CONTAINER_DNS=1.1.1.1,8.8.8.8
```

### Memasang di node rootless

Hanya untuk node yang belum pindah, karena limit per workload tidak berlaku di sini.
Perintah ini mengikuti [panduan instalasi gVisor](https://gvisor.dev/docs/user_guide/install/).
Dokumentasi gVisor tidak membahas `daemon.json` rootless. Lokasi `~/.config/docker/daemon.json`
berasal dari [dokumentasi Docker rootless](https://docs.docker.com/engine/security/rootless/troubleshoot/),
dan `--config_file` serta `--ignore-cgroups` diuji pada model node.

```bash
# Sebagai user dengan sudo. gvisor-bin/ harus berada di samping runsc.
ARCH=$(uname -m)
URL=https://storage.googleapis.com/gvisor/releases/release/latest/${ARCH}
wget ${URL}/gvisor.tar.zstd ${URL}/gvisor.tar.zstd.sha512
sha512sum -c gvisor.tar.zstd.sha512
sudo tar --zstd -xf gvisor.tar.zstd -C /usr/local/bin

# Sebagai user yang menjalankan Docker rootless.
mkdir -p ~/.config/docker
runsc install --config_file="$HOME/.config/docker/daemon.json" --runtime runsc -- \
  --platform=systrap --ignore-cgroups
systemctl --user restart docker

# Cek. Baris pertama harus "Starting gVisor...".
docker run --rm --runtime runsc alpine:3.22 dmesg | head -1
docker info --format '{{range $n, $r := .Runtimes}}{{$n}}{{range $r.Args}} {{.}}{{end}}{{println}}{{end}}'
```

`runsc install` menimpa entri runtime bernama sama dan menyimpan file lama sebagai
`daemon.json~`. Setelah itu, pada konfigurasi Agent:

```dotenv
SAKALA_CONTAINER_RUNTIME=runsc
SAKALA_CONTAINER_DNS=1.1.1.1,8.8.8.8
```

Ubuntu 24.04 ke atas membatasi user namespace lewat AppArmor, menurut dokumentasi
Docker. Docker rootless yang sudah berjalan berarti pembatasan itu sudah ditangani untuk
RootlessKit, tetapi belum bisa dipastikan apakah `runsc` butuh profil sendiri. Kalau
`docker run --runtime runsc` gagal dengan pesan izin user namespace, itulah yang perlu
diperiksa.

### Yang wajib diuji di node sungguhan

1. `dmesg` dan `docker info` di atas, pada setiap node baru. `systrap` sudah terbukti di node
   staging dengan kernel 7.0.
2. Smoke test berikut, yang men-deploy aplikasi Node sungguhan dan memeriksa health,
   log, restart, sleep, wake, serta crash. Image `caddy:2.11.3-alpine` dan
   `node:24-alpine` harus sudah ada. Tambahkan `SAKALA_SMOKE_LIMITS_ENFORCED=1` hanya
   pada Docker rootful, karena test itu membiarkan proses tumbuh sampai ada yang
   menghentikannya.

   ```bash
   SAKALA_SMOKE_CONTAINER_RUNTIME=runsc \
     cargo test -p sakala-agent --test docker_smoke_test -- --ignored --nocapture a_workload_runs_under
   ```

3. DNS keluar dengan `SAKALA_CONTAINER_DNS`. Pada model node rootless, keluar ke internet
   gagal juga di `runc` karena jaringan bersarang, jadi DNS hanya terbukti di rootful.
4. Berapa PID dan memory minimum aplikasi pilot di `systrap`, untuk meninjau default
   `SAKALA_DEFAULT_CONTAINER_PIDS_LIMIT` dan `SAKALA_DEFAULT_CONTAINER_MEMORY_MB`.
