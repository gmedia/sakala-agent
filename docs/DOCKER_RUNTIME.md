# Docker Runtime v1

`DockerRuntimeExecutor` adalah executor opt-in untuk deployment HTTP app stateless pada satu runtime node. Default binary tetap `noop`.

## Flow

```txt
validate command
-> init repository dan fetch immutable commit SHA
-> pilih Dockerfile atau Railpack
-> enforce build deadline
-> docker buildx build --load
-> buat network project bila belum ada, sambungkan container Caddy ke sana
-> bila ada release_command: jalankan satu container release, tunggu exit, hapus
-> docker run candidate pada network project
-> deployment.health.started: tunggu state Docker, lalu probe HTTP dari dalam container Caddy
-> deployment.route.started
-> tulis route Caddy secara atomik
-> validate dan reload Caddy
-> ambil startup logs
-> hapus container deployment sebelumnya
```

Deployment yang [memakai ulang image](#memakai-ulang-image) melewati tiga langkah
pertama setelah validasi: init repository, pemilihan builder, dan build. Sisanya sama.

Deployment dengan beberapa replika menjalankan langkah `docker run` dan readiness untuk
setiap replika, menulis satu route berisi semua replika, lalu menghentikan container
lama dalam satu `docker stop`. Nama replika adalah nama container biasa ditambah
`-r<indeks>`, dan labelnya menambah `dev.sakala.replica` serta `dev.sakala.replicas`.
Deployment satu replika tetap memakai nama tanpa akhiran. Lihat [Replika](AGENT_API.md#replika)
dan [Route untuk beberapa replika](CADDY_INTEGRATION.md#route-untuk-beberapa-replika).

Bila start, readiness, atau routing gagal, Agent lebih dulu mengirim baris terakhir
output aplikasi, baru menghapus candidate. Tanpa langkah itu output aplikasi hilang
bersama container, sehingga user tidak punya petunjuk untuk memperbaiki deploy-nya.

Startup agent juga menjalankan scan orphan secara detection-only. Detail guard Phase 9 ada di [Runtime hardening](RUNTIME_HARDENING.md).

Reconciliation juga mendiscover setiap container managed yang memiliki label
project/deployment valid dan menyimpan status Docker-nya dalam report lokal.
Container dengan label tidak lengkap atau status `Created`/`Exited`/`Dead`
tetap dilaporkan sebagai orphan, bukan dihapus otomatis.

Sebelum polling command, driver Docker menjalankan preflight fatal untuk Git, Docker daemon, Docker Buildx, Railpack, container Caddy, network runtime, filesystem workspace (termasuk akses direktori), disk workspace, serta direktori route. Node tidak mulai menerima command bila salah satunya gagal; log menyertakan nama check dan detail yang aman untuk operator. Check `project-networks` tidak fatal, lihat [Network per project](#network-per-project).

## Network per project

Setiap project punya bridge network sendiri bernama `sakala-project-<project-id>`.
Workload tidak lagi bergabung ke `sakala-runtime`. Di network itu hanya ada
container project tersebut, yaitu paling banyak deployment lama dan candidate saat
redeploy, ditambah container Caddy runtime. Docker memisahkan bridge network satu
sama lain, jadi aplikasi satu project tidak bisa menjangkau aplikasi project lain,
baik lewat nama maupun IP, kecuali lewat Caddy seperti pengunjung biasa.
Sebelumnya semua workload berbagi `sakala-runtime` dengan ICC default, sehingga
aplikasi satu mahasiswa bisa membuka koneksi langsung ke aplikasi mahasiswa lain.

Network diberi label `dev.sakala.managed=true`, `dev.sakala.network-kind=project`,
`dev.sakala.project-id`, dan `dev.sakala.agent-id`. ICC di dalamnya tetap default,
karena isinya hanya satu project.

Siklus hidupnya:

- **Deploy.** Sebelum `docker run`, Agent membuat network bila belum ada dan
  menjalankan `docker network connect <network> <SAKALA_CADDY_CONTAINER>`. Probe
  readiness (`docker exec <caddy> wget`) dan route tetap memakai nama container.
- **Restart, wake, `RefreshRoute`, `restore_route`.** Agent menyambungkan Caddy lagi
  sebelum probe, untuk kasus container Caddy dibuat ulang.
- **Stop, candidate gagal, `cleanup_failed_candidate`.** Setelah container dihapus,
  Agent memeriksa `docker ps --all --filter network=<network>`. Bila tidak ada
  container lain selain Caddy, Caddy diputus dan network dihapus. Container yang
  sedang sleep ikut dihitung, karena `docker start` gagal bila network-nya hilang.
  Kegagalan di langkah ini hanya dicatat di log Agent, tidak menggagalkan command.
- **Startup, lalu setiap 60 detik.** Agent menyambungkan Caddy ke setiap network
  project milik `SAKALA_AGENT_ID` ini yang belum tersambung. Berbeda dengan container
  dan image yang hanya dilaporkan, network yang tidak dipakai container mana pun
  langsung dihapus, karena network tidak menyimpan state.

Pemeriksaan berkala ini menangani container Caddy yang dibuat ulang saat Agent
berjalan, misalnya oleh `docker compose up`, pergantian pin image Caddy, atau reboot
host. Container baru itu hanya tersambung ke `sakala-runtime`, jadi route project
gagal 502 paling lama sekitar 60 detik sampai Agent menyambungkannya lagi. Operator
tidak perlu me-restart Agent. Pada smoke test dengan interval 1 detik, kedua project
kembali terjangkau 1,8 detik setelah Caddy dibuat ulang.

Saat tidak ada perubahan, pemeriksaan ini hanya `docker network ls`, inspect
container Caddy, dan satu `docker ps`. Log hanya ditulis bila ada network yang
disambungkan ulang atau dihapus. Penyambungan ulang selalu berjalan. Penghapusan
dilewati selama ada command aktif, dan juga tidak bisa berjalan bersamaan dengan
start workload: start memegang lock bersama dari pembuatan network sampai
`docker run` selesai, sedangkan penghapusan memegang lock eksklusif. Tanpa lock
itu, network yang baru disiapkan untuk candidate bisa terhapus sebelum candidate
memakainya. Pemeriksaan ini berjalan dari worker sendiri, bukan dari health worker,
karena kontrak `health_snapshot` melarang mutasi runtime.

**Kompatibilitas.** Workload dari Agent versi lama tetap berada di `sakala-runtime`
dan tetap dilayani, karena Caddy tetap tergabung di sana. Redeploy berikutnya
memindahkannya: candidate dibuat di network project, lalu container lama dihapus.
Karena itu `sakala-runtime` dan check `runtime-network` tetap dibutuhkan. Pada
`runsc`, Caddy tetap berjalan dengan runtime default dan me-resolve nama workload
lewat DNS bawaan Docker, sama seperti di `sakala-runtime`; jalur ini belum diuji
ulang dengan `runsc` sejak network per project.

### Address pool Docker

Tanpa pengaturan, Docker hanya punya sekitar 31 subnet untuk bridge network
(172.17.0.0/16 sampai 172.31.0.0/16 dan 192.168.0.0/16 yang dipecah menjadi /20),
dan sebagian sudah dipakai `docker0`, `sakala-runtime`, `sakala-railpack`, serta
network Compose lain. Node dengan lebih dari sekitar 20 project akan kehabisan, dan
deploy berikutnya gagal dengan `runtime_capacity_exceeded`.

Agent tidak memilih subnet sendiri. Atur pool di `daemon.json` Docker
(`/etc/docker/daemon.json`, atau `~/.config/docker/daemon.json` pada Docker rootless):

```json
{
  "default-address-pools": [
    { "base": "172.16.0.0/12", "size": 24 }
  ]
}
```

Rentang ini sama dengan rentang bawaan Docker, jadi tidak menambah risiko bentrok
dengan network host, dan memberi 4.096 network /24. Network yang sudah ada tetap
memakai subnetnya. Docker harus di-restart, dan tanpa `live-restore` restart itu
ikut me-restart semua container. Pengaturan ini belum diuji pada daemon Sakala;
cek setelah restart dengan `docker info --format '{{json .DefaultAddressPools}}'`.

Check preflight `project-networks` (tidak fatal) mencoba membuat lalu menghapus
network `sakala-project-preflight`, membaca `DefaultAddressPools` dari `docker info`,
dan menghitung kira-kira berapa network lagi yang bisa dibuat. Check ini memberi
peringatan bila pembuatan network gagal, atau bila sisanya lebih kecil dari
`SAKALA_MAX_ACTIVE_CONTAINERS`. Contoh pada daemon tanpa pengaturan:

```text
Docker can create only about 17 more bridge networks (31 in Docker's default address pools, 14 exist), fewer than SAKALA_MAX_ACTIVE_CONTAINERS (20). ...
```

## Image dan GC

Image ditag berdasarkan project, commit, dan deployment. Image dan container diberi ownership labels agar discovery dan cleanup tidak menyentuh workload di luar Sakala:

- `dev.sakala.managed=true`
- `dev.sakala.project-id=<uuid>`
- `dev.sakala.deployment-id=<uuid>`
- `dev.sakala.workload-kind=web`
- `dev.sakala.agent-id=<agent-id>`

Label `project-id` dan `deployment-id` juga dipasang pada image Dockerfile dan
Railpack (untuk Railpack lewat langkah terpisah, lihat
[Label image Railpack](#label-image-railpack)). Setiap image deployment punya tag sendiri, jadi image lama tidak pernah
menjadi dangling, dan `docker image prune` tanpa `-a` tidak menghapus apa pun. Karena
itu GC memilih per project. Project yang masih punya container mempertahankan image
yang dipakai container mana pun (berjalan, tidur, atau berhenti, dan semua replika) dan
image previous untuk rollback, berapa pun umurnya, dan tidak ada yang lain. Image
previous adalah image bertag `sakala/project-<project>:previous`; bila belum ada image
dengan tag itu (image dari Agent yang belum mengenal tag ini), aturan lama berlaku: image terbaru yang
lebih tua dari image aktif terbaru. Project tanpa container sama sekali, misalnya project
yang sudah dihapus, hanya mempertahankan image yang lebih muda dari
`SAKALA_ORPHANED_IMAGE_GRACE_SECONDS` (default 24 jam). Sisanya dihapus dengan
`docker image rm <nama>...` tanpa `--force`, tag `previous` lebih dulu. Hanya image
berlabel `dev.sakala.managed=true` dengan project yang valid, dan yang semua namanya
`sakala/project-<project>:<tag>`, yang pernah dipilih.

Pengecualiannya image tanpa label `dev.sakala.project-id` sama sekali yang semua namanya
`sakala/project-<project>:<tag>` untuk satu project: GC menganggapnya milik project itu
dan menerapkan aturan yang sama. Image Railpack dari Agent sampai v0.16.0 tidak punya
label apa pun, dan di staging sekitar 37 image seperti itu, termasuk milik project yang
sudah dihapus, menumpuk karena tidak pernah terpilih. GC membaca nama image dari
`docker image ls` tanpa filter, karena filter `reference` hanya menampilkan nama yang
cocok dan menyembunyikan nama lain image yang sama; image yang punya satu saja nama di
luar repository project itu tidak pernah dipilih. Image tanpa label tidak pernah diberi
tag `previous` dan tidak pernah dipakai ulang, karena keduanya tetap mensyaratkan label.
Container yang memakai image itu, berjalan maupun berhenti, tetap melindunginya.

Label image dibaca dari `{{json .Config}}` dalam satu `docker image inspect` untuk semua
kandidat. `{{index .Config.Labels "..."}}` maupun `{{json .Config.Labels}}` tidak dipakai:
bila satu perintah menyebut beberapa image, Docker 29.10 menggagalkan seluruh perintah
dengan `map has no entry for key "Labels"` untuk image yang tidak punya label sama sekali,
dan satu image Railpack lama saja sudah cukup untuk menggagalkan setiap putaran GC.

Image yang punya dua tag (tag asli dan `previous`) dihapus lewat kedua namanya. Docker
dengan containerd image store menolak `docker image rm <id>` untuk image seperti itu
(`image is referenced in multiple repositories`, juga bila kedua tag di repository yang
sama), sedangkan menghapus per nama melepas tag satu per satu dan menghapus image saat
nama terakhirnya hilang. Bila sebuah container mulai memakai image itu di tengah jalan,
Docker hanya menolak nama terakhir, dan karena `previous` dilepas lebih dulu image itu
tetap memegang nama aslinya, nama yang dicari reuse.

GC ini berjalan sendiri setiap `SAKALA_IMAGE_GC_INTERVAL_SECONDS` (default satu jam),
dan dilewati selama ada command aktif. Command `CleanupRuntime` dengan `approved: true`
menjalankan aturan yang sama sekali jalan, dan startup reconciliation hanya melaporkan
kandidatnya. Karena umur tidak lagi melindungi image baru, GC hanya menghapus image
sebuah project sambil memegang lock project yang sama dengan command: project yang
sedang di-deploy dilewati sampai putaran berikutnya, dan image project yang lock-nya
didapat dipilih ulang dulu. Deployment yang baru selesai build tetapi belum menyalakan
container-nya karena itu tidak pernah kehilangan image-nya.

`StopProject` dengan `remove_images: true`, yang dikirim API saat project dihapus,
langsung menghapus semua image project itu setelah container-nya hilang. Aturan
lengkapnya ada di [Sakala Agent API](AGENT_API.md#approved-runtime-cleanup).

Versi sebelumnya membaca label dengan `{{.Label "..."}}` pada `docker image ls`, yang
ditolak Docker (`can't evaluate field Label in type *formatter.imageContext`), sehingga
deteksi gagal dan tidak ada image yang pernah dihapus. Label kini dibaca lewat
`docker image inspect`.

### Label image Railpack

Frontend BuildKit Railpack mengabaikan `--label`: menurut source v0.23.0 (versi yang
dipin) dan `main` per 12 November 2026, frontend itu hanya membaca `build-arg:*`,
`platform`, dan `filename`, lalu menyerahkan config image buatannya sendiri ke BuildKit.
Di staging, `docker image inspect -f '{{json .Config.Labels}}'` pada setiap image
Railpack menghasilkan `null`, sehingga reuse dan rollback selalu jatuh ke build dengan
`image_not_owned`, tag `previous` tidak pernah dibuat, dan image GC tidak pernah
menyentuh image itu.

Karena itu build Railpack kini diberi nama sementara `<image>-unlabelled`, lalu Agent
menulis label di langkah `railpack-label`:

```bash
docker container create --pull never --network none --name sakala-label-<uuid> <image>-unlabelled
docker container commit --change 'LABEL dev.sakala.managed="true"' ... sakala-label-<uuid> <image>
docker container rm --force --volumes sakala-label-<uuid>
docker image rm <image>-unlabelled
```

Container itu tidak pernah dijalankan dan tidak membawa label Sakala, jadi rekonsiliasi,
pencarian replika, dan metrik tidak pernah melihatnya. Commit container yang tidak
berubah tidak menambah layer, dan Docker menyusun config baru dari config image, jadi
CMD, ENTRYPOINT, ENV, WORKDIR, USER, EXPOSE, dan VOLUME tetap seperti tulisan Railpack.
Nilai label dikutip dengan `\`, `"`, dan `$` di-escape, karena Docker membaca
`--change` sebagai baris Dockerfile dan mengekspansi variabel; nilai berisi karakter
kontrol ditolak. Langkah ini hanya memakai daemon, sehingga tidak bergantung pada
builder buildx. Build `FROM <image lokal>` dengan `--label` sengaja tidak dipakai: dengan
builder `docker-container` image lokal tidak terlihat, dan BuildKit lalu menarik
`docker.io/sakala/project-...` dari registry. Image tanpa label tetap disimpan Docker
sebagai parent tanpa nama dari image berlabel dan ikut terhapus bersamanya.

Bila langkah ini gagal, build gagal dengan `runtime_build_failed`
(`railpack-label failed with exit code N`), stderr Docker masuk log deployment, dan
container serta nama sementara tetap dihapus. Bila timeout build memotong langkah ini,
container tetap dihapus (container yang tertinggal akan menahan image itu selamanya,
karena GC menyimpan setiap image yang dipakai container), dan nama sementara yang
tersisa dibersihkan image GC. Image tanpa label tidak pernah dijalankan,
karena reuse, rollback, dan image GC akan diam-diam tidak berfungsi untuk image itu.
Smoke test `railpack_labels_smoke_test` membuktikan jalur ini dengan build Railpack
sungguhan: label lengkap, config sama persis selain label, image pertama menjadi
`previous` setelah deployment kedua, dan rollback ke image itu berjalan tanpa build.

### Image sebelumnya dan rollback

Setelah `DeployProject` sukses, masih di bawah lock project, Agent:

1. membaca container project itu dan image-nya. Workload yang digantikan adalah container
   lain yang berjalan, atau, bila tidak ada yang berjalan (project tidur), container lain
   yang ada; container terbarunya yang menentukan;
2. memberi image workload itu tag `sakala/project-<project>:previous`, bila image-nya
   berbeda dari image yang sekarang melayani dan semua namanya ada di repository project
   itu. Ini terjadi setelah route pindah dan sebelum container lama dihapus, jadi bila
   Agent mati di antaranya, tag-nya sudah benar dan container lama dibersihkan sebagai
   orphan;
3. menghapus container lama seperti sebelumnya;
4. menjalankan aturan GC di atas untuk project itu saja, sehingga yang tersisa hanya image
   live dan image previous.

Langkah 2 dan 4 idempoten: tag yang sudah ada tidak ditulis ulang, dan image yang sudah
hilang hanya membuat `docker image rm` gagal tanpa efek. Kegagalan di langkah 2 atau 4
tidak menggagalkan deployment; Agent mengirim `deployment.images.retained` dan image GC
berkala menyelesaikannya. Bila tag tidak bisa ditulis, langkah 4 dilewati, karena image
yang baru digantikan akan terhapus. Kontraknya ada di
[Rollback dan image sebelumnya](AGENT_API.md#rollback-dan-image-sebelumnya).

Rollback adalah deployment yang memakai ulang image deployment lama lewat
`reuse_image_from_deployment_id`. Smoke test `image_rollback_smoke_test` membuktikan
urutannya pada Docker sungguhan: setelah deploy A, B, C hanya C (live) dan B (previous,
dua tag) yang tersisa; rollback ke B memakai image B tanpa checkout dan build
(`deployment.image.reused`), lalu B live dan C previous; image GC berkala sesudahnya
tidak menghapus apa pun; rollback ke A, yang image-nya sudah dihapus, build ulang dengan
`image_missing`.

### Dampak disk

Per project yang masih punya container, node kini menyimpan paling banyak dua image
deployment (live dan previous), ditambah image yang dipakai container lain bila ada
(misalnya selama deployment bergulir). Sebelumnya node menyimpan image previous ditambah
semua image yang lebih muda dari tujuh hari, jadi project yang di-deploy 20 kali seminggu
bisa memegang sekitar 21 image.

Ukuran yang terukur lokal (Docker 29.10, containerd image store, 2026-11-23) dari contoh
gmedia/sakala-examples:

| Image | Ukuran di disk | Ukuran konten (terkompresi) | Unik per deployment setelah perubahan kode satu baris |
| --- | --- | --- | --- |
| `node-express`, Railpack v0.23.0 (Node) | 526 MB | 125 MB | sekitar 94 kB |
| `laravel`, Dockerfile FrankenPHP | 349 MB | 84,5 MB | sekitar 1,9 MB |

Layer dipakai bersama antar-image: dua deployment yang hanya berbeda kode berbagi base
image dan layer dependency, jadi image tambahan hanya memakan layer aplikasinya. Untuk
deployment yang hanya mengubah kode, aturan lama dan baru hampir sama besar (puluhan kB
sampai beberapa MB per image). Bedanya terasa saat dependency atau base image berubah:
setiap image lalu membawa layer sebesar `node_modules` atau `vendor`-nya, puluhan sampai
ratusan MB. Dalam kasus terburuk, setiap image unik penuh, aturan lama bisa memegang
sekitar 21 x 350 sampai 530 MB, yaitu 7 sampai 11 GB per project, sedangkan aturan baru
paling banyak 2 x 350 sampai 530 MB, yaitu 0,7 sampai 1,1 GB. `reclaimed_image_bytes`
tetap batas atas, karena menjumlahkan ukuran penuh image termasuk layer bersama.

## Readiness

Container `running` belum berarti aplikasinya melayani request. Karena itu
readiness punya dua tahap, yang diulang sampai `start_timeout_seconds` habis:

1. **State Docker.** `restarting`, `exited`, `dead`, atau restart count yang naik
   berarti aplikasi crash, dan penantian langsung berhenti. `HEALTHCHECK` image
   ikut dihormati: `unhealthy` langsung gagal, `starting` terus ditunggu.
2. **Probe HTTP** ke `http://<container>:<port>/`. Status di bawah 500 berarti
   sehat, termasuk 404 atau halaman login, karena tujuannya memastikan aplikasi
   menjawab, bukan menilai halamannya. Status 5xx terus dicoba sampai deadline,
   karena aplikasi bisa membalas 503 selama boot.

Agent menjalankan probe dengan `docker exec <caddy_container> wget`, bukan dari
host. Binary host tidak bisa menjangkau IP container pada rootless Docker, dan
jalur ini sama dengan jalur yang nanti dipakai route.

**Container Caddy runtime wajib menyediakan `wget`.** Image `caddy:*-alpine`
menyertakan BusyBox `wget`. Kalau tidak ada, deployment gagal dengan
`runtime_dependency_failed`, dan preflight mencatat warning non-fatal
`caddy-http-probe` saat startup. Preflight sengaja tidak fatal supaya Agent tetap
bisa start lebih dulu daripada Caddy.

Perilaku BusyBox `wget` v1.37.0 yang menentukan desain ini, diverifikasi pada
`caddy:2.11.3-alpine`:

- Exit code-nya 1 untuk 4xx, 5xx, koneksi ditolak, DNS gagal, dan redirect yang
  terlalu panjang. Hanya stderr yang membedakannya, jadi Agent membaca baris status
  pertama dari `--server-response`.
- Redirect selalu diikuti dan tidak ada flag untuk mematikannya. Tanpa pencegahan,
  aplikasi user bisa membuat container Caddy mengambil alamat apa pun lewat header
  `Location`. Agent karena itu memakai mode proxy (`-Y on` dengan `http_proxy` yang
  menunjuk aplikasi itu sendiri), sehingga semua request dalam rantai redirect tetap
  masuk ke koneksi aplikasi. Penilaian hanya memakai jawaban pertama.
- Konsekuensi mode proxy: request line berbentuk absolut (`GET http://<container>:<port>/
  HTTP/1.1`). Server HTTP wajib menerimanya menurut RFC 9112, dan Node maupun Caddy
  terbukti menerimanya. Aplikasi yang merutekan dari `req.url` mentah bisa membalas
  404, yang tetap dihitung sehat.

### Host yang ditolak framework

Status di bawah 500 dihitung sehat, jadi aplikasi yang menolak domain project dulu
lolos readiness lalu menjawab 403 ke setiap pengunjung. Itu terjadi di production
2026-11-19 dengan `vite preview`: Vite 6 ke atas menolak `Host` di luar
`preview.allowedHosts`. Probe `wget` mengirim nama container sebagai `Host`, yang juga
ditolak Vite, sehingga status 403 saja tidak membedakan host yang ditolak dari halaman
login.

Karena itu, saat deploy dan hanya bila probe mendapat 400 atau 403, Agent mengirim satu
request lagi lewat container Caddy dengan `Host` yang sama dengan pengunjung, yaitu
`domain` deployment:

```txt
docker exec sakala-caddy sh -c '<printf request> | nc -w <detik> "$2" "$3" | head -c "$5"' \
  sh <domain> <container> <port> <detik> 4096
```

BusyBox `wget` tidak mencetak body untuk 4xx, jadi request ini memakai BusyBox `nc` dari
image yang sama. Nilainya masuk sebagai parameter posisi, bukan bagian script, dan
koneksinya langsung ke aplikasi tanpa mengikuti redirect. Hanya 4096 byte pertama yang
dibaca. Deployment gagal dengan `runtime_app_host_blocked` hanya bila jawabannya persis
salah satu ini:

| Framework | Status | Isi yang dicocokkan |
| --- | --- | --- |
| Vite preview (`vite preview`) | 403 | `Blocked request. This host (`, `is not allowed`, dan `preview.allowedHosts` |
| Vite dev server (`vite`) | 403 | Teks yang sama dengan `server.allowedHosts` |
| Django dengan `DEBUG = True` | 400 | `Invalid HTTP_HOST header` dan `ALLOWED_HOSTS` |

Teks Vite diambil dari Vite 8.3.5 (`hostCheck.ts`) dan dicocokkan dengan jawaban asli
yang direkam lewat `nc`. Vite selalu mengizinkan `localhost` dan subdomain `.localhost`,
jadi di instalasi lokal dengan domain `*.run.sakala.localhost` masalah ini tidak muncul
dan cek ini tidak pernah gagal; ia baru terlihat dengan domain sungguhan seperti
`*.sakala.app`. Django dengan `DEBUG = False` menjawab `Bad Request (400)` tanpa
menyebut penyebabnya, jadi tidak dikenali dan tetap lolos seperti sebelumnya. Next.js
tidak dimasukkan karena tidak memeriksa `Host` pada request biasa (`allowedDevOrigins`
hanya untuk resource dev lintas origin), dan webpack-dev-server (`Invalid Host header`)
belum dimasukkan karena teksnya belum diverifikasi pada versi yang dipakai.

Jawaban lain, termasuk 403 dari halaman login, 403 untuk nama container saja (Vite dengan
`allowedHosts` yang benar), atau request yang tidak bisa dijalankan karena image router
tidak punya `nc`, tidak mengubah hasil readiness. Gagal dipilih, bukan berhasil dengan
peringatan, karena setiap pengunjung akan mendapat penolakan itu, dan dengan gagal
deployment sebelumnya tetap melayani. `RestartProject`, `WakeProject`, `RefreshRoute`,
`restore_route`, dan `HealthCheck` tidak menjalankan cek ini, karena image-nya sudah lolos
saat deploy.

Probe tidak menilai aplikasi saat ia tidak bisa berjalan. Kegagalan seperti `wget`
hilang, Caddy mati, atau nama container tidak ter-resolve dilaporkan sebagai
`runtime_dependency_failed`.

Di bawah `--restart unless-stopped`, crash terlihat sebagai `restarting`, dan Docker
mengosongkan exit code serta flag OOM begitu restart berhasil. Karena itu, untuk
crash yang tertangkap di antara dua restart, Agent membaca `docker events` sebagai
sumber exit code dan event `oom`. Bila riwayat itu pun kosong, pesan menyatakan exit
code tidak lagi dilaporkan Docker.

Daftar `error_code` dan pesannya ada di
[Sakala Agent API](AGENT_API.md#kegagalan-yang-disebabkan-aplikasi).

Command `RestartProject`, `WakeProject`, `RefreshRoute`, dan aksi `restore_route`
memakai probe yang sama dengan deadline `SAKALA_START_TIMEOUT_SECONDS`, karena
payload-nya tidak membawa deadline sendiri. `HealthCheck` juga memakai probe yang
sama, tetapi hanya menunggu 10 detik, karena ia bertanya tentang keadaan sekarang,
dan melaporkan hasilnya pada field `ready`, `reason`, dan `error_code`.

## Konfigurasi build dari project

Bagian ini boleh dipakai untuk dokumentasi user.

**Root directory.** Sakala membangun dari folder ini, bukan dari root repository. Harus
path relatif di dalam repository. Pada project dengan Dockerfile, `Dockerfile` dicari di
folder itu.

**Monorepo JavaScript.** Bila folder itu adalah salah satu package workspace pnpm, npm,
Yarn, atau Bun yang root-nya ada di atasnya, build Railpack berjalan dari root workspace,
karena lockfile, `pnpm-workspace.yaml`, dan `packageManager` ada di sana. Contohnya
root directory `apps/web` di repository yang punya `pnpm-workspace.yaml` dengan
`packages: [apps/*]` di root-nya. Detailnya di [Monorepo JavaScript](#monorepo-javascript).

**Build command.** Hanya berlaku untuk project tanpa Dockerfile. Pada project dengan
Dockerfile, langkah build ditentukan oleh Dockerfile itu sendiri, jadi perintah ini
diabaikan dan log build menyebutkannya.

**Start command.** Berlaku untuk keduanya. Pada project dengan Dockerfile, perintah ini
menggantikan `ENTRYPOINT` dan `CMD` image, dan dijalankan dengan `sh -c` di dalam
container, jadi image-nya harus punya `sh`. Image `scratch` dan distroless gagal start.

Semua perintah ini hanya berjalan di dalam build atau container, tidak pernah di shell
host. Agent tidak memakai `sh -c` di host; argumen dikirim terstruktur ke Docker.

## Monorepo JavaScript

Di workspace pnpm, npm, Yarn, atau Bun, lockfile dan package manager hanya ada di root
workspace. Railpack yang dijalankan di folder package saja memilih npm dan tidak punya
lockfile, sehingga build gagal dengan `pnpm: not found` (gmedia/sakala-agent#74).
Railpack sendiri mendukung workspace, tetapi hanya bila build berjalan dari root
workspace, dan di sana ia membangun serta menjalankan package root
([dokumentasi Railpack](https://railpack.com/languages/node#monorepos-and-workspaces)).
Karena itu Agent membangun dari root workspace dan mengarahkan perintahnya ke package.

**Deteksi.** Hanya pada build Railpack (termasuk `auto` tanpa Dockerfile di root
directory) dan hanya bila `root_directory` diisi. Agent mencari direktori terdekat di
atas root directory, sampai root repository, yang punya `package.json` dan daftar
workspace: `packages` di `pnpm-workspace.yaml`, atau `workspaces` di `package.json`
(array atau `{ "packages": [...] }`), dengan urutan yang sama seperti Railpack. Root
directory dianggap package workspace bila ia punya `package.json` dan cocok dengan salah
satu pola (glob seperti pnpm dan npm: `*` satu level, `**` berapa pun level, pola
berawalan `!` mengecualikan). Workspace terdekat yang memutuskan; bila ia tidak memuat
root directory, build berjalan seperti sebelumnya. Lockfile tidak dibaca untuk
keanggotaan, karena pnpm dan npm hanya menulis package workspace ke lockfile bila daftar
workspace di atas ada, dan Yarn 1 tidak menulisnya sama sekali.

Package manager dipilih dengan urutan Railpack, supaya perintahnya memakai tool yang
dipasang Railpack: `packageManager` di `package.json` root, lalu `pnpm-lock.yaml`,
`bun.lockb` atau `bun.lock`, `.yarnrc.yml` atau `.yarnrc.yaml`, `yarn.lock`, lalu
`engines`, dan npm bila tidak ada.

**Perintah.** Konteks build dan direktori `railpack prepare` menjadi root workspace.
Railpack memasang dependency seluruh workspace dengan lockfile-nya, lalu:

| Situasi | Build | Start |
| --- | --- | --- |
| Project mengisi `build_command` / `start_command` | `cd '<package>' && <perintah>` | `cd '<package>' && <perintah>` |
| pnpm, package punya script `build` | `pnpm --filter '<name>...' run build`, jadi package workspace yang dipakainya ikut dibangun lebih dulu | |
| npm, Yarn, Bun, atau package tanpa `name` | `cd '<package>' && <pm> run build` | |
| Package punya script `start` | | `cd '<package>' && <pm> run start` |
| Tanpa script `start`, package adalah SPA Vite | | Railpack menyajikan `<package>/<outDir>` dengan Caddy-nya sendiri lewat `RAILPACK_SPA_OUTPUT_DIR` |

Aturan SPA Vite sama dengan Railpack: `vite` ada di `dependencies` atau
`devDependencies`, ada script `build`, dan script itu memuat `vite build` atau ada
`vite.config.js`/`vite.config.ts`; SvelteKit tidak termasuk. `outDir` dibaca dari config
seperti Railpack membacanya, dengan default `dist`, dan nilai yang keluar dari package
diabaikan. Framework SPA lain yang dikenali Railpack (Astro statis, CRA, Angular, React
Router) belum diturunkan; package seperti itu butuh script `start` atau `start_command`.

Perintah dari project tetap menang atas variabel project bernama sama. Perintah yang
diturunkan Agent hanya dipakai bila project tidak punya variabel `RAILPACK_BUILD_CMD`,
`RAILPACK_START_CMD`, atau `RAILPACK_SPA_OUTPUT_DIR`, jadi variabel itu tetap cara
menggantinya, dan nilainya berjalan di root workspace apa adanya. `release_command` juga
dijalankan dengan `cd '<package>' && ...`, karena working directory image adalah root
workspace. Nama package dan path di-quote untuk shell, karena Railpack menjalankan
perintah dengan `bash -c`.

Deployment seperti ini mengirim event `deployment.workspace.detected` setelah
`deployment.analysis.started`, dengan metadata yang sama bentuknya dengan field
`workspace` di hasil `InspectProject`. `InspectProject` dengan `root_directory` di
package workspace menjalankan `railpack info` di root workspace dengan perintah turunan
yang sama, sehingga preview menunjukkan rencana build yang sebenarnya.

Build Dockerfile tidak berubah: `Dockerfile` di root directory tetap dibangun dengan
root directory sebagai konteksnya. Repository tanpa workspace juga tidak berubah sama
sekali.

**Batas keamanan deteksi.** Deteksi membaca file repository di host, jadi symlink hanya
diikuti selama tetap di dalam checkout, setiap file dibatasi 1 MiB, dan
`pnpm-workspace.yaml` dibaca dari event parser YAML dengan alias ditolak, supaya file
kecil tidak bisa meminta memori besar lewat ekspansi alias. File yang tidak terbaca
berarti "bukan workspace", jadi deteksi hanya mengubah build bila buktinya jelas.

## Release command

Release command adalah perintah satu kali per deployment untuk pekerjaan yang harus
selesai sebelum versi baru menerima request, terutama migrasi database. Agent
menjalankannya dari image hasil build, setelah build dan sebelum replika pertama.

**Migrasi harus tetap kompatibel dengan versi sebelumnya.** Versi lama tetap melayani
request selama release command berjalan dan sampai route pindah ke versi baru. Bila
deploy gagal setelah release command, misalnya karena replika baru tidak pernah ready,
versi lama juga tetap melayani dengan skema yang sudah berubah. Jadi tambahkan kolom atau
tabel dulu, dan hapus atau ganti nama yang lama di deploy berikutnya.

Container release dijalankan dengan:

- image deployment baru, `--entrypoint sh` dan `-c <release_command>`, sama seperti
  `start_command` pada Dockerfile, jadi image harus punya `sh`;
- env file yang sama dengan replika, termasuk secret dan `PORT`;
- network project, jadi container ini menjangkau database luar dengan cara yang sama
  seperti aplikasinya, dan tidak menjangkau project lain;
- `--memory`, `--memory-swap`, `--cpus`, `--pids-limit`, `--runtime` (misalnya `runsc`),
  mount `resolv.conf` dari `SAKALA_CONTAINER_DNS`, `--security-opt no-new-privileges:true`,
  `--cap-drop ALL`, dan [allowlist capability](#capability-workload) yang sama dengan replika;
- tanpa `--restart`, tanpa port yang dipublikasikan, dan tanpa route;
- nama `sakala-release-<project8>-<deployment-id>` dan label `dev.sakala.role=release`,
  `dev.sakala.project-id`, `dev.sakala.deployment-id`, `dev.sakala.command-id`, serta
  `dev.sakala.agent-id`. Label `dev.sakala.managed=true` sengaja tidak dipasang, jadi
  listing workload, health check, laporan crash, pemulihan replika, dan deteksi orphan
  tidak pernah melihat container ini.

Agent mengikuti output-nya dengan `docker logs --follow` ke log deployment berawalan
`[release]`, lalu membaca exit code dengan `docker wait`. Exit code 0 melanjutkan deploy.
Exit code lain menggagalkan deploy dengan `release_command_failed`. Bila `docker run`
sendiri keluar dengan 126 atau 127, artinya Docker tidak bisa menjalankan `sh` di image,
hasilnya juga `release_command_failed`; kegagalan Docker lain, misalnya runtime yang
tidak terdaftar, tetap `runtime_container_failed`. Melewati
`SAKALA_RELEASE_COMMAND_TIMEOUT_SECONDS` menggagalkan deploy dengan
`release_command_timeout`.

Container release dihapus dengan `docker rm --force --volumes` di setiap jalur: setelah
exit, setelah timeout, setelah `docker run` gagal (container bisa tertinggal dalam status
`Created`), dan saat deploy dibatalkan atau Agent shutdown. Bila task command di-abort
setelah `SAKALA_SHUTDOWN_GRACE_SECONDS`, penghapusan berjalan di background. Container
release yang masih tertinggal milik Agent ini, misalnya setelah Agent mati mendadak,
dihapus saat Agent start berikutnya. Setelah kegagalan, image candidate dan network
project yang tidak dipakai lagi dibersihkan seperti kegagalan deploy lainnya.

Kapasitas: container release diterima lewat pengecekan yang sama dengan replika, di
bawah lock admission yang sama, dan meminta jumlah replika deployment itu. Node yang
tidak muat untuk versi baru menolak sebelum release command mengubah apa pun. Selama
berjalan, container release adalah satu container tambahan project itu, seperti replika
lama selama deploy bergulir: tidak dihitung untuk project-nya sendiri, tetapi dihitung
satu slot oleh project lain. Lock admission dilepas begitu container release berjalan,
jadi deploy dan wake project lain tidak menunggu migrasi selesai.

## Memakai ulang image

Redeploy yang hanya mengubah konfigurasi, misalnya variabel, rotasi password database,
atau attach database, memakai commit yang sama dengan deployment aktif. Image-nya pun
sama, jadi build ulang hanya membuang waktu: di staging, rotasi password database
membuat aplikasi 503 selama 6 menit 56 detik, 6 menit 17 detik di antaranya build. Bila
payload membawa `reuse_image_from_deployment_id`, Agent memakai image deployment itu.
Kontraknya ada di [Memakai ulang image](AGENT_API.md#memakai-ulang-image-tanpa-build).

Setiap image yang dibangun Agent kini membawa label berikut, untuk Dockerfile maupun
Railpack:

| Label | Isi |
| --- | --- |
| `dev.sakala.managed` | `true`, seperti sebelumnya. |
| `dev.sakala.project-id` | Project yang membangunnya, seperti sebelumnya. |
| `dev.sakala.deployment-id` | Deployment yang membangunnya, seperti sebelumnya. |
| `dev.sakala.builder` | `dockerfile` atau `railpack`. Baru. |
| `dev.sakala.package-directory` | Direktori package bila image dibangun dari root workspace JavaScript, misalnya `apps/web`. Baru, dan tidak ada untuk image lain. |

Sebelum checkout, Agent menghitung referensi image dari project yang meminta,
`sakala/project-<project_id>:<commit12>-<source8>`, lalu membaca labelnya dengan
`docker image inspect --format '{{json .Config.Labels}}' <referensi>`. Image dipakai
ulang hanya bila `dev.sakala.project-id` dan `dev.sakala.deployment-id` menyebut project
dan deployment sumber itu, dan `dev.sakala.builder` dikenal. Nama image saja tidak
membuktikan apa pun, karena pemegang Docker socket bisa men-tag image project lain di
bawah nama itu; smoke test `an_image_tagged_into_another_projects_repository_is_never_run`
membuktikan image seperti itu ditolak dan project-nya build image sendiri.

Jalur reuse:

- tidak ada checkout, tidak ada pemeriksaan ruang disk workspace
  (`SAKALA_MIN_WORKSPACE_FREE_MB`), dan tidak mengantre di
  `SAKALA_MAX_CONCURRENT_BUILDS`, karena tidak ada yang dibangun;
- Agent tetap membuat direktori workspace kosong per command, hanya untuk env file
  `runtime.env` milik container release dan replika, dan menghapusnya seperti checkout;
- label builder dan package directory mengembalikan hasil build aslinya, jadi image
  Dockerfile menerima `start_command` saat dijalankan, image Railpack tidak (start
  command-nya sudah ada di image), dan release command image workspace berjalan dengan
  `cd '<package>' && ...`;
- release command, pemeriksaan kapasitas, readiness, replika, perpindahan route,
  penghapusan container lama, dan log follower berjalan persis seperti setelah build.

Bila image tidak ada, labelnya tidak cocok, atau label builder-nya belum ada (image dari
Agent sebelum fitur ini), Agent mengirim `deployment.image.reuse_skipped` dengan alasannya
lalu build seperti biasa. Image yang gagal di-inspect Docker, misalnya karena daemon
lambat menjawab dalam 10 detik, juga dianggap tidak ada (`image_missing`): build selalu
jawaban yang benar, hanya lebih lambat.

Deployment reuse yang gagal atau dibatalkan sebelum route pindah menghapus container
kandidatnya, tetapi tidak pernah menghapus image-nya. Image itu milik deployment yang
masih melayani; deployment yang build sendiri tetap menghapus image kandidatnya seperti
sebelumnya.

Tidak ada retensi image tambahan. Image GC tidak berubah: image yang dipakai ulang
dipakai container deployment baru, jadi otomatis dipertahankan seperti image aktif
lainnya. Bila image sudah dibersihkan, misalnya karena project lama tidak punya
container, redeploy berikutnya build ulang dengan `image_missing`. Retensi untuk rollback
adalah topik terpisah.

Smoke test `image_reuse_smoke_test` membuktikan ketiga jalur di Docker, BuildKit, dan
Caddy asli. Pada laptop dengan Docker 29.9.3 dan fixture Dockerfile `busybox` yang
sangat kecil, deploy dengan build butuh 4,0 sampai 5,3 detik dan deploy reuse 3,6 sampai
3,8 detik (route pindah setelah 2,0 sampai 2,4 detik) pada tiga run. Selisihnya kecil karena build fixture ini
hanya beberapa detik; pada aplikasi nyata yang build-nya beberapa menit, seluruh waktu
build itu hilang dari redeploy konfigurasi.

## Isolasi antar-project saat build

Semua project pada satu node memakai BuildKit yang sama, jadi tanpa pembatasan satu project bisa memakai hasil project lain. Ketiga jalur ini terbukti nyata pada BuildKit asli:

| Jalur | Bukti tanpa isolasi | Isolasi | Bukti dengan isolasi |
| --- | --- | --- | --- |
| Layer ter-cache | Dockerfile yang sama dengan secret berbeda menghasilkan image berisi secret project pertama. BuildKit tidak menghitung nilai secret dalam cache key. | `--no-cache` | Tiap image berisi secret-nya sendiri. |
| Cache mount | `RUN --mount=type=cache,id=x` pada project kedua membaca isi project pertama. | `BUILDKIT_CACHE_MOUNT_NS=<project>`, ditambah `--no-cache` yang membuat cache mount terlihat kosong selama build itu | Cache mount kosong di tiap build, dan `ARG BUILDKIT_CACHE_MOUNT_NS` di Dockerfile tidak mengubahnya. |
| Image lokal | `FROM` atau `COPY --from` image project lain di node ini berhasil dan bisa membaca isinya. | `--pull` | Gagal resolve, karena image itu hanya ada di node ini. |

Baris `# syntax=` bisa memilih frontend yang mengabaikan semua flag di atas, jadi Agent membangun salinan Dockerfile dengan komentar biasa di baris pertama. BuildKit berhenti mencari directive pada komentar pertama, sehingga `# syntax=` di repository tidak berpengaruh dan frontend bawaan yang dipakai. Directive `# escape=` tetap dipertahankan. Frontend eksternal yang dipin lewat `BUILDKIT_SYNTAX` sudah dicoba dan ditolak, karena di bawahnya `--pull` tidak berlaku dan `FROM` tetap bisa membaca image lokal project lain.

Biayanya terukur pada `heroku/node-js-getting-started`, dengan builder host network:

| Build Dockerfile dengan `npm ci` | Cache biasa | Dengan isolasi |
| --- | --- | --- |
| Tanpa perubahan | 2,5 detik | 15,4 detik |
| Kode berubah | 2,2 detik | 14,2 detik |

Build dengan isolasi selalu memasang ulang dependensi, dan cache mount tidak membantu karena `--no-cache` membuatnya terlihat kosong. Per-project builder akan mempertahankan cache, tetapi butuh satu container BuildKit privileged per project, jadi belum dipilih.

Railpack berbeda, karena frontend Railpack v0.23.0 mengabaikan `--no-cache` dan `--pull`. Build pertama butuh 64,4 detik, dan build ulang setelah kode berubah butuh 4,2 detik dengan flag isolasi maupun 4,4 detik tanpa flag. Jadi flag itu tidak mengisolasi apa pun di jalur ini, dan isolasinya datang dari tiga hal lain:

- `secrets-hash`, sidik jari ber-key dari nilai secret, membuat langkah yang memakai secret tidak berbagi layer. Smoke test `railpack_projects_keep_their_own_secrets` membuktikan tiap image memegang secret-nya sendiri.
- `cache-key=<project>` memisahkan cache mount. Ini baru dibaca dari source frontend dan belum diuji terpisah.
- Plan yang menyebut image `sakala/project-*` ditolak, karena `--pull` tidak berlaku.

Langkah dengan konteks, plan, dan sidik jari secret yang identik tetap berbagi layer antar-project. Isinya berasal dari input yang sama, jadi project lain hanya bisa memakainya bila sudah memiliki source yang sama.

## Pembatalan

Saat API meminta batal, Agent membatalkan token command itu. Melepas future process
mematikan process group-nya, termasuk klien `docker buildx`, dan BuildKit menghentikan
langkah build yang sedang berjalan saat kliennya hilang. Candidate, image, dan workspace
lalu dihapus, kecuali image yang [dipakai ulang](#memakai-ulang-image). Setelah aktivasi route dimulai, pembatalan tidak lagi menghentikan
deployment, karena route lama sudah mulai digantikan.

## Variabel saat build

Semua variabel project, kecuali `PORT` dan yang kosong, tersedia saat build sebagai
secret BuildKit. Nilainya tidak masuk layer atau history image, kecuali langkah
build sendiri menulisnya ke sana. Teks berikut boleh dipakai untuk dokumentasi user.

**Tanpa Dockerfile (Railpack).** Semua variabel otomatis tersedia sebagai environment
variable pada setiap langkah build. `VITE_API_URL` atau `NEXT_PUBLIC_API_URL` bekerja
tanpa pengaturan tambahan. Mengubah nilainya membangun ulang langkah yang memakainya.

**Dengan Dockerfile.** `ARG` tidak diisi, karena nilainya akan tersimpan di history
image dan bisa dibaca siapa pun yang punya image-nya. Minta variabelnya secara
eksplisit pada langkah yang membutuhkan:

```dockerfile
RUN --mount=type=secret,id=VITE_API_URL,env=VITE_API_URL npm run build
```

Nilainya juga tersedia sebagai file `/run/secrets/VITE_API_URL` bila hanya memakai
`--mount=type=secret,id=VITE_API_URL`. Pada Dockerfile, mengubah nilai variabel tidak
otomatis membangun ulang langkah yang sudah ter-cache. Perubahan kode sebelum langkah
itu akan membangunnya ulang.

**Yang tetap harus diperhatikan user.** Variabel yang dimasukkan framework ke hasil
build, seperti `VITE_*` dan `NEXT_PUBLIC_*`, akan terbaca oleh siapa pun yang membuka
aplikasinya, jadi jangan menaruh secret di sana. Secret pada langkah build juga bisa
tersimpan bila perintahnya menulisnya ke file. Log build yang mencetak nilainya
diredaksi, selama nilainya 8 karakter atau lebih dan bukan kata umum.

Detail implementasi:

- Agent tidak memakai `--build-arg` untuk variabel project, dan nilai tidak pernah
  ada di process argument.
- Nilai sampai ke `docker buildx` lewat environment bernama `SAKALA_BUILD_SECRET_<n>`,
  bukan nama variabel project. Nama seperti `LD_PRELOAD` atau `DOCKER_HOST` tidak boleh
  sampai ke environment process di host.
- `railpack prepare`, yang berjalan di [sandbox](RAILPACK_STRATEGY.md#sandbox), hanya
  menerima nama dengan nilai pengganti, karena nama itu yang ditulis ke plan. Pengecualiannya pengaturan `RAILPACK_*` milik Railpack sendiri, yang
  membutuhkan nilai asli, seperti `RAILPACK_NODE_VERSION`.
- `secrets-hash` untuk Railpack adalah sidik jari ber-key acak per process Agent,
  bukan hash polos, supaya nilai lemah tidak bisa ditebak dari process argument.
  Setelah Agent restart, langkah ter-cache dibangun ulang satu kali.

Jalur Dockerfile diverifikasi dengan BuildKit asli oleh smoke test. Jalur Railpack
diverifikasi end-to-end pada `heroku/node-js-getting-started` oleh smoke test.

## Download macet di dalam build

Pada satu laptop WSL2, build Railpack gagal di `mise install-into caddy` karena download
di dalam BuildKit timeout. Yang terukur, dengan mengunduh arsip rilis Caddy dari GitHub:

| Jalur | Hasil |
| --- | --- |
| `docker buildx build`, network default | Timeout, 2 dari 2. Request kecil berhasil. |
| `docker buildx build --network host` | Berhasil, 12 detik. |
| `docker run`, bridge MTU 1500 | Berhasil, 2 dari 2. |
| `docker run`, bridge MTU 1280 | Timeout, 2 dari 2. |

Dugaan PMTU black hole belum terbukti, dan data ini tidak mendukungnya: default route
laptop itu lewat interface ber-MTU 1500, bukan `eth0` yang 1280, dan menurunkan MTU bridge
malah membuat download macet. Jadi jangan menurunkan `mtu` Docker sebagai perbaikan tanpa
mengukurnya. Penyebabnya belum diketahui.

Agent sengaja tidak menyediakan pilihan `--network host`. Menurut source Railpack v0.23.0,
frontend-nya tidak membaca mode network, jadi flag itu kemungkinan besar tidak berpengaruh
pada build Railpack, yang justru bermasalah di sini. Pada build Dockerfile flag itu memberi
langkah build milik user akses ke network host, termasuk loopback. Operator yang butuh jalan
keluar dapat memilih builder lain lewat `BUILDX_BUILDER`, yang diwarisi process `docker`,
misalnya builder `docker-container` dengan `--driver-opt network=host`. Jalur ini sudah terbukti
pada deployment sungguhan di mesin yang sama: build lewat builder itu selesai, padahal builder
default macet saat mengunduh. Langkah build di builder ini mendapat akses ke network host, jadi
pakai hanya pada node yang menjalankan repository tepercaya.

## Port dari base image

Dockerfile yang paling sering dipakai mahasiswa berakhir di `FROM nginx:alpine`,
`nginx:1.27`, atau `httpd:2.4` tanpa `EXPOSE` sendiri. Port 80-nya datang dari `EXPOSE 80`
di dalam base image, yang tidak terlihat dari Dockerfile. Tanpa port itu, API memakai 3000
dan deployment gagal dengan `runtime_app_not_listening`.

Karena itu, bila stage terakhir tidak punya `EXPOSE`, `InspectProject` membaca config base
image dari registry:

1. Agent mengikuti `FROM <alias stage>` mundur sampai stage yang dibangun dari image luar.
   `scratch` dilewati, begitu juga referensi yang masih berisi `ARG` atau `$` yang tidak
   bisa dipastikan. `ARG` dengan default dipakai default-nya, sama seperti saat build.
2. `docker buildx imagetools inspect <ref> --raw` mengambil manifest atau index. Untuk index
   multi-platform, Agent memilih manifest `linux/<arsitektur node>`, lalu `linux/amd64` bila
   tidak ada.
3. `docker buildx imagetools inspect <ref>@<digest> --format '{{json .Image}}'` mengambil
   config platform itu. Port TCP di `Config.ExposedPorts` menjadi `exposed_ports`, dengan
   aturan yang sama seperti `EXPOSE`: terurut, tanpa duplikat, paling banyak 32, tanpa
   `/udp` dan `/sctp`.

Bila Dockerfile punya `EXPOSE` di mana pun pada rantai stage terakhir, termasuk yang tidak
bisa dipastikan atau yang hanya `/udp`, lookup tidak dijalankan dan port dari Dockerfile
yang dipakai. BuildKit sebenarnya menggabungkan keduanya, tetapi `FROM nginx` dengan
`EXPOSE 8080` biasanya berarti nginx sudah dipindah ke 8080, dan port 80 dari base image
akan membuat API tidak memilih port sama sekali.

`imagetools` bicara langsung ke registry dan tidak memakai Docker daemon, jadi tidak ada
layer yang ditarik, tidak ada build, dan tidak ada yang dijalankan dari repository.
Satu `--format` tanpa `--raw` sebenarnya cukup, tetapi buildx lalu mengambil manifest semua
platform di index, satu request per manifest, jadi belasan request untuk image resmi seperti
`nginx:alpine`. Docker Hub menghitung setiap request manifest terhadap limit anonim node
(100 per jam per IP saat diuji), dan limit itu juga dipakai build. Saat diuji, belasan
lookup dengan cara itu sudah menghabiskan limit dan berakhir dengan `429 Too Many
Requests`. Dua langkah di atas hanya memakai dua request manifest.

Batasnya, karena referensi datang dari repository yang tidak tepercaya:

| Batas | Nilai |
| --- | --- |
| Registry | Hanya `docker.io` (termasuk image `library` tanpa host), `ghcr.io`, `quay.io`, `public.ecr.aws`, `mcr.microsoft.com`, dan `registry.gitlab.com`, dicocokkan persis setelah referensi dinormalkan seperti Docker. Host lain, alamat IP, `localhost`, host dengan port, dan nama seperti `ghcr.io@sha256:...` yang bagi Docker berarti image Docker Hub dilewati tanpa network call. |
| Credential | `DOCKER_CONFIG` dan `BUILDX_CONFIG` menunjuk direktori kosong mode `0700` yang dibuat untuk satu lookup lalu dihapus. Process tidak mewarisi `DOCKER_*`, `BUILDX_*`, atau `BUILDKIT_*` dari Agent, jadi `DOCKER_AUTH_CONFIG`, credential helper, context, dan builder operator tidak terpakai. Allowlist environment lain (`PATH`, `HOME`, proxy, sertifikat) tetap berlaku. |
| Waktu | 10 detik untuk seluruh lookup. Di laptop pengujian, satu lookup ke Docker Hub butuh sekitar 5 sampai 7 detik. |
| Jumlah | Satu referensi per inspection, paling banyak dua request manifest. |
| Kegagalan | Timeout, registry menolak, rate limit, image tidak ada, atau buildx tidak ada hanya berarti tidak ada port dari base image. Inspection tidak pernah gagal karena lookup ini. |
| Input | Referensi dibentuk ulang dari komponen yang sudah divalidasi menjadi bentuk lengkap, misalnya `docker.io/library/nginx:alpine`, dan digest dari registry dicek formatnya sebelum dipakai sebagai argumen. |

Bila buildx hanya terpasang di direktori plugin milik user (`~/.docker/cli-plugins` atau
`$DOCKER_CONFIG/cli-plugins`), config kosong itu memuat `cliPluginsExtraDirs` yang menunjuk
ke sana. Hanya plugin yang dipakai dari direktori itu.

Registry publik ikut mengetahui image apa yang dicari node, dan isi config-nya dikendalikan
pemilik image. Agent hanya mengambil nomor port dari config itu.

## Saran variabel dan database

Wizard Console menyarankan variabel dan database sebelum deployment pertama. Bahannya
datang dari `InspectProject`: `env_example` berisi variabel dari `.env.example`,
`.env.sample`, atau `.env.dist`, dan `database_hint` berisi bukti bahwa project memakai
database beserta release command migrasinya. Bentuk field dan aturan lengkapnya ada di
[Sakala Agent API](AGENT_API.md#variabel-dari-envexample).

`frameworks` menyebut framework yang merender halaman saat build, seperti Gatsby atau Astro,
dari nama dependency di `package.json`. API memakainya untuk memutuskan apakah perubahan
variabel butuh build ulang, karena Railpack melaporkan framework seperti itu sebagai Node biasa.

Ketiganya hanya membaca root directory project (folder package pada monorepo), tidak
pernah di atasnya, dengan batas yang sama seperti `Dockerfile`: symlink hanya diikuti
selama tetap di dalam direktori itu, hanya file biasa, paling besar 1 MiB per file
(64 KiB untuk file contoh environment), dan listing direktori berhenti di 200 entri.
`.env` dan nama lain di luar tiga kandidat tidak pernah dibuka. Tidak ada yang
dijalankan dan isi file tidak ditulis ke log.

Parser file contoh environment ditulis sendiri. `dotenvy` 0.15.7 mengisi `${VAR}` dan
`$VAR` dari environment process lebih dulu (`std::env::var` di `apply_substitution`)
tanpa opsi untuk mematikannya, sehingga baris seperti `X=${SAKALA_AGENT_TOKEN}` di
repository akan menyalin environment Agent ke hasil inspection. Parser Sakala tidak
melakukan interpolasi apa pun.

Nama package yang dihitung sebagai sinyal `package_dependency`:

| File | Package | Engine |
| --- | --- | --- |
| `package.json` (`dependencies`, `devDependencies`) | `pg`, `postgres`, `@neondatabase/serverless` | postgresql |
| | `mysql`, `mysql2` | mysql |
| | `better-sqlite3`, `sqlite3` | sqlite |
| | `@prisma/client`, `prisma`, `drizzle-orm`, `sequelize`, `typeorm`, `knex` | tidak menentukan |
| `composer.json` (`require`, `require-dev`) | `doctrine/orm`, `doctrine/dbal` | tidak menentukan |
| `requirements.txt`, `pyproject.toml` | `psycopg`, `psycopg2`, `psycopg2-binary`, `asyncpg` | postgresql |
| | `mysqlclient`, `pymysql` | mysql |
| | `dj-database-url`, `sqlalchemy` | tidak menentukan |
| `Gemfile` | `pg` | postgresql |
| | `mysql2`, `trilogy` | mysql |
| | `sqlite3` | sqlite |

File Python dibaca per baris: nama sebelum versi, extra, atau marker, dinormalisasi
seperti PEP 503 (huruf kecil, `_` dan `.` menjadi `-`). Pada `pyproject.toml` string
dalam array satu baris seperti `dependencies = ["django>=5", "psycopg"]` ikut dibaca.
`config/database.yml` dibaca per baris, bukan sebagai YAML, karena file Rails biasanya
memuat ERB; `adapter:` berisi ERB dilaporkan sebagai `null`.

## Batas Keamanan MVP

- Repository memakai URL canonical `https://github.com` tanpa credential pada URL. Repository private memakai credential lease sementara melalui `GIT_ASKPASS` dan tidak menaruh token di arguments, remote URL, atau log.
- Checkout harus memakai full 40-character commit SHA.
- Tidak ada shell interpolation; executable dan arguments dikirim langsung ke process API.
- Process anak hanya mewarisi allowlist environment (`PATH`, `HOME`, `DOCKER_*`, `BUILDX_*`, dan sejenisnya). `SAKALA_*`, termasuk token Agent, tidak pernah diwariskan. Lookup port base image juga tidak mewarisi `DOCKER_*`, `BUILDX_*`, dan `BUILDKIT_*`.
- Runtime env ditulis sementara ke file mode `0600`, dibaca `docker run --env-file`, lalu dihapus.
- Agent mengekspor `PORT=container_port` ke env container bila deployment tidak menyetelnya. Image Railpack menyajikan aplikasi pada `$PORT` (Caddyfile-nya listen pada `:{$PORT:80}`, starter Node membaca `process.env.PORT`), sehingga tanpa variabel ini workload listen pada default-nya sendiri sementara route menunjuk port yang dipilih control plane. `PORT` dari payload yang berbeda dari `container_port` ditolak dengan `runtime_port_conflict`, bukan dimenangkan salah satu. Agent sendiri tidak memilih port. `InspectProject` melaporkan `exposed_ports` dari `EXPOSE` di Dockerfile, atau dari [base image](#port-dari-base-image) bila Dockerfile tidak punya `EXPOSE`, dan API bisa memakainya sebagai `container_port` bila hanya ada satu port dan project tidak mengatur port sendiri. Aplikasi tetap wajib listen di `0.0.0.0:$PORT`, dan kegagalan readiness menyebut port-nya.
- Container memakai memory, CPU, PID limit dari command control plane, `no-new-privileges`, dan `--cap-drop ALL` lalu hanya delapan capability dari allowlist kecil yang dikembalikan. Lihat [Capability workload](#capability-workload).
- Tidak ada host volume atau Docker socket di aplikasi user.
- Setiap project berada di bridge network sendiri bersama Caddy, sehingga aplikasi tidak bisa menjangkau aplikasi project lain secara langsung. Lihat [Network per project](#network-per-project).
- Caddy dan aplikasi user tidak menerima Docker socket.

Ini belum merupakan multi-tenant sandbox. Docker daemon access tetap privileged; pilot harus memakai node khusus dan workload terkontrol.

## Capability workload

Workload berjalan dengan `--security-opt no-new-privileges:true`, `--cap-drop ALL`, lalu
`--cap-add` untuk delapan capability ini, dalam urutan yang sama dengan konstanta
`WORKLOAD_CAPABILITIES` di `crates/sakala-agent-runtime/src/containers/docker.rs`:

| Capability | Kenapa dipertahankan |
| --- | --- |
| `CHOWN` | Image nginx resmi meng-`chown` direktori cache ke uid 101 sebelum worker-nya turun ke user itu. |
| `DAC_OVERRIDE` | Proses root di entrypoint membaca dan menulis file milik user aplikasi, misalnya saat menyiapkan direktori sebelum turun privilege. |
| `FOWNER` | `chmod` dan `utime` pada file milik user lain, dipakai entrypoint yang menyiapkan direktori. |
| `FSETID` | Bit setgid tetap terpasang saat root mengubah file milik grup lain. |
| `KILL` | Master process mengirim sinyal ke worker yang sudah berjalan dengan uid lain. |
| `SETGID`, `SETUID` | Turun dari root ke user tanpa privilege: worker nginx dan httpd, `gosu`, `su-exec`, supervisord, pool php-fpm. |
| `NET_BIND_SERVICE` | Exec biner bertanda `cap_net_bind_service=ep` seperti FrankenPHP dan Caddy. Lihat di bawah. |

Sisa set default Docker tetap dibuang:

| Capability | Alasan tetap dibuang |
| --- | --- |
| `NET_RAW` | Raw socket dan packet socket memungkinkan memalsukan paket (ARP, IP) ke container tenant lain di bridge yang sama. |
| `MKNOD` | Membuat device node. Workload web tidak memerlukannya. |
| `SYS_CHROOT` | Hanya dipakai tooling login dan sebagian daemon mail; tidak dibutuhkan image web. |
| `AUDIT_WRITE` | Menulis ke audit log kernel host, dipakai PAM dan sshd. |
| `SETFCAP` | Memasang file capability pada biner, yang bisa dipakai menyiapkan biner untuk menaikkan privilege. |
| `SETPCAP` | Mengubah capability process lain dan bounding set. |

Allowlist ini delapan dari 14 capability default Docker, dan tidak memuat capability
di luar set default seperti `SYS_ADMIN` atau `NET_ADMIN`. Root di dalam container tetap
dibatasi mount namespace-nya sendiri: workload tidak punya host volume, dan satu-satunya
mount dari host, `/etc/resolv.conf` dari `SAKALA_CONTAINER_DNS`, dipasang read-only, yang
tidak bisa ditembus `DAC_OVERRIDE`. `KILL` hanya menjangkau process di PID namespace
container sendiri. Karena `no-new-privileges` tetap dipasang, process non-root tidak
mendapat satu pun capability ini lewat file setuid atau file capability.

### Kenapa bukan hanya `NET_BIND_SERVICE`

v0.3.0 hanya mempertahankan `NET_BIND_SERVICE`. Pada 2026-11-11 terbukti di staging dan
terulang di Docker 29.9.3 lokal bahwa image yang dimulai sebagai root lalu turun privilege
tidak bisa berjalan dengan aturan itu. Hasil pada `nginx:alpine`
(`sha256:713fc2eb...`), `httpd:alpine` (`sha256:610eddac...`), dan `node:22-alpine`
(`sha256:0e175475...`), dicek dengan `wget` dari dalam container:

| Image | Hanya `NET_BIND_SERVICE` | Allowlist |
| --- | --- | --- |
| `nginx:alpine` | Keluar dengan exit code 1, `chown("/var/cache/nginx/client_temp", 101) failed (1: Operation not permitted)` | Melayani port 80, `CapEff` 0x4fb |
| `httpd:alpine` | Melayani port 80, tetapi worker gagal `setgid` (`AH02156`) dan tetap berjalan sebagai root | Melayani port 80, worker berjalan sebagai `www-data` |
| `node:22-alpine` sebagai user 1000 | Melayani port 3000, `CapEff` 0 | Sama, `CapEff` 0 |

Kasus httpd menunjukkan aturan lama justru lebih lemah untuk image seperti itu: request
dilayani process root karena process itu tidak bisa turun ke user tanpa privilege.

### File capability dan `NET_BIND_SERVICE`

`NET_BIND_SERVICE` dipertahankan karena image seperti FrankenPHP (image PHP Railpack)
dan Caddy membawa biner bertanda file capability `cap_net_bind_service=ep`. Smoke test
membacanya langsung dari image Laravel hasil Railpack:
`/usr/local/bin/frankenphp cap_net_bind_service=ep`. Bit `e` membuat kernel memperlakukan
biner itu sebagai *capability-dumb*, dan saat exec kernel mewajibkan seluruh file
permitted capability benar-benar didapat. Bila capability itu tidak ada di bounding set,
`execve` gagal dengan `EPERM` (`bprm_caps_from_vfs_caps` di `security/commoncap.c`,
Linux v6.18 baris 634-647, dan bagian "Safety checking for capability-dumb binaries" di
capabilities(7)). `--cap-drop ALL` tanpa capability ini mengosongkan bounding set,
sehingga setiap aplikasi Laravel hasil Railpack keluar dengan exit code 126 dan
`exec: frankenphp: Operation not permitted`.

`no-new-privileges` bukan penyebabnya dan tetap dipasang. Dengan flag itu kernel hanya
membuang capability yang akan didapat dari file (`cap_bprm_creds_from_file`, baris
940-951) tanpa menggagalkan exec. Terukur pada Docker 29.9.3 dengan `caddy:2.11.3-alpine`:

| Opsi | Hasil exec biner Caddy |
| --- | --- |
| tanpa opsi, atau hanya `no-new-privileges` | Berjalan |
| `--cap-drop ALL`, dengan atau tanpa `no-new-privileges` | Gagal, `operation not permitted` |
| `no-new-privileges --cap-drop ALL --cap-add NET_BIND_SERVICE`, user root | Berjalan, `CapEff` 0x400 |
| sama, user 65534 | Berjalan, `CapEff` 0, capability hanya ada di bounding set |

Capability ini tidak menambah kemampuan yang berarti. Docker memasang
`net.ipv4.ip_unprivileged_port_start=0` di network namespace container, jadi proses tanpa
capability pun sudah bisa listen di port 80 di namespace itu, dan namespace itu milik
container sendiri. Aplikasi tetap listen di `$PORT`.

Biner yang meminta capability di luar allowlist, misalnya `cap_net_raw`, tetap gagal
dengan `runtime_app_exec_denied`. Solusinya `setcap -r` di image atau image lain, bukan
menambah capability.

## Log deployment

Log yang dikirim ke control plane hanya berisi output build dan aplikasi milik
deployment itu, serta pesan sistem yang aman untuk user, semuanya melalui redaction.
Output `caddy validate` dan `caddy reload` tidak ikut, karena menggambarkan router
bersama: path config, log Caddy sendiri, dan peringatan untuk semua site di node,
termasuk milik tenant lain. Output itu masuk log Agent, dan bila Caddy menolak config
deployment gagal dengan `runtime_routing_failed` tanpa menyebut path. Path workspace
dan path file route juga hanya tercatat di log Agent; `git init` dijalankan dengan
`--quiet` karena pesannya memuat path workspace.

## Payload

Lihat `examples/commands/deploy-project.json`. `builder=auto` memilih root Dockerfile bila tersedia dan Railpack bila tidak tersedia. Dockerfile pada subdirectory dan manual command belum didukung.

Resource request berbentuk `memory_mb`, `cpu_millis`, dan `pids_limit`. API adalah source of truth untuk policy produk; agent hanya memakai fallback lokal saat field kosong dan menolak nilai yang melampaui hard maximum node. Completion result mengembalikan nilai `requested_resources` dan `applied_resources` agar API dapat menyimpan konfigurasi runtime aktual. Jika finalisasi setelah route cutover ditunda, result juga membawa `finalization_deferred=true`; API wajib menghentikan deployment sebelumnya dengan `StopProject` karena Agent tidak menghapus running superseded workload secara otomatis.

Kegagalan cepat saat menghentikan atau menghapus container deployment sebelumnya tidak diabaikan. Agent tetap mencoba ready event serta memasang log follower untuk deployment baru, kemudian mengembalikan post-commit error supaya completion membawa `finalization_deferred_reason=runtime_error`.

## Verifikasi

Test default memakai fake process runner dan tidak menyentuh daemon. Integration test nyata perlu dijalankan secara opt-in pada node disposable dengan `sakala-api` yang mendukung protocol revision 4.

Isolasi network per project diuji terhadap daemon sungguhan oleh smoke test opt-in
yang hanya membuat resource berawalan `sakala-netiso-` dan tidak membangun image:

```bash
cargo test -p sakala-agent-runtime --test project_network_smoke_test -- --ignored --nocapture
```

Test ini membuktikan Caddy menjangkau dua project dan workload lama di network
bersama lewat nama, project A gagal menjangkau project B dan workload lama lewat
nama maupun IP, worker berkala menyambungkan ulang Caddy yang dibuat ulang,
`StopProject` menghapus network A dan memutus Caddy, serta startup menyambungkan
ulang Caddy dan menghapus network yang kosong.

Perilaku Docker dan `wget` diuji sungguhan oleh smoke test opt-in. Test ini menjalankan executor dan reporter asli terhadap daemon lokal, hanya checkout Git dan build image yang diganti, serta hanya membuat dan menghapus resource berawalan `sakala-agent-sm`:

```bash
cargo test -p sakala-agent --test docker_smoke_test -- --ignored --nocapture
```
