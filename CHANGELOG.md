# Changelog

Semua perubahan penting pada project ini akan dicatat di sini. Format mengikuti Keep a Changelog dan release mendatang akan menggunakan Semantic Versioning.

## [Unreleased]

## [0.5.0] - 2026-11-11

### Added

- Result `InspectProject` kini memuat `exposed_ports`, yaitu port TCP yang dideklarasikan `EXPOSE` di stage terakhir `Dockerfile`, misalnya `[80]` untuk Dockerfile yang berakhir di `nginx:alpine` dengan `EXPOSE 80`. API bisa memakainya sebagai port default bila hanya ada satu port, sehingga aplikasi seperti itu tidak lagi gagal dengan `runtime_app_not_listening` karena Sakala memprobe port 3000. Pembacanya mengikuti aturan BuildKit untuk multi-stage, `ARG` dan `ENV`, komentar, continuation, dan heredoc, dan mengabaikan nilai yang tidak bisa dipastikan. Hasilnya sudah dicocokkan dengan `ExposedPorts` dari build BuildKit sungguhan untuk 24 Dockerfile. Field ini aditif di protocol revision 4; lihat [Compatibility](docs/COMPATIBILITY.md#revision-4-additions-after-v040).
- Bila stage terakhir `Dockerfile` tidak punya `EXPOSE`, `exposed_ports` kini berisi port dari `Config.ExposedPorts` base image-nya, dibaca dari registry tanpa menarik image. Dockerfile yang berakhir di `FROM nginx:alpine` atau `FROM httpd:2.4` tanpa `EXPOSE` sendiri kini menghasilkan `[80]`, bukan `[]` yang membuat API memprobe port 3000. Agent mengikuti `FROM <stage>` sampai image luar, melewati `scratch` dan referensi berisi `ARG` yang tidak bisa dipastikan, dan memilih platform node dari index multi-platform. Lookup memakai `docker buildx imagetools inspect` dengan dua request manifest, hanya untuk `docker.io`, `ghcr.io`, `quay.io`, `public.ecr.aws`, `mcr.microsoft.com`, dan `registry.gitlab.com`, dengan `DOCKER_CONFIG` kosong dan tanpa `DOCKER_*`, `BUILDX_*`, atau `BUILDKIT_*` dari Agent, sehingga credential registry tidak pernah terpakai. Batasnya 10 detik dan satu referensi per inspection, dan kegagalan tidak menggagalkan inspection. Lihat [Docker Runtime](docs/DOCKER_RUNTIME.md#port-dari-base-image).
- Smoke test opt-in `a_real_inspection_reports_the_port_of_an_nginx_dockerfile` menjalankan inspection sungguhan, termasuk sandbox Railpack, atas Dockerfile Node lalu nginx, dan membandingkan hasilnya dengan `ExposedPorts` dari BuildKit.

### Fixed

- Inspection project yang punya `Dockerfile` tidak lagi gagal dengan `runtime_build_failed` saat `railpack info` gagal, misalnya pada repository yang hanya berisi Dockerfile. Build Dockerfile tidak memakai Railpack, jadi preview tetap selesai dengan `railpack: null`, dan output Railpack tetap ada di log. Tanpa Dockerfile, kegagalan Railpack tetap menggagalkan inspection.

## [0.4.0] - 2026-11-11

### Security

- Setiap project kini berjalan di bridge network sendiri, `sakala-project-<project-id>`, bukan di `sakala-runtime` bersama semua project. Sebelumnya aplikasi satu tenant bisa membuka koneksi langsung ke aplikasi tenant lain tanpa melewati Caddy. Agent menyambungkan container Caddy ke network project sebelum probe dan route, dan memutusnya serta menghapus network saat container terakhir project hilang. Saat startup dan setiap 60 detik, Agent menyambungkan ulang Caddy yang kehilangan network project, misalnya karena container-nya dibuat ulang, dan menghapus network kosong ketika tidak ada command aktif. Workload dari Agent lama tetap dilayani di `sakala-runtime` sampai redeploy berikutnya. Lihat [Docker Runtime](docs/DOCKER_RUNTIME.md#network-per-project).

Pool alamat bawaan Docker hanya cukup untuk sekitar 31 bridge network. Atur `default-address-pools` di `daemon.json` sebelum node menampung lebih dari sekitar 20 project; tanpa itu deploy gagal dengan `runtime_capacity_exceeded`.

### Fixed

- Aplikasi yang image-nya berakhir di `nginx` resmi, termasuk build multi-stage yang menyajikan file statis dengan `nginx:alpine`, kini bisa start. Sejak v0.3.0 container seperti itu keluar dengan exit code 1 dan `chown("/var/cache/nginx/client_temp", 101) failed (1: Operation not permitted)`, karena workload hanya mempertahankan `NET_BIND_SERVICE`. Workload kini mempertahankan allowlist `CHOWN`, `DAC_OVERRIDE`, `FOWNER`, `FSETID`, `KILL`, `SETGID`, `SETUID`, dan `NET_BIND_SERVICE` yang dibutuhkan image yang mulai sebagai root lalu turun ke user tanpa privilege. Worker httpd pun kini benar-benar turun ke `www-data`; sebelumnya `setgid` gagal dan request dilayani sebagai root. `NET_RAW`, `MKNOD`, `SYS_CHROOT`, `AUDIT_WRITE`, `SETFCAP`, dan `SETPCAP` tetap dibuang, dan `no-new-privileges` tetap dipasang. Pesan `runtime_app_exec_denied` tidak lagi menyebut `NET_BIND_SERVICE` sebagai satu-satunya capability. Lihat [Docker Runtime](docs/DOCKER_RUNTIME.md#capability-workload).
- Image project yang sudah dihapus tidak lagi tertinggal selamanya di node. Aturan GC per project selalu mempertahankan image aktif dan image sebelumnya, sedangkan project yang dihapus tidak punya container lagi, sehingga dua image terakhirnya (di staging 239 sampai 593 MB per image) tidak pernah terhapus dan disk terus terisi. Kini project tanpa container sama sekali kehilangan semua image yang lebih tua dari `SAKALA_ORPHANED_IMAGE_GRACE_SECONDS` (default 24 jam, minimal `SAKALA_COMMAND_TIMEOUT_SECONDS`), termasuk image terbarunya. Image yang dipakai container mana pun tetap dipertahankan.
- Image GC hanya memilih image yang semua namanya `sakala/project-<project_id>:<tag>`. Image berlabel Sakala yang di-tag ulang dengan nama lain, atau yang dangling, tidak lagi dihapus.

### Added

- Image GC kini berjalan sendiri setiap `SAKALA_IMAGE_GC_INTERVAL_SECONDS` (default satu jam) dengan aturan yang sama seperti target `stale_images` pada `CleanupRuntime`, dan dilewati selama ada command aktif. Sebelumnya image hanya dihapus lewat `CleanupRuntime` yang disetujui admin.
- Field opsional `remove_images` pada payload `StopProject`. Bila `true`, Agent menghapus semua image project setelah container-nya hilang, dan result menambah `removed_images` serta `reclaimed_image_bytes`. API perlu mengirimnya saat project dihapus; sampai itu terjadi, image GC yang menghapusnya. Lihat [Compatibility](docs/COMPATIBILITY.md#revision-4-additions-after-v030).
- Check preflight non-fatal `project-networks`, yang mencoba membuat network dan memperkirakan sisa address pool Docker.
- Smoke test opt-in `project_network_smoke_test` untuk isolasi network antar-project.

## [0.3.0] - 2026-11-11

### Security

- Sandbox Railpack tidak punya path host yang bisa ditulis. Hasilnya keluar lewat stdout, sehingga repository tidak bisa mengisi disk node atau meninggalkan file yang gagal dihapus Agent saat inspection. Pemeriksaan plan terhadap image `sakala/project-*` kini membaca JSON, sehingga escape tidak bisa menyembunyikan namanya.
- Railpack kini berjalan di container sekali pakai, tanpa socket Docker, secret, atau capability. Sebelumnya `railpack info` dan `prepare` berjalan di host, sehingga `mise.toml` di repository bisa mengeksekusi kode sebagai user Agent saat project dibuat, dan symlink file versi bisa membaca file host. `SAKALA_RAILPACK_IMAGE` kini wajib dan harus dipin dengan digest.
- Process anak tidak lagi mewarisi seluruh environment Agent, melainkan allowlist, sehingga token Agent tidak bisa bocor ke process anak.
- Build Dockerfile antar-project tidak lagi berbagi layer, cache mount, atau image lokal: `--no-cache`, `--pull`, namespace cache per project, dan frontend bawaan BuildKit yang tidak bisa diganti `# syntax=`. Build ulang naik dari sekitar 2,5 detik menjadi 15 detik pada aplikasi Node kecil. Frontend Railpack mengabaikan kedua flag itu, jadi isolasi build Railpack bersandar pada `secrets-hash`, `cache-key` per project, dan penolakan plan; lihat [Docker Runtime](docs/DOCKER_RUNTIME.md#isolasi-antar-project-saat-build).
- Satu baris output process dibatasi 64 KiB. Sebelumnya container yang mencetak tanpa newline bisa menghabiskan memory Agent.
- `bearer` yang diikuti spasi ganda atau delimiter tidak lagi membuat redactor panic, dan token di balik spasi ganda kini ikut diredaksi. Command yang task-nya panic kini dilaporkan `fail`, bukan menunggu lease habis.

Deploy `sakala-api` lebih dulu; lihat [Compatibility](docs/COMPATIBILITY.md). Container Caddy runtime kini wajib menyediakan `wget`, yang sudah ada pada image `caddy:*-alpine`.

### Added

- `SAKALA_CONTAINER_RUNTIME` menjalankan workload di runtime OCI yang terdaftar di Docker, misalnya `runsc` (gVisor, ADR-014), tanpa memengaruhi build dan sandbox Railpack. Runtime yang tidak terdaftar membuat preflight gagal fatal, dan Agent tidak pernah pindah ke runtime lain. Heartbeat menambah `runtime_dependencies.container_runtime` dan `container_runtime_limits_enforced`. Di Docker rootless, `runsc` butuh `--ignore-cgroups`, dan limit memory, CPU, serta PID tidak berlaku; lihat [Runtime Hardening](docs/RUNTIME_HARDENING.md#gvisor).
- `SAKALA_CONTAINER_DNS` memberi workload name server lewat `/etc/resolv.conf` read-only, karena DNS bawaan Docker tidak terjangkau dari `runsc` pada network buatan user.
- `error_code` baru pada `fail`: `runtime_app_not_listening`, `runtime_app_unhealthy`, `runtime_app_crashed`, `runtime_app_out_of_memory`, dan `runtime_port_conflict`, dengan pesan yang aman untuk user dan menyebut port, status, exit code, atau batas memory.
- `metadata.workloads.crashed_details` dan `metadata.detail_counts.crashed_details` pada heartbeat, batas 50 item, untuk workload yang sedang di-restart Docker atau yang crash berulang (minimal 3 restart dan hidup kurang dari 5 menit), dengan `restart_count` dan `running`. Crash satu kali yang pulih hanya dicatat pada log operator. Keduanya tidak dikirim bila state crash tidak diketahui.
- Field `error_code` pada completion `HealthCheck`, dan check preflight non-fatal `caddy-http-probe`.
- Output terakhir aplikasi dilaporkan melalui reporter sebelum candidate dihapus bila start, readiness, atau routing gagal, sehingga deploy gagal tidak lagi berakhir tanpa log.
- Smoke test opt-in `docker_smoke_test` yang menjalankan executor asli terhadap daemon Docker lokal.
- Event `deployment.analysis.started` (hanya Railpack), `deployment.health.started`, dan `deployment.route.started` pada saat tahapnya benar-benar dimulai.
- Redaksi nilai environment per command pada log, event, dan pesan `fail`, termasuk log follower dan follower yang dipulihkan setelah Agent restart. Nilai di bawah 8 karakter dan kata umum dikecualikan.
- Variabel project tersedia saat build sebagai secret BuildKit, otomatis pada Railpack dan lewat `RUN --mount=type=secret` pada Dockerfile, tanpa masuk layer atau history image. Lihat [Docker Runtime](docs/DOCKER_RUNTIME.md#variabel-saat-build).
- Pembatalan deployment lewat `cancel_requested` pada polling, dengan `fail` `runtime_cancelled`, atau `complete` bila route sudah mulai berpindah.
- Field `root_directory`, `build_command`, dan `start_command` pada `DeployProject`, serta `root_directory` opsional pada `InspectProject`. `root_directory` divalidasi ulang secara lokal, termasuk symlink yang keluar dari repository, dengan code baru `runtime_root_directory_invalid`.

### Changed

- Pada build Railpack, `deployment.build.started` kini terkirim setelah `railpack prepare`, bukan sebelumnya.
- `PORT` environment yang berbeda dari `container_port` kini gagal dengan `runtime_port_conflict`, bukan `invalid_runtime_command`.
- `RestartProject`, `WakeProject`, `RefreshRoute`, dan `restore_route` kini menunggu aplikasi menjawab HTTP hingga `SAKALA_START_TIMEOUT_SECONDS`. `health_attempts * health_interval` kini hanya mengatur lama tunggu `HealthCheck`.

### Fixed

- Pesan kegagalan proses kini menyebut `exit code 1`, bukan format debug Rust `status Some(1)`.
- Pembatalan saat aktivasi route tidak lagi bisa meninggalkan route yang menunjuk candidate yang sudah dihapus.
- `Dockerfile` berupa symlink ke luar checkout tidak lagi dibangun.
- Output proses yang bukan UTF-8 tidak lagi menggagalkan command dengan `runtime_filesystem_failed` dan menghilangkan baris berikutnya. Byte tersebut kini menjadi karakter pengganti.
- Aplikasi Laravel hasil Railpack kini bisa start. Sebelumnya semuanya keluar dengan exit code 126 dan `exec: frankenphp: Operation not permitted`, karena biner FrankenPHP bertanda file capability `cap_net_bind_service=ep` dan kernel menolak exec biner seperti itu bila capability-nya tidak ada di bounding set, yang dikosongkan `--cap-drop ALL`. Workload kini mempertahankan `NET_BIND_SERVICE` saja; `no-new-privileges` tetap dipasang. Lihat [Docker Runtime](docs/DOCKER_RUNTIME.md#capability-workload).
- GC image kini benar-benar berjalan. Format `{{.Label ...}}` pada `docker image ls` ditolak Docker sehingga deteksi selalu gagal, dan `docker image prune` tanpa `-a` memang tidak pernah menyentuh image deployment, yang selalu punya tag. Per project, `CleanupRuntime` kini menghapus image lama dan mempertahankan image yang dipakai container, image deployment sebelumnya, serta image yang lebih muda dari `SAKALA_IMAGE_GC_MAX_AGE_SECONDS`. Result menambah `removed_images`, dan `reclaimed_image_bytes` kini batas atas.
- Kegagalan registry atau jaringan saat build menarik image kini dilaporkan sebagai `runtime_registry_unavailable`, bukan `runtime_build_failed` yang menyuruh user memperbaiki repository.
- Program yang ditolak kernel saat start kini dilaporkan sebagai `runtime_app_exec_denied` dengan pesan yang menyebut capability, bukan `runtime_app_crashed` yang hint API-nya menyalahkan environment variable.
- Log deployment tidak lagi memuat output `caddy validate` dan `caddy reload`, yang berisi path config router bersama dan peringatan untuk site tenant lain, maupun path workspace di node, yang sebelumnya tercetak oleh `git init` di setiap checkout dan oleh peringatan cleanup workspace. Detail itu kini masuk log Agent.
- Deployment yang aplikasinya rusak tidak lagi dilaporkan berhasil. Readiness sebelumnya lolos begitu Docker menyebut container `running`, termasuk bila aplikasi salah port atau crash setelah start. Kini route baru aktif setelah probe `wget` dari dalam container Caddy menerima status di bawah 500 dari aplikasi, dalam deadline `start_timeout_seconds`. Redirect tidak pernah keluar dari aplikasi, dan crash menghentikan penantian lebih awal.

## [0.2.3] - 2026-09-26

### Fixed

- Baris output proses dipecah pada carriage return sebelum dilaporkan, dan CR/LF yang tersisa dinetralkan di reporter. Build tooling menggambar ulang progress dengan `\r`, sehingga satu baris output dapat memuat beberapa frame; `sakala-api` mewajibkan satu message satu baris dan menolak seluruh batch dengan `422 The logs.N.message field format is invalid`.
- Kegagalan pengiriman log tidak lagi menggagalkan command. Sebelumnya satu batch log yang ditolak membuat deployment staging yang build-nya sudah berjalan berakhir `runtime_reporting_failed`; log adalah telemetri dan tidak menentukan apakah pekerjaannya berhasil. Penghentian delivery tetap dicatat pada log agent.

## [0.2.2] - 2026-09-24

### Fixed

- Nama container managed dipotong agar tetap di bawah batas 63 oktet satu label DNS (`sakala-app-<project8>-<deployment-id>`). Nama sebelumnya berisi dua UUID penuh (84 oktet); Docker menerimanya sebagai nama container tetapi DNS internalnya membalas `Message too large`, sehingga setiap route runtime gagal resolve dan edge membalas `502`.
- Agent mengekspor `PORT=container_port` ke env container bila deployment tidak menyetelnya, karena image Railpack menyajikan aplikasi pada `$PORT` sementara route dibuat dari port pilihan control plane. `PORT` payload yang berbeda dari `container_port` kini ditolak sebagai command tidak valid.

## [0.2.1] - 2026-09-20

### Fixed

- `metadata.startup_reconciliation.captured_at` pada heartbeat kini diserialisasi sebagai string RFC 3339. Sebelumnya nilai `OffsetDateTime` di dalam `json!` metadata terkirim sebagai tuple `[tahun, hari-dalam-tahun, ...]`, sehingga `sakala-api` menolak setiap heartbeat node connected (driver `noop` maupun `docker`) dengan `422` dan node tidak pernah menjadi `ready`. Test heartbeat kini memverifikasi seluruh field `*_at` pada payload yang diserialisasi sebagai string RFC 3339.

## [0.2.0] - 2026-09-20

### Added

- `metadata.detail_counts` pada heartbeat dan batas 50 item untuk collection detail (`unhealthy_details`, `recovered_workloads`, `orphans`, `stale_routes`, `stale_images`, `compatibility_issues`) agar payload tetap di bawah batas 256 KiB API (#48).
- Report event/log dikirim sebagai batch `{ "events": [...] }` / `{ "logs": [...] }` dengan header `Idempotency-Key` per request; log di-buffer dan di-flush per 100 baris/512 KiB/200 ms serta sebelum `complete`/`fail` (#49).
- Retry backoff terbatas untuk report, `complete`, dan `fail` pada kegagalan transport dan `408`/`429`/`5xx`; `RetryPolicy` pada `ApiClient` (#49).
- `stale_routes[].deployment_id` pada heartbeat `startup_reconciliation` (#49).

### Changed

- `ReconcileWorkload` aksi `restart_log_follower` memasang follower di bawah identitas `DeployProject` asli milik workload (label command-id dan log bounds container) melalui `RuntimeReporterFactory`, bukan command reconcile yang memerintahkannya; workload tanpa label command-id ditolak (#49).
- Respons `200` report wajib membawa acknowledgement yang valid dengan `accepted_count` sama dengan jumlah item batch; body yang tidak dapat dibaca di-retry dengan `Idempotency-Key` yang sama, body yang tidak sesuai kontrak atau parsial dianggap tidak terkirim (#49).
- Update transitive `rustls` ke `0.23.45` untuk `RUSTSEC-2026-0285`.
- Respons `409`/`422`/`413` pada report menghentikan delivery log command tersebut tanpa retry sehingga follower berhenti setelah lease expired atau budget log habis; `terminal_at` pada `409` disertakan dalam pesan konflik terminal (#49).
- Body `fail` disanitasi sesuai batas control plane (`error_code` `[A-Za-z0-9._-]` ≤ 64, `error_message` tanpa control/bidi/zero-width ≤ 1000) (#49).
- Dokumentasi `AGENT_API.md`: bagian polling/claim diisi, shape completion `ReconcileWorkload` disamakan dengan kode, semantik lease/offline/pinning/`Claimed -> Running`, log setelah `complete`, dan catatan `NodeStatus::busy`; keputusan adopsi dicatat di `COMPATIBILITY.md` (#49).

## [0.1.0] - 2026-08-24

### Added

- Protocol revision 4 dengan bootstrap desired lifecycle node dari control plane sebelum scheduler mengklaim command.
- Dokumentasi matriks operating mode, ownership command melalui atomic claim, dan batas topologi Caddy container/host.
- Docker runtime executor untuk checkout immutable GitHub commit, Buildx image build, candidate container, health check, Caddy route activation, dan cleanup deployment lama.
- Dockerfile-first builder selection dengan Railpack fallback melalui version-pinned BuildKit frontend.
- Command `InspectProject` untuk preview repository melalui `railpack info`, scanner metadata ringan, dan typed completion result.
- Per-line subprocess log streaming, bounded output capture, runtime resource limits, dan temporary mode-0600 environment files.
- Typed deployment resource request, node safety defaults/ceilings, dan requested/applied resource reporting.
- Per-repository task tracking untuk connected agent, real runtime, dan hardening berikutnya.
- Wiremock coverage for connected heartbeat and successful/failed command reporting lifecycles.
- Private repository checkout dengan temporary credential in-memory, credential-free remote URL, dan `GIT_ASKPASS` owner-only.
- Bounded scheduler untuk command lintas project serta batas build image Docker/Railpack.
- Cancellation end-to-end sampai process group, cleanup candidate/workspace, dan graceful shutdown deadline.
- Docker preflight, label workload canonical, heartbeat protocol revision, serta GC workspace UUID yang konservatif.
- Cargo workspace dengan binary agent serta crate protocol, core, dan runtime.
- Safe local mode dengan heartbeat/polling log dan graceful shutdown.
- Control-plane API client skeleton untuk connected mode.
- `NoopRuntimeExecutor` sebagai runtime driver default tanpa host mutation.
- Protocol types, log redaction, integration tests, CI, dan dokumentasi awal.
- Protocol revision 3 untuk recovery log follower, explicit workload reconciliation actions, dan approval-gated Sakala-only runtime cleanup.
- Restart-in-flight serta repeated redeploy soak coverage untuk memory, process cleanup, workspace/container/image/route retention, follower deduplication, dan API retry pacing.

### Changed

- Route Caddy membawa deployment identity, host telemetry berpindah dari core ke runtime adapter, dan snapshot reconciliation heartbeat diberi nama serta timestamp startup yang eksplisit.
- Recovery menoleransi metadata container legacy per workload, stale route hanya mempertahankan workload running, dan semaphore build dilepas sebelum fase start/readiness.
- Heartbeat meng-cache versi dependency dan membatasi durasi probe subprocess/runtime.
- Refactor runtime crate dari flat modules menjadi executor, workspace, builders, containers, routing, health, logs, dan process boundaries dengan dependency injection.
- Pisahkan protocol sebagai DTO-only, core sebagai command lifecycle/application ports, runtime sebagai adapter implementation, dan binary sebagai composition root.
- Pisahkan Caddy file route transaction dari transport reload `docker exec`, sehingga lokasi proses Caddy dapat diganti tanpa mengubah deploy orchestration.
- Align heartbeat, command polling, event, log, and failure payloads with the `sakala-api` resource and persistence conventions.
- Integrasi connected mode diarahkan ke `sakala-api` melalui `SAKALA_API_URL` dan modul `api`.
- Update transitive `quinn-proto` dependency to `0.11.15` to address `RUSTSEC-2026-0185`.
- Update transitive `h2` dependency to `0.4.16` to address `RUSTSEC-2026-0258`.
- Container runtime menyimpan command identity dan bounded-log policy sebagai label agar execution bookkeeping dan follower dapat dipulihkan setelah Agent restart.

### Fixed

- Propagasikan kegagalan cleanup container deployment sebelumnya setelah ready/follower agar completion meminta deferred control-plane repair.
- Tandai completion deployment yang melewati post-commit finalization grace sebagai `finalization_deferred` agar control plane dapat menghentikan superseded workload secara eksplisit.
- Batasi finalisasi deployment setelah cutover dengan grace 30 detik dan pisahkan cached dependency versions dari live readiness Docker, Caddy, network, serta workspace.
- Jadikan route cutover sebagai deployment commit point, lindungi modern route dari stale legacy cleanup, pertahankan partial telemetry, dan laporkan node aktif yang tidak operasional sebagai degraded.
- Tutup race concurrent container admission dengan authoritative pre-run check, lindungi route deployment baru dari lifecycle command lama, dan terima output decimal Docker image prune tanpa mengubah cleanup sukses menjadi gagal.
- Checkout Git mengambil commit sebelum checkout, redeploy menghentikan container lama yang masih running, serta semantik missing workload untuk Stop/Sleep dibedakan.

[Unreleased]: https://github.com/gmedia/sakala-agent/compare/v0.5.0...HEAD
[0.5.0]: https://github.com/gmedia/sakala-agent/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/gmedia/sakala-agent/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/gmedia/sakala-agent/compare/v0.2.3...v0.3.0
[0.2.3]: https://github.com/gmedia/sakala-agent/compare/v0.2.2...v0.2.3
[0.2.2]: https://github.com/gmedia/sakala-agent/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/gmedia/sakala-agent/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/gmedia/sakala-agent/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/gmedia/sakala-agent/releases/tag/v0.1.0
