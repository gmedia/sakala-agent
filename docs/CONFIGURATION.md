# Configuration

Agent memuat konfigurasi dari environment variable atau CLI flag yang sepadan. CLI flag mengoverride environment value.

| Variable | Default | Keterangan |
| --- | --- | --- |
| `SAKALA_AGENT_MODE` | `local` | `local` atau `connected`. |
| `SAKALA_AGENT_ID` | `local-agent-01` | Identitas node yang dikirim sebagai `X-Agent-Id`. |
| `SAKALA_AGENT_TOKEN` | tidak ada | Bearer token; wajib dan bukan `change-me` pada connected mode. |
| `SAKALA_API_URL` | `http://localhost:8000` | Base URL `sakala-api` control plane yang dihubungi Agent. Boleh alamat internal, misalnya `http://127.0.0.1:8100`, asalkan `SAKALA_PUBLIC_API_URL` diisi. |
| `SAKALA_PUBLIC_API_URL` | nilai `SAKALA_API_URL` | Base URL `sakala-api` seperti yang dijangkau browser pengunjung, misalnya `https://api.example.ac.id`. Hanya dipakai untuk URL yang dibuka browser, yaitu permintaan wake di halaman route tidur (`POST <URL>/api/v1/public/projects/<slug>/wake`). Isi bila Agent menghubungi API lewat alamat internal, seperti instalasi self-host; tanpa itu project yang tidur tidak bisa dibangunkan dari URL-nya. Harus URL absolut `http` atau `https` tanpa kredensial, query, atau fragment; path dipakai sebagai prefix tempat API dilayani. Nilai yang tidak valid membuat Agent berhenti saat start. Kosong atau tidak diisi berarti sama dengan `SAKALA_API_URL`. |
| `SAKALA_PUBLIC_CONSOLE_URL` | tidak ada | Base URL Console seperti yang dijangkau browser, misalnya `https://app.example.ac.id`. Halaman project yang tidur menautkan pemilik project ke sini ("Kalau ini proyekmu, ... di Console Sakala"); tanpa nilai ini kalimatnya tetap tampil tanpa tautan. Aturannya sama dengan `SAKALA_PUBLIC_API_URL`: URL absolut `http` atau `https` tanpa kredensial, query, atau fragment, dan path dipakai sebagai prefix. Nilai yang tidak valid membuat Agent berhenti saat start. Kosong berarti tidak diisi. |
| `SAKALA_POLL_INTERVAL_SECONDS` | `3` | Interval polling command, harus lebih besar dari nol. |
| `SAKALA_HEARTBEAT_INTERVAL_SECONDS` | `10` | Interval heartbeat, harus lebih besar dari nol. `sakala-api` menandai node `offline` dan berhenti menawarkan command bila tidak ada heartbeat selama `SAKALA_AGENT_OFFLINE_AFTER_SECONDS` (default API 60 detik); jangan mendekati batas itu tanpa koordinasi. |
| `SAKALA_METRICS_INTERVAL_SECONDS` | `60` | Seberapa sering heartbeat mengambil sampel [`workloads.metrics`](AGENT_API.md#metrik-workload) (CPU, memori, dan restart per container workload), antara 10 dan 3600 detik. Sampel dikirim sekali, di heartbeat pertama setelah ia diambil; heartbeat di antaranya tidak membawa list itu. Satu sampel membuat Docker sibuk sekitar 2 detik, dan API merangkumnya per menit, jadi interval di bawah 60 detik jarang berguna. |
| `SAKALA_COMMAND_TIMEOUT_SECONDS` | `900` | Hard maximum node untuk deadline seluruh lifecycle command setelah claim; payload deployment boleh meminta nilai lebih pendek. |
| `SAKALA_MAX_CONCURRENT_COMMANDS` | `4` | Batas global command aktif. Command pada project yang sama tetap diproses satu per satu; command yang belum mendapat slot tetap pending di control plane. |
| `SAKALA_MAX_CONCURRENT_JOBS` | `2` | Batas command [`RunJob`](AGENT_API.md#job-runjob) yang berjalan bersamaan, bilangan bulat di atas 0. Pool ini terpisah dari `SAKALA_MAX_CONCURRENT_COMMANDS` dan dari antrean per project, jadi job yang lama tidak menahan deploy, restart, atau sleep project yang sama. Job yang belum mendapat slot tetap pending di control plane. Setiap job juga diterima di bawah `SAKALA_MAX_ACTIVE_CONTAINERS` sebagai satu container. |
| `SAKALA_SHUTDOWN_GRACE_SECONDS` | `30` | Waktu maksimum untuk command aktif menyelesaikan cancellation dan cleanup sebelum agent membatalkan task yang tersisa. |
| `SAKALA_MAX_CONCURRENT_BUILDS` | `1` | Batas build image Docker/Railpack aktif pada node agar build tidak saling menghabiskan CPU, memori, dan I/O. |
| `SAKALA_RUNTIME_NETWORK` | `sakala-runtime` | Network bersama tempat container Caddy berada. Workload baru tidak bergabung ke sini, melainkan ke network project masing-masing; network ini tetap dibutuhkan untuk workload dari Agent lama. Lihat [Network per project](DOCKER_RUNTIME.md#network-per-project). |
| `SAKALA_AGENT_REGION` | kosong | Region node untuk penempatan project, misalnya `jkt` atau `id-jog-1`. Dikirim di heartbeat sebagai `runtime.region`. Wajib cocok dengan `^[a-z0-9]+(?:-[a-z0-9]+)*$` dan paling panjang 32 karakter, aturan yang sama dengan API. Nilai yang tidak valid membuat Agent berhenti saat start, karena API akan menolak setiap heartbeat-nya. Kosong atau tidak diisi berarti tidak dikirim. Lihat [Lokasi dan kapasitas node](AGENT_API.md#lokasi-dan-kapasitas-node). |
| `SAKALA_AGENT_EDGE_ADDRESS` | kosong | Alamat router runtime node yang bisa dijangkau Sakala Edge, berbentuk `host:port` tanpa skema atau path, misalnya `10.0.47.201:8180`, `[fd00::21]:8180`, atau `runtime-01.internal:8180`. Host berupa IPv4, IPv6 dalam kurung siku, atau nama DNS; port 1 sampai 65535; paling panjang 255 karakter. Dikirim di heartbeat sebagai `runtime.edge_address`, dan API memakainya sebagai upstream di tabel route Edge. Nilai yang tidak valid membuat Agent berhenti saat start. Kosong atau tidak diisi berarti tidak dikirim, dan project di node ini tidak masuk tabel route Edge kecuali admin mengisi override di API. |
| `SAKALA_RUNTIME_DRIVER` | `noop` | `noop` atau executor opt-in `docker`. |
| `SAKALA_RUNTIME_WORKSPACE` | `/var/lib/sakala/builds` | Root workspace checkout/build sementara. |
| `SAKALA_WORKSPACE_GC_MAX_AGE_SECONDS` | `86400` | Umur minimum workspace UUID terbengkalai sebelum GC startup boleh menghapusnya. Nama non-UUID dan symlink tidak disentuh. |
| `SAKALA_IMAGE_GC_MAX_AGE_SECONDS` | tidak dipakai | Usang dan diabaikan. Dulu umur minimum image project yang masih punya container sebelum image GC boleh menghapusnya. Kini project seperti itu menyimpan image yang dipakai container dan image previous (tag `sakala/project-<project>:previous`) berapa pun umurnya, dan tidak ada yang lain. Nilainya masih dibaca dan diperiksa (bilangan bulat di atas 0) supaya konfigurasi lama tetap bisa dimuat, dan Agent menulis peringatan `configuration setting is no longer used and is ignored` saat start. Hapus dari konfigurasi. Lihat [Image dan GC](DOCKER_RUNTIME.md#image-dan-gc). |
| `SAKALA_ORPHANED_IMAGE_GRACE_SECONDS` | `86400` | Umur minimum image milik project yang tidak punya container sama sekali, misalnya project yang sudah dihapus, sebelum image GC menghapusnya, termasuk image terbarunya. Dihitung dari waktu image dibuat. Minimal sama dengan `SAKALA_COMMAND_TIMEOUT_SECONDS`, supaya image deploy yang belum sempat dijalankan tidak ikut terhapus. |
| `SAKALA_IMAGE_GC_INTERVAL_SECONDS` | `3600` | Jarak antar-putaran image GC otomatis. Putaran pertama berjalan satu interval setelah start, dan putaran dilewati selama ada command aktif. Aturannya sama dengan target `stale_images` pada `CleanupRuntime` dan dengan pembersihan setelah setiap deployment sukses: per project yang masih punya container, hanya image yang dipakai container dan image previous yang disimpan. Hanya image berlabel Sakala dengan project yang valid dan bernama `sakala/project-<project>:<tag>` yang dipilih, dan Agent tidak menjalankan `docker image prune -a`. |
| `SAKALA_MIN_WORKSPACE_FREE_MB` | `1024` | Ruang bebas minimum filesystem workspace sebelum Agent menerima checkout/build baru. Nilai di bawah batas lokal ditolak sebagai disk pressure. |
| `SAKALA_RUNTIME_HEALTH_INTERVAL_SECONDS` | `30` | Interval pemeriksaan batch workload aktif berlabel Sakala. Agent mencatat perubahan state lokal saja sampai kontrak pelaporan health ke API disepakati. Interval yang sama menjalankan [pemulihan replika](RUNTIME_HARDENING.md#pemulihan-replika). |
| `SAKALA_CADDY_SITES_DIR` | `/var/lib/sakala/caddy/sites` | Folder route host yang dimount ke Caddy. |
| `SAKALA_CADDY_CONTAINER` | `sakala-caddy` | Container Caddy yang divalidasi dan direload. |
| `SAKALA_RAILPACK_FRONTEND` | `ghcr.io/railwayapp/railpack-frontend:v0.23.0` | BuildKit frontend; pin sesuai Railpack di `SAKALA_RAILPACK_IMAGE`. |
| `SAKALA_RAILPACK_IMAGE` | kosong | Wajib untuk driver `docker`. Image sandbox yang menjalankan Railpack, dipin dengan digest atau image ID. Tag ditolak, dan tanpa nilai ini preflight gagal fatal. Operator membangunnya dari `docker/railpack-sandbox`; langkahnya ada di [Asal image](RAILPACK_STRATEGY.md#asal-image). |
| `SAKALA_RAILPACK_NETWORK` | `sakala-railpack` | Network sandbox Railpack. Dibuat Agent bila belum ada, tanpa komunikasi antar-container. |
| `SAKALA_CONTAINER_RUNTIME` | kosong | Runtime OCI untuk workload, misalnya `runsc`, dengan nama yang terdaftar di Docker. Kosong berarti default Docker. Build dan sandbox Railpack tidak terpengaruh. Bila terisi tetapi tidak terdaftar, preflight gagal fatal, dan Agent tidak pernah pindah ke runtime lain. Lihat [gVisor](RUNTIME_HARDENING.md#gvisor). |
| `SAKALA_CONTAINER_DNS` | kosong | Daftar IP name server untuk workload, dipisah koma, misalnya `1.1.1.1,8.8.8.8`. Agent menulisnya ke `.sakala-resolv.conf` di `SAKALA_RUNTIME_WORKSPACE` dan me-mount-nya sebagai `/etc/resolv.conf` read-only. Wajib di bawah `runsc`; tanpa ini workload tidak bisa me-resolve hostname. |
| `SAKALA_PROJECT_DOMAIN_SUFFIXES` | `run.sakala.localhost,staging.sakala.app,sakala.app` | Suffix domain yang boleh dipakai deployment, dipisah koma, misalnya `run.example.ac.id`. Domain diterima hanya bila berupa tepat satu label DNS ditambah salah satu suffix ini, seperti `portfolio.run.example.ac.id`. Isi dengan `SAKALA_PROJECT_DEFAULT_DOMAIN` milik `sakala-api`. Setiap suffix wajib huruf kecil, minimal dua label, tanpa wildcard, dan label internasional ditulis dalam bentuk `xn--`. Nilai yang tidak valid membuat Agent berhenti saat start. Kosong atau tidak diisi berarti tiga suffix default Sakala hosted. Lihat [Domain project](#domain-project). |
| `SAKALA_CUSTOM_DOMAIN_FORBIDDEN_ZONES` | `sakala.app,sakala.dev,localhost` | Zona yang tidak boleh menjadi custom domain project (ADR-024), dipisah koma: zona itu sendiri dan semua nama di bawahnya ditolak dengan `runtime_custom_domain_invalid`. Suffix domain project selalu ditolak juga, tanpa perlu ditulis di sini. Setiap zona huruf kecil, boleh satu label (`localhost`), tanpa wildcard. Mengisi nilai mengganti seluruh default. Nilai yang tidak valid membuat Agent berhenti saat start. Samakan dengan `SAKALA_CUSTOM_DOMAIN_FORBIDDEN_ZONES` di `sakala-api` dan `SAKALA_EDGE_CUSTOM_DOMAIN_FORBIDDEN_ZONES` di Sakala Edge. Lihat [Custom domain](#custom-domain). |
| `SAKALA_BUILD_TIMEOUT_SECONDS` | `600` | Hard maximum node untuk fase build image; payload deployment boleh meminta nilai lebih pendek. |
| `SAKALA_START_TIMEOUT_SECONDS` | `120` | Hard maximum node untuk start container dan health readiness; harus lebih pendek dari command timeout. |
| `SAKALA_RELEASE_COMMAND_TIMEOUT_SECONDS` | `600` | Batas waktu [release command](DOCKER_RUNTIME.md#release-command) satu deployment, dalam detik. Harus bilangan bulat positif dan lebih pendek dari `SAKALA_COMMAND_TIMEOUT_SECONDS`; nilai lain membuat Agent berhenti saat start. Release command yang melewati batas ini dibunuh dan deploy gagal dengan `release_command_timeout`. Batas ini berlaku di dalam deadline command yang sama dengan build dan start, jadi build lama ditambah release command lama bisa lebih dulu mencapai command timeout dan gagal dengan `runtime_timeout`. |
| `SAKALA_WORKER_STARTUP_SECONDS` | `10` | Berapa detik [worker](AGENT_API.md#proses-worker) harus tetap berjalan, tanpa restart, setelah dinyalakan sebelum deployment menganggapnya siap. Worker yang keluar lebih dulu menggagalkan deploy dengan `worker_start_failed` sebelum route berpindah. Bilangan bulat positif yang lebih pendek dari `SAKALA_COMMAND_TIMEOUT_SECONDS`; nilai lain membuat Agent berhenti saat start. |
| `SAKALA_WORKER_STOP_GRACE_SECONDS` | `30` | Jeda antara `SIGTERM` dan `SIGKILL` untuk container worker saat deploy menggantikannya, `RestartProject`, `SleepProject`, dan `StopProject`, supaya job yang sedang dikerjakan bisa selesai. Replika web tetap 10 detik. Aturan nilainya sama dengan `SAKALA_WORKER_STARTUP_SECONDS`. |
| `SAKALA_MAX_ACTIVE_CONTAINERS` | `20` | Guard kapasitas container aktif pada node, bukan kuota plan/user. Setiap replika dan setiap container worker dihitung satu; container release dan job yang sedang berjalan juga. Deploy atau wake diterima bila container berjalan milik project lain ditambah replika baru tidak melebihi nilai ini; container project itu sendiri tidak dihitung karena akan digantikan, jadi redeploy tetap memperoleh replacement slot. `WakeProject` memakai guard yang sama, dan nilai ini juga menjadi batas item `workloads.traffic` di heartbeat. Nilainya dikirim di heartbeat sebagai `workloads.maximum_active_workloads` untuk penempatan berdasarkan kapasitas. |
| `SAKALA_MAX_REPLICAS_PER_PROJECT` | `4` | Jumlah replika terbanyak yang boleh diminta satu deployment lewat field `replicas`, antara 1 dan 64. Deployment yang meminta lebih gagal dengan `runtime_replicas_exceeded`, bukan dijalankan dengan jumlah lebih sedikit. Setiap replika tetap dihitung terhadap `SAKALA_MAX_ACTIVE_CONTAINERS`. Nilainya dikirim di heartbeat sebagai `workloads.maximum_replicas_per_project`. Lihat [Replika](AGENT_API.md#replika). |
| `SAKALA_DEFAULT_CONTAINER_MEMORY_MB` | `256` | Fallback memory bila command tidak menentukan nilai. |
| `SAKALA_MAX_CONTAINER_MEMORY_MB` | `512` | Hard ceiling memory yang diizinkan node. |
| `SAKALA_DEFAULT_CONTAINER_CPU_MILLIS` | `500` | Fallback CPU; `500` berarti `0.5` vCPU. |
| `SAKALA_MAX_CONTAINER_CPU_MILLIS` | `1000` | Hard ceiling CPU; `1000` berarti `1` vCPU. |
| `SAKALA_DEFAULT_CONTAINER_PIDS_LIMIT` | `128` | Fallback jumlah process container. |
| `SAKALA_MAX_CONTAINER_PIDS_LIMIT` | `256` | Hard ceiling jumlah process pada node. |
| `SAKALA_DATABASE_DRIVER` | `none` | `none` atau `postgres`. `postgres` menjadikan node host database untuk PostgreSQL terkelola, mengiklankan capability `managed-postgres`, dan bisa dipakai bersama driver runtime mana pun. Lihat [Database Host](DATABASE_HOST.md). |
| `SAKALA_DATABASE_ADMIN_DSN` | tidak ada | Wajib dengan `SAKALA_DATABASE_DRIVER=postgres`. DSN libpq (URL atau `key=value`) ke instance lokal sebagai superuser, misalnya `host=/var/run/postgresql user=postgres dbname=postgres`. Semua host harus socket Unix atau alamat loopback; host lain membuat Agent berhenti saat start. Hanya dibaca dari environment (tidak ada CLI flag, supaya password superuser tidak muncul di argumen process) dan tidak pernah dicetak. Simpan di env file berizin terbatas. |
| `SAKALA_DATABASE_PUBLIC_HOST` | tidak ada | Wajib dengan `SAKALA_DATABASE_DRIVER=postgres`. Alamat instance seperti yang dijangkau runtime node lewat jaringan privat: IPv4, IPv6 tanpa kurung siku, atau nama DNS. Dikirim di heartbeat sebagai `database.endpoint.host`. |
| `SAKALA_DATABASE_PUBLIC_PORT` | `5432` | Port 1 sampai 65535 untuk `SAKALA_DATABASE_PUBLIC_HOST`. |
| `SAKALA_OBJECT_STORAGE_DRIVER` | `none` | `none` atau `rustfs`. `rustfs` menjadikan node host object storage untuk bucket terkelola, mengiklankan capability `managed-s3`, dan bisa dipakai bersama driver runtime dan database mana pun. Lihat [Object Storage Host](OBJECT_STORAGE_HOST.md). |
| `SAKALA_OBJECT_STORAGE_CONTAINER` | `sakala-object-storage-rustfs-1` | Container RustFS yang dijalankan `docker exec`. Wajib cocok dengan `^[A-Za-z0-9][A-Za-z0-9_.-]{0,127}$`. |
| `SAKALA_OBJECT_STORAGE_PUBLIC_ENDPOINT` | tidak ada | Wajib dengan `SAKALA_OBJECT_STORAGE_DRIVER=rustfs`. Alamat S3 publik, misalnya `https://s3.sakala.app`: `https://host[:port]` tanpa path, query, fragment, atau kredensial, paling panjang 255 karakter; `http://` hanya untuk host loopback. Dikirim di heartbeat sebagai `object_storage.endpoint`. |
| `SAKALA_OBJECT_STORAGE_REGION` | `us-east-1` | Region tanda tangan S3, `^[a-z0-9]+(-[a-z0-9]+)*$`, paling panjang 32 karakter. Harus sama dengan `RUSTFS_REGION`. Dikirim sebagai `object_storage.region`. |
| `SAKALA_OBJECT_STORAGE_TOTAL_QUOTA_MB` | tidak ada | Wajib dengan `rustfs`, bilangan bulat di atas 0. Jumlah semua kuota bucket di instance tidak boleh melewatinya. Dikirim sebagai `object_storage.capacity_mb`. Isi dengan nilai yang sama dengan `DISK_GUARD_OBJECT_STORAGE_QUOTA_MB` di repo deployment. |
| `SAKALA_OBJECT_STORAGE_MIN_FREE_MB` | `1024` | Bilangan bulat ≥ 0. Ruang disk bebas minimum di `SAKALA_OBJECT_STORAGE_DATA_DIR` saat semua bucket penuh; bucket baru dan kenaikan kuota yang melanggarnya ditolak. |
| `SAKALA_OBJECT_STORAGE_DATA_DIR` | `/var/lib/sakala/object-storage` | Path absolut direktori data RustFS di host, dibaca dengan `df`. |
| `SAKALA_VALKEY_DRIVER` | `none` | `none` atau `docker`. `docker` menjalankan store Valkey terkelola (ADR-023), satu container per store, dan mengiklankan capability `managed-valkey`. Butuh `SAKALA_RUNTIME_DRIVER=docker`. Lihat [Valkey Host](VALKEY_HOST.md). |
| `SAKALA_VALKEY_IMAGE` | `valkey/valkey:9.2.0-alpine@sha256:c8e2d174…0882` | Image store, wajib dipin dengan digest: `<name>[:tag]@sha256:<64 hex huruf kecil>`. Nilai lengkap default ada di [Valkey Host](VALKEY_HOST.md#konfigurasi). |
| `SAKALA_VALKEY_NETWORK_POOL` | tidak ada | Wajib dengan `docker`. Rentang IPv4 CIDR (prefix /27 atau lebih pendek, ditulis di alamat network-nya), misalnya `10.213.0.0/16`, tempat setiap network store mengambil satu /27. Pilih rentang di luar LAN, rute host, dan `default-address-pools` Docker. |
| `SAKALA_VALKEY_TOTAL_MEMORY_MB` | tidak ada | Wajib dengan `docker`, bilangan bulat di atas 0. Σ(`maxmemory_mb` + 64) semua store node tidak boleh melewatinya. Dikirim sebagai `valkey.capacity_mb`. |
| `SAKALA_VALKEY_DATA_DIR` | `/var/lib/sakala/valkey` | Path absolut catatan, password admin, dan konfigurasi setiap store. Tanpa koma, tanda sama dengan, tanda kutip, atau baris baru. Data store sendiri ada di volume Docker `sakala-valkey-<id>`. |
| `SAKALA_LOG_LEVEL` | `info` | Filter `tracing-subscriber`, misalnya `debug` atau `sakala_agent=debug`. |

`SAKALA_AGENT_MODE` dan `SAKALA_RUNTIME_DRIVER` tidak mewakili hal yang sama. Mode menentukan koneksi control plane, sedangkan driver menentukan efek command pada runtime node. Lihat [Operating Modes](OPERATING_MODES.md) untuk matriks lengkap.

## Local Mode

```bash
cp examples/env/local.env.example .env
cargo run -p sakala-agent
```

File `.env` tidak dimuat otomatis oleh binary. Export variable menggunakan shell/environment runner yang dipilih, atau jalankan dengan default local mode tanpa file. Tidak ada control-plane request pada mode ini.

## Connected Mode

```bash
SAKALA_AGENT_MODE=connected \
SAKALA_AGENT_ID=node-01 \
SAKALA_AGENT_TOKEN='<issued-agent-token>' \
SAKALA_API_URL=http://localhost:8000 \
cargo run -p sakala-agent
```

Jangan menyimpan token issued pada repository atau command history yang dibagikan.

Executor Docker harus diaktifkan eksplisit dan hanya pada runtime node yang telah memiliki Git, Docker Buildx, image sandbox Railpack, network `sakala-runtime`, dan Caddy contract dari `sakala-infra`. Jangan mengaktifkannya pada laptop contributor tanpa memahami operasi container yang akan dijalankan.

Implementasi MVP mengharapkan Caddy berjalan sebagai container bernama `SAKALA_CADDY_CONTAINER` dan bergabung ke network `sakala-runtime`. Agent menyambungkan container itu ke network setiap project, dan menyambungkannya lagi dalam 60 detik bila container Caddy dibuat ulang. Instalasi `/usr/bin/caddy` pada host tidak dipakai oleh adapter ini. Jangan menjalankan Caddy host dan Caddy container pada host port yang sama.

## Domain project

Payload `DeployProject` membawa `domain` yang ditulis Agent ke route Caddy. Agent hanya menerima domain yang berupa satu label DNS di bawah suffix pada `SAKALA_PROJECT_DOMAIN_SUFFIXES`, supaya satu project tidak bisa mengklaim domain project lain atau domain yang tidak dilayani node ini.

Instalasi self-hosted di `example.ac.id` yang memberi domain project `<slug>.run.example.ac.id` cukup memakai:

```bash
SAKALA_PROJECT_DOMAIN_SUFFIXES=run.example.ac.id
```

Dengan nilai itu, `portfolio.run.example.ac.id` diterima, sedangkan `api.portfolio.run.example.ac.id` (subdomain bertingkat), `Portfolio.run.example.ac.id` (huruf besar), `portfolio.sakala.app`, dan label lebih dari 63 karakter ditolak dengan `invalid_runtime_command`. Panjang domain maksimum 253 karakter sesuai RFC 1035. Label internasional harus dikirim dalam bentuk `xn--`, misalnya `xn--mnchen-3ya.run.example.ac.id`; ejaan Unicode dan label `xn--` yang bukan punycode valid ditolak.

Sakala hosted memakai `sakala.app` untuk production (`<slug>.sakala.app`) dan `staging.sakala.app` untuk staging (`<slug>.staging.sakala.app`), registrable domain yang terpisah dari API dan Console di `sakala.dev` (ADR-016). Suffix harus sama persis, bukan sekadar akhiran: node yang hanya memakai `sakala.app` menolak `x.staging.sakala.app`, dan slug `staging` dicadangkan di `sakala-api`.

Suffix yang berlaku dicatat pada log preflight sebagai check `project-domains`. Ganti nilainya bila `SAKALA_PROJECT_DEFAULT_DOMAIN` di `sakala-api` berubah, karena domain yang dikirim API harus tetap berada di bawah salah satu suffix ini.

Mengganti domain tidak memindahkan workload yang sudah berjalan. Agent menyimpan domain sebagai label `dev.sakala.domain` di container, dan `WakeProject` serta `RestartProject` membangun route dari label itu. Setelah `projects:move-domain` di `sakala-api`, setiap project harus dideploy ulang, termasuk yang sedang tidur, supaya route-nya pindah ke domain baru. Container lama dengan domain yang tidak lagi diizinkan tetap bisa di-wake atau di-restart, tetapi bila ditidurkan, Agent tidak bisa membuat halaman bangun untuknya dan menghapus route-nya.

## Custom domain

Project juga bisa dilayani di custom domain yang sudah dibuktikan pemiliknya di `sakala-api` (ADR-024). Command yang menulis route membawa `custom_domains`, dan Agent menambahkannya sebagai alamat site di file route project yang sama, setelah domain project. Berbeda dengan domain project, custom domain tidak disimpan di label container, karena bisa berubah tanpa redeploy lewat `UpdateProjectDomains`; file route-nya yang menjadi catatan, sehingga command tanpa daftar mempertahankannya.

Agent tidak menormalkan nama dan menolak nama di bawah suffix domain project dan di bawah `SAKALA_CUSTOM_DOMAIN_FORBIDDEN_ZONES`. Instalasi self-hosted di `example.ac.id` yang tidak ingin pengguna memakai nama kampus sebagai custom domain bisa memakai:

```bash
SAKALA_PROJECT_DOMAIN_SUFFIXES=run.example.ac.id
SAKALA_CUSTOM_DOMAIN_FORBIDDEN_ZONES=example.ac.id,localhost
```

TLS custom domain diterbitkan di Edge, bukan di router node, yang tetap HTTP di port 80. Aturan lengkap dan kode kegagalan ada di [Agent API](AGENT_API.md#custom-domain).

## Resource Policy Boundary

`sakala-api` menentukan policy resource berdasarkan project, workspace, atau plan lalu mengirim `resources` pada command deployment. Agent tidak memiliki logic plan. Konfigurasi `DEFAULT` hanya dipakai ketika field command kosong, sedangkan konfigurasi `MAX` melindungi kapasitas node. Request nol atau melebihi maximum ditolak secara eksplisit dan tidak di-clamp diam-diam.

Kuota jumlah project per user/workspace dan jumlah deployment yang diizinkan plan tetap milik `sakala-api`. `SAKALA_MAX_ACTIVE_CONTAINERS` hanya mencegah runtime node menerima workload baru setelah kapasitas lokal tercapai. Nilai ini tidak boleh dipakai untuk menyimpulkan entitlement user.

Perubahan memory/CPU/PID dilakukan dengan command redeploy yang membawa resource profile baru. Agent membuat candidate container baru, memverifikasinya, mengalihkan route, lalu membersihkan container sebelumnya. Phase 9 sengaja tidak memakai `docker update` agar requested state, applied state, image, dan deployment history tetap konsisten.

`SAKALA_RUNTIME_NETWORK` tetap konfigurasi node karena merupakan detail topologi host, bukan product-level resource policy.

## Runtime Timeout and Log Policy Boundary

`sakala-api` mengirim `timeouts` dan `log_bounds` yang telah di-resolve dari policy produk. Untuk `DeployProject`, agent memakai deadline payload pada fase build, start/health, dan lifecycle command. Konfigurasi `SAKALA_*_TIMEOUT_SECONDS` tetap merupakan hard maximum node: nilai nol atau nilai payload di atas maximum ditolak, bukan di-clamp diam-diam.

Log deployment selalu melewati redaksi sebelum dikirim. Agent kemudian memotong setiap message pada `log_bounds.max_line_length` dan berhenti mengirim setelah `log_bounds.max_total_bytes`. Baris dikirim dalam batch `{ "logs": [...] }` yang dibatasi `min(log_bounds.max_batch_lines, 100)` baris dan 512 KiB message per request; `max_batch_lines = 0` berarti tidak ada log yang dikirim. Setiap request membawa `Idempotency-Key` sehingga retry transient aman. API tetap harus memvalidasi ulang seluruh batas ini sebagai trust boundary terakhir.
