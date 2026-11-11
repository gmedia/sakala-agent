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
-> docker run candidate pada network project
-> deployment.health.started: tunggu state Docker, lalu probe HTTP dari dalam container Caddy
-> deployment.route.started
-> tulis route Caddy secara atomik
-> validate dan reload Caddy
-> ambil startup logs
-> hapus container deployment sebelumnya
```

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

Image ditag berdasarkan project, commit, dan deployment. Image dan container diberi ownership labels agar discovery dan cleanup tidak menyentuh workload di luar Sakala:

- `dev.sakala.managed=true`
- `dev.sakala.project-id=<uuid>`
- `dev.sakala.deployment-id=<uuid>`
- `dev.sakala.workload-kind=web`
- `dev.sakala.agent-id=<agent-id>`

Label `project-id` dan `deployment-id` juga dipasang pada image Dockerfile dan
Railpack. Setiap image deployment punya tag sendiri, jadi image lama tidak pernah
menjadi dangling, dan `docker image prune` tanpa `-a` tidak menghapus apa pun. Karena
itu GC memilih per project. Project yang masih punya container mempertahankan image
yang dipakai container mana pun, image deployment sebelumnya untuk rollback, dan image
yang lebih muda dari `SAKALA_IMAGE_GC_MAX_AGE_SECONDS` (default tujuh hari). Project
tanpa container sama sekali, misalnya project yang sudah dihapus, hanya mempertahankan
image yang lebih muda dari `SAKALA_ORPHANED_IMAGE_GRACE_SECONDS` (default 24 jam).
Sisanya dihapus dengan `docker image rm` tanpa `--force`. Hanya image berlabel
`dev.sakala.managed=true` dengan project yang valid, dan yang semua namanya
`sakala/project-<project>:<tag>`, yang pernah dipilih.

GC ini berjalan sendiri setiap `SAKALA_IMAGE_GC_INTERVAL_SECONDS` (default satu jam),
dan dilewati selama ada command aktif. Command `CleanupRuntime` dengan `approved: true`
menjalankan aturan yang sama sekali jalan, dan startup reconciliation hanya melaporkan
kandidatnya. `StopProject` dengan `remove_images: true`, yang dikirim API saat project
dihapus, langsung menghapus semua image project itu setelah container-nya hilang. Aturan
lengkapnya ada di [Sakala Agent API](AGENT_API.md#approved-runtime-cleanup).

Versi sebelumnya membaca label dengan `{{.Label "..."}}` pada `docker image ls`, yang
ditolak Docker (`can't evaluate field Label in type *formatter.imageContext`), sehingga
deteksi gagal dan tidak ada image yang pernah dihapus. Label kini dibaca lewat
`docker image inspect`.

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

**Build command.** Hanya berlaku untuk project tanpa Dockerfile. Pada project dengan
Dockerfile, langkah build ditentukan oleh Dockerfile itu sendiri, jadi perintah ini
diabaikan dan log build menyebutkannya.

**Start command.** Berlaku untuk keduanya. Pada project dengan Dockerfile, perintah ini
menggantikan `ENTRYPOINT` dan `CMD` image, dan dijalankan dengan `sh -c` di dalam
container, jadi image-nya harus punya `sh`. Image `scratch` dan distroless gagal start.

Semua perintah ini hanya berjalan di dalam build atau container, tidak pernah di shell
host. Agent tidak memakai `sh -c` di host; argumen dikirim terstruktur ke Docker.

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
lalu dihapus. Setelah aktivasi route dimulai, pembatalan tidak lagi menghentikan
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

## Batas Keamanan MVP

- Repository memakai URL canonical `https://github.com` tanpa credential pada URL. Repository private memakai credential lease sementara melalui `GIT_ASKPASS` dan tidak menaruh token di arguments, remote URL, atau log.
- Checkout harus memakai full 40-character commit SHA.
- Tidak ada shell interpolation; executable dan arguments dikirim langsung ke process API.
- Process anak hanya mewarisi allowlist environment (`PATH`, `HOME`, `DOCKER_*`, `BUILDX_*`, dan sejenisnya). `SAKALA_*`, termasuk token Agent, tidak pernah diwariskan.
- Runtime env ditulis sementara ke file mode `0600`, dibaca `docker run --env-file`, lalu dihapus.
- Agent mengekspor `PORT=container_port` ke env container bila deployment tidak menyetelnya. Image Railpack menyajikan aplikasi pada `$PORT` (Caddyfile-nya listen pada `:{$PORT:80}`, starter Node membaca `process.env.PORT`), sehingga tanpa variabel ini workload listen pada default-nya sendiri sementara route menunjuk port yang dipilih control plane. `PORT` dari payload yang berbeda dari `container_port` ditolak dengan `runtime_port_conflict`, bukan dimenangkan salah satu. Agent sendiri tidak memilih port. `InspectProject` melaporkan `exposed_ports` dari `EXPOSE` di Dockerfile, dan API bisa memakainya sebagai `container_port` bila hanya ada satu port dan project tidak mengatur port sendiri. Aplikasi tetap wajib listen di `0.0.0.0:$PORT`, dan kegagalan readiness menyebut port-nya.
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
