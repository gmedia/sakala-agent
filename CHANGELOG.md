# Changelog

Semua perubahan penting pada project ini akan dicatat di sini. Format mengikuti Keep a Changelog dan release mendatang akan menggunakan Semantic Versioning.

## [Unreleased]

### Fixed

- Event deployment yang tidak terkirim tidak lagi menggagalkan deployment. Sebelumnya `reporter.event` menyerah setelah 3 percobaan dalam sekitar 1,5 detik, dan error-nya menggagalkan langkah yang mengirim event itu, jadi outage API yang jatuh tepat saat pergantian tahap (seperti 77 detik di staging 2026-11-19) bisa menggagalkan deploy yang sebenarnya berjalan baik (ledger X-14). Kini setiap event memakai satu `Idempotency-Key` dan body yang sama di setiap percobaan, kegagalan transient (`408`, `429`, `5xx`, koneksi, timeout, body tidak terbaca) di-retry dengan backoff eksponensial 1 sampai 15 detik plus jitter (`backon`) dan tidak kurang dari `Retry-After`, selama paling lama 90 detik per event. Event yang tetap tidak di-ack, atau ditolak `413`, `422`, `4xx` lain, atau `200` tanpa acknowledgement valid, dibuang dengan warning `deployment event was not delivered` dan dihitung; sebelum `complete`/`fail` Agent mencatat totalnya (`deployment events could not be fully delivered`). Setelah satu event dibuang, event berikutnya command itu hanya dicoba sekali sampai ada yang terkirim lagi, supaya outage panjang hanya menahan command satu budget. `command.claimed` ikut aturan ini, dan batas waktu eksekusi command kini dihitung sejak awal pemrosesan (claim), bukan setelah event itu, supaya penantiannya tidak membuat lease API habis. `409` tetap error karena API tidak lagi menerima report command itu, dan `complete`/`fail` tidak berubah. Tanpa perubahan wire maupun API: API sudah men-dedupe event per key dan hash payload. Lihat [Event and Log Payloads](docs/AGENT_API.md#event-and-log-payloads) dan [Compatibility](docs/COMPATIBILITY.md#revision-4-behaviour-change-event-delivery-retries).

## [0.16.1] - 2026-11-23

### Fixed

- Image yang dibangun Railpack kini membawa label Sakala. Frontend BuildKit Railpack mengabaikan `--label` (diperiksa di source v0.23.0 yang dipin dan di `main` per 12 November 2026), sehingga di staging dengan Agent v0.16.0 setiap image Railpack punya label `null`: redeploy tanpa build dan rollback selalu jatuh ke build dengan `image_not_owned` walaupun Console menjanjikan "tidak perlu build", tag `previous` tidak pernah dibuat untuk project Railpack, dan image GC tidak pernah menyentuh image itu. Build Railpack kini diberi nama sementara `<image>-unlabelled`, lalu langkah baru `railpack-label` membuat container yang tidak pernah dijalankan (`docker container create --pull never --network none`, tanpa label Sakala) dan menulis semua label lewat `docker container commit --change 'LABEL ...'` ke nama image deployment, lalu menghapus container dan nama sementara. Commit ini tidak menambah layer dan tidak mengubah CMD, ENTRYPOINT, ENV, WORKDIR, USER, EXPOSE, atau VOLUME. Cara ini dipilih daripada build `FROM <image lokal>` karena hanya bergantung pada daemon: dengan builder buildx `docker-container`, image lokal tidak terlihat dan BuildKit akan menarik `docker.io/sakala/project-...`. Kegagalan langkah ini menggagalkan build dengan `runtime_build_failed`, karena image tanpa label diam-diam mematikan reuse, rollback, dan GC. Smoke test baru `railpack_labels_smoke_test` membuktikannya dengan build Railpack sungguhan (Docker 29.10, containerd image store): label lengkap, config sama persis selain label, image pertama menjadi `previous`, dan rollback ke image itu tanpa build. Lihat [Label image Railpack](docs/DOCKER_RUNTIME.md#label-image-railpack).
- Image GC kini juga membersihkan image Railpack lama yang tidak berlabel. Image tanpa label `dev.sakala.project-id` sama sekali yang semua namanya `sakala/project-<project>:<tag>` untuk satu project dianggap milik project itu dan mengikuti aturan simpan yang sama: image yang dipakai container (berjalan, berhenti, atau tidur) dan image previous tetap disimpan, image project tanpa container dihapus setelah `SAKALA_ORPHANED_IMAGE_GRACE_SECONDS`. Nama image dibaca dari `docker image ls` tanpa filter, karena filter `reference` menyembunyikan nama lain sebuah image; image dengan satu saja nama di luar repository project itu, image berlabel project yang tidak valid, dan image dangling tidak pernah dipilih. Label image kini dibaca dari `{{json .Config}}`, karena dengan beberapa image dalam satu `docker image inspect`, Docker 29.10 menggagalkan seluruh perintah (`map has no entry for key "Labels"`) bila ada image tanpa label sama sekali; tanpa perubahan ini satu image Railpack lama saja menggagalkan setiap putaran GC. Image seperti ini tidak pernah diberi tag `previous` dan tidak pernah dipakai ulang. Di staging sekitar 37 image seperti itu, termasuk milik project yang sudah dihapus, menumpuk di runtime node. Lihat [Image dan GC](docs/DOCKER_RUNTIME.md#image-dan-gc).

## [0.16.0] - 2026-11-23

### Added

- Konfigurasi opsional `SAKALA_PUBLIC_CONSOLE_URL`. Bila diisi, halaman project yang tidur dan halaman tidak tersedia menautkan pemilik project ke Console; tanpa itu kalimatnya tampil tanpa tautan. Nilai yang tidak valid membuat Agent berhenti saat start. Lihat [Configuration](docs/CONFIGURATION.md).
- Rollback tanpa build. Capability node baru `image-rollback`, diiklankan hanya oleh driver `docker`, menjanjikan bahwa per project node menyimpan image yang sedang melayani (*live*) dan image workload yang digantikan deployment sukses terakhir (*previous*). Setelah `DeployProject` sukses, masih di bawah lock project, Agent memberi image yang digantikan tag kedua `sakala/project-<project>:previous` (hanya bila berbeda dari image live baru, jadi redeploy konfigurasi tidak menggesernya, dan hanya bila semua nama image itu ada di repository project dan label `dev.sakala.project-id`-nya cocok), lalu menghapus image lain project itu yang tidak dipakai container. Rollback memakai mekanisme yang sudah ada: API mengirim commit deployment tujuan dan `reuse_image_from_deployment_id` deployment yang membangun image-nya; image yang sudah hilang tetap jatuh ke build dengan `image_missing`. Deployment gagal tidak pernah menggeser previous. Kegagalan tag atau penghapusan tidak menggagalkan deployment: Agent mengirim event baru `deployment.images.retained` (level `warning`, metadata `project_id`) dan image GC berkala merapikannya. Uji Docker nyata (`image_rollback_smoke_test`) membuktikan setelah deploy A, B, C hanya C dan B yang tersisa, rollback ke B tanpa build membuat B live dan C previous, dan rollback ke A build ulang. Aditif di protocol revision 4; lihat [Rollback dan image sebelumnya](docs/AGENT_API.md#rollback-dan-image-sebelumnya) dan [Compatibility](docs/COMPATIBILITY.md#revision-4-additions-for-image-rollback).

### Changed

- **Perubahan perilaku image GC.** Untuk project yang masih punya container, image GC kini hanya menyimpan image yang dipakai container (dalam state apa pun, termasuk container yang tidur dan semua replika) dan image bertag `previous`; tanpa tag itu (image dari Agent lama) tetap image terbaru yang lebih tua dari image aktif terbaru. Umur tidak lagi melindungi image: sebelumnya setiap image yang lebih muda dari tujuh hari ikut disimpan. `SAKALA_IMAGE_GC_MAX_AGE_SECONDS` kini usang dan diabaikan; nilainya masih dibaca dan diperiksa supaya konfigurasi lama tetap dimuat, dan Agent menulis peringatan saat start. Supaya image yang baru dibangun tetap aman tanpa perlindungan umur, GC (berkala maupun `CleanupRuntime`) hanya menghapus image sebuah project sambil memegang lock project yang sama dengan command, melewati project yang sedang dipegang command, dan memilih ulang image project itu setelah lock didapat. Image dengan dua tag dihapus per nama dengan tag `previous` lebih dulu, karena Docker dengan containerd image store menolak `docker image rm <id>` untuk image bertag lebih dari satu. Project tanpa container tetap memakai `SAKALA_ORPHANED_IMAGE_GRACE_SECONDS`. Putaran GC pertama setelah upgrade pada project yang punya banyak image dalam tujuh hari terakhir menghapus semuanya kecuali image live dan satu image sebelumnya. Lihat [Image dan GC](docs/DOCKER_RUNTIME.md#image-dan-gc).
- `RestartProject` kini juga menyalakan ulang deployment yang semua container-nya exited atau sedang crash loop, setelah pemeriksaan kapasitas seperti wake. Sebelumnya command itu ditolak dengan `runtime_workload_not_running` bila tidak ada replika yang berjalan, sehingga project yang crash tidak bisa di-restart. Deployment yang sedang tidur, yaitu yang route project-nya halaman tidur untuk deployment itu, tetap ditolak. Lihat [Workload Lifecycle Commands](docs/AGENT_API.md#workload-lifecycle-commands).
- Halaman di domain project yang tidur ditulis ulang untuk pengunjung. Halaman tunggu kini berjudul "Aplikasi ini sedang dinyalakan", menyebut bahwa aplikasi biasanya siap dalam kurang dari satu menit (perkiraan yang sama dengan Console, dari wake tercatat 3 sampai 11 detik di production dan 5 sampai 35 detik di staging), menampilkan garis progres yang diam saat `prefers-reduced-motion: reduce`, dan memakai simbol Sakala dengan teks "Di-host di Sakala". Halaman tidak tersedia kini menunjukkan ke pemilik project di mana alasannya bisa dilihat. Kedua halaman mengirim `Content-Security-Policy` yang hanya mengizinkan blok inline-nya sendiri (hash SHA-256) dan origin endpoint wake, dan tetap tidak memuat apa pun dari luar. Tanpa JavaScript, halaman tunggu menampilkan tombol "Nyalakan aplikasi" yang mengirim permintaan wake lewat form biasa. Lihat [Caddy Integration](docs/CADDY_INTEGRATION.md).

- Versi Rust minimum (`rust-version`) naik dari 1.88 ke 1.89. `uuid` 1.29.2, yang masuk `Cargo.lock` lewat update Dependabot #68 (2026-11-11) dan ikut di setiap rilis sejak v0.5.0, membutuhkan rustc 1.89.0, sehingga `cargo +1.88.0 check` gagal dengan `uuid@1.29.2 requires rustc 1.89.0` dan nilai 1.88 tidak lagi benar. CI kini punya job `msrv` yang menjalankan `cargo +<rust-version> check --workspace --all-targets --all-features --locked` dengan versi yang dibaca dari `Cargo.toml`, supaya update dependency berikutnya tidak menaikkan MSRV diam-diam. Workflow release tidak lagi meminta toolchain 1.88.0: `rust-toolchain.toml` (`stable`) selalu mengalahkan input itu, jadi binary rilis memang dibangun dengan `stable`, seperti di CI. Lihat [Development Workflow](docs/DEVELOPMENT.md#versi-rust-minimum).

### Fixed

- Membuka URL project yang tidur kini tetap membangunkannya walaupun permintaan wake pertama hilang. Halaman tunggu dulu hanya mengirim satu permintaan per kunjungan dan tidak bisa membaca jawabannya (`no-cors`), jadi bila permintaan itu gagal, misalnya karena jaringan pengunjung berganti atau API sedang sibuk, halaman memuat ulang selama 2 menit tanpa ada yang membangunkan project, lalu menampilkan "belum bisa dinyalakan". Ini cocok dengan laporan 19 November 2026 di staging: URL tidak membangunkan dua project, sementara tombol Bangunkan di Console berhasil, dan tabel command API tidak mencatat satu pun wake publik untuk keduanya. Halaman kini mengirim ulang setiap 15 detik selama menunggu; API hanya menyimpan satu wake per project, dan satu tab tetap di bawah batas 6 permintaan per menit. Diuji di Chromium terhadap Caddy sungguhan dengan permintaan pertama yang sengaja diputus: versi lama berhenti setelah 135 detik tanpa bangun, versi baru membuka aplikasi dalam 24,8 detik.

- Container project yang sedang tidur tidak lagi dilaporkan sebagai orphan. Sleep otomatis menghentikan container deployment aktif dan menyimpannya untuk dibangunkan, tetapi rekonsiliasi saat Agent start menggolongkan setiap container `Exited`/`Dead` sebagai `stale stopped deployment container`, sehingga setelah restart Agent v0.15.0 di production menulis peringatan `orphaned Sakala container detected` untuk enam container milik project yang tidur. Agent kini membaca route tidur di direktori site Caddy, satu-satunya catatan lokal yang ditulis saat sleep: container berhenti yang project dan deployment-nya sama dengan route berakhiran `(sleeping)` dilaporkan di `sleeping_workloads`, bukan `orphans`. Deployment lama project yang sama dan container tanpa route tetap orphan. `workloads.stopped` tetap menghitung keduanya, dan route tidur tetap dianggap dikenal sehingga cleanup route tidak menghapusnya. Aditif di protocol revision 4; lihat [Sakala Agent API](docs/AGENT_API.md#heartbeat-payload) dan [Compatibility](docs/COMPATIBILITY.md#revision-4-additions-for-sleeping-workloads).

### Security

- Update `url` to 2.5.9 for RUSTSEC-2026-0592 (stack overflow in `Url::origin()` on nested `blob:` URLs).

### Documentation

- Document that tenant dumps are restored with an unprivileged role, never as superuser (`docs/DATABASE_HOST.md`).

## [0.15.0] - 2026-11-20

### Added

- Hasil `InspectProject` kini selalu membawa dua field baru untuk saran variabel dan database di wizard Console. `env_example` berisi variabel dari `.env.example`, `.env.sample`, atau `.env.dist` pertama yang ada di root directory project (folder package pada monorepo): paling banyak 200 entri `{ key, example_value }` dan `truncated`, atau `null` bila tidak ada file, file itu symlink yang keluar dari direktori, bukan file biasa, atau lebih dari 64 KiB. `.env` tidak pernah dibuka. Nilai kosong, multi-baris, atau mirip credential (blok PEM, URL dengan password di userinfo) dilaporkan `null`, dan nilai dipotong di 200 karakter. Parsernya ditulis sendiri tanpa interpolasi, karena `dotenvy` 0.15.7 mengisi `${VAR}` dari environment process Agent tanpa opsi untuk mematikannya. `database_hint` berisi `likely`, `engine` (`postgresql`, `mysql`, `sqlite`, `unknown`), paling banyak 16 `signals` (variabel database di file contoh, migrasi Laravel, schema Prisma, settings Django, `config/database.yml` Rails, dan nama package database di `package.json`, `composer.json`, `requirements.txt`, `pyproject.toml`, `Gemfile`), serta `suggested_release_command` yang hanya bisa bernilai `php artisan migrate --force`, `python manage.py migrate`, `npx prisma migrate deploy`, atau `bin/rails db:migrate`. Semua file dibaca dengan batas yang sama seperti `Dockerfile` dan tidak ada yang dijalankan. Uji dengan `.env.example` dan layout asli contoh Laravel gmedia/sakala-examples menghasilkan 11 variabel tanpa baris `DB_*` yang dikomentari, sinyal `laravel_migrations`, dan `php artisan migrate --force`. Pembacaan file aman yang sebelumnya terpisah di `js_workspace` dan inspection kini memakai satu modul `repository_file`. Aditif di protocol revision 4; lihat [Sakala Agent API](docs/AGENT_API.md#variabel-dari-envexample) dan [Compatibility](docs/COMPATIBILITY.md#revision-4-additions-for-env-and-database-suggestions).
- Hasil `InspectProject` kini selalu membawa field `frameworks`, daftar framework JavaScript yang bisa merender halaman saat build, supaya API tahu perubahan variabel di project seperti itu butuh build baru. Railpack v0.23.0 melaporkan Gatsby, Astro mode server, Eleventy, Docusaurus, VitePress, dan SvelteKit tanpa config Vite sebagai `node` biasa, sehingga perubahan variabel di sana memakai ulang image dan menyajikan nilai lama yang tertanam saat build. Nilainya himpunan tertutup `astro`, `docusaurus`, `eleventy`, `gatsby`, `next`, `nuxt`, `react-router`, `remix`, `sveltekit`, `tanstack-start`, `vitepress`, terurut dan tanpa duplikat, dari nama package persis di `dependencies` dan `devDependencies` `package.json` direktori yang diinspeksi (folder package pada monorepo). `react-router` dan `@remix-run/react` saja tidak dihitung; yang dihitung `@react-router/dev` dan `@remix-run/dev`. File dibaca dengan batas yang sama seperti `database_hint`, dan file yang tidak ada, terlalu besar, symlink keluar direktori, atau JSON tidak valid menghasilkan `[]`. Pembacaan dependency `package.json` dan `composer.json` kini memakai satu modul bersama untuk `database_hint` dan `frameworks`. Aditif di protocol revision 4; lihat [Sakala Agent API](docs/AGENT_API.md#framework-yang-merender-saat-build) dan [Compatibility](docs/COMPATIBILITY.md#revision-4-additions-for-framework-hints).

## [0.14.0] - 2026-11-19

### Added

- Redeploy tanpa build ulang. Payload `DeployProject` boleh membawa `reuse_image_from_deployment_id` (string UUID deployment yang membangun image-nya), dan Agent lalu menyalakan deployment dari image itu dengan environment baru, tanpa checkout dan tanpa build. Gunanya untuk redeploy yang hanya mengubah konfigurasi pada commit yang sama: di staging, rotasi password database membuat aplikasi 503 selama 6 menit 56 detik, 6 menit 17 detik di antaranya build. Agent menghitung referensi image sendiri dari project yang meminta, `sakala/project-<project_id>:<commit12>-<source8>`, tidak pernah menerima referensi dari payload, dan memakainya hanya bila label `dev.sakala.project-id` dan `dev.sakala.deployment-id` image itu cocok dan `dev.sakala.builder` bernilai `dockerfile` atau `railpack`. Setiap image yang dibangun kini membawa label baru `dev.sakala.builder` dan, bila dibangun dari root workspace JavaScript, `dev.sakala.package-directory`, sehingga `start_command` dan `release_command` berperilaku persis seperti pada build aslinya. Release command tetap dijalankan; pemeriksaan kapasitas, readiness, replika, dan perpindahan route tidak berubah; pemeriksaan disk workspace dan antrean build dilewati. Event baru `deployment.image.reused` (level `info`, metadata `image`, `source_deployment_id`, `builder`) menggantikan event checkout, analisis, dan build. Bila image tidak bisa dipakai, Agent mengirim `deployment.image.reuse_skipped` (level `warning`, metadata `source_deployment_id` dan `reason`: `image_missing`, `image_not_owned`, `image_metadata_missing`, atau `invalid_reference`) lalu build seperti biasa; ini bukan kegagalan. Field yang bukan UUID tidak pernah menolak payload. Deployment reuse yang gagal tidak pernah menghapus image yang dipakainya, karena image itu milik deployment yang sedang melayani. Image GC tidak berubah. Image dari Agent sebelumnya belum punya label builder, jadi redeploy konfigurasi pertama setelah upgrade build satu kali. Uji Docker nyata (`image_reuse_smoke_test`) membuktikan redeploy reuse menyajikan nilai variabel baru tanpa build, image yang tidak ada jatuh ke build dengan `image_missing`, dan image project lain yang di-tag ke repository sebuah project ditolak dengan `image_not_owned` dan tidak pernah dijalankan. Aditif di protocol revision 4; lihat [Memakai ulang image](docs/DOCKER_RUNTIME.md#memakai-ulang-image) dan [Compatibility](docs/COMPATIBILITY.md#revision-4-additions-for-image-reuse).
- Capability node `image-reuse`, diiklankan hanya oleh driver `docker`. API hanya boleh mengirim `reuse_image_from_deployment_id` ke node yang mengiklankannya; Agent lama mengabaikan field itu dan build.

### Fixed

- Log deployment tidak lagi hilang permanen saat API sebentar tidak terjangkau. Pada staging 2026-11-19 17.53 sampai 17.54 WIB, pengiriman log gagal dengan `error sending request` selama sekitar 80 detik; reporter berhenti setelah tiga percobaan (`deployment log delivery stopped`, 49 dan 116 baris dibuang), dan deployment yang sukses berakhir dengan 50 baris tanpa satu pun baris `[release]`. Kini `log` hanya memasukkan baris ke buffer dan tidak pernah menunggu jaringan, dan satu worker per command mengirim batch berurutan. Kegagalan transport, `408`, `429`, dan `5xx` di-retry dengan `Idempotency-Key` dan isi batch yang sama (backoff 1 sampai 15 detik dengan jitter, tidak kurang dari `Retry-After`) selama command berjalan, sehingga API tidak menyimpan baris dua kali dan urutannya tetap. Sebelum `complete`/`fail`, Agent menunggu buffer kosong paling lama 120 detik tanpa melewati batas waktu command. Buffer dibatasi 10.000 baris dan 2 MiB per command; bila penuh, baris tertua dibuang dan timeline mendapat satu baris `system` yang menyebut jumlah baris yang hilang. Warning `deployment logs could not be fully delivered` kini menyebut jumlah baris yang benar-benar hilang. `409`, `413`, `422`, `4xx` lain, dan acknowledgement yang tidak valid tetap menghentikan delivery tanpa retry. Tidak ada perubahan wire; lihat [Logging](docs/LOGGING.md#foundation-behavior) dan [Compatibility](docs/COMPATIBILITY.md#revision-4-behaviour-change-log-delivery-retries).
- Dependency baru `backon` (backoff eksponensial dengan jitter, dipakai untuk menghitung jeda retry) dan `httpdate` (membaca `Retry-After` berbentuk tanggal HTTP) di `sakala-agent-core`.

- Aplikasi kini menerima alamat pengunjung yang sebenarnya. Setiap route project memuat `header_up X-Forwarded-For {client_ip}` dan `header_up X-Real-IP {client_ip}`, jadi kedua header berisi tepat satu alamat: pengunjung untuk request dari Caddy host atau Sakala Edge, atau peer itu sendiri untuk koneksi lain seperti workload yang memanggil router langsung. Pada staging 2026-11-19, aplikasi di balik HTTPS menerima `X-Forwarded-Proto: http` dan `X-Forwarded-For` berisi gateway network project, sehingga Laravel dengan `trustProxies(at: '*')` membuat URL `http://`; `X-Real-IP` kiriman pengunjung juga sampai ke aplikasi apa adanya. Perbaikan ini bergantung pada `servers { trusted_proxies ... }` di Caddyfile runtime node (sakala-deployment), yang hanya mempercayai alamat asal Edge; tanpa blok itu `{client_ip}` adalah alamat Edge. Uji Docker nyata dengan `caddy:2.11.3-alpine` (`client_ip_smoke_test`) membuktikan alamat dari Edge dipakai, rantai yang ditulis pengunjung dilewati, dan header palsu dari workload tidak dipercaya. Lihat [Alamat klien dan header forwarded](docs/CADDY_INTEGRATION.md#alamat-klien-dan-header-forwarded).

## [0.13.0] - 2026-11-19

### Added

- Root directory yang merupakan package workspace JavaScript kini bisa dideploy tanpa perintah khusus (gmedia/sakala-agent#74). Pada uji production 2026-11-19, `root_directory: apps/web` di monorepo pnpm gagal dengan `sh: 1: pnpm: not found`, karena lockfile, `pnpm-workspace.yaml`, dan `packageManager` hanya ada di root repository. Bila root directory adalah package workspace pnpm, npm, Yarn, atau Bun yang root-nya ada di atasnya (`packages` di `pnpm-workspace.yaml` atau `workspaces` di `package.json`, workspace terdekat yang memutuskan), build Railpack kini berjalan dari root workspace dan perintahnya diarahkan ke package: `pnpm --filter '<name>...' run build` pada pnpm sehingga package workspace yang dipakainya ikut dibangun, `cd '<package>' && <pm> run build` pada manager lain, `cd '<package>' && <pm> run start` bila ada script `start`, dan SPA Vite tanpa script `start` disajikan dengan Caddy milik Railpack lewat `RAILPACK_SPA_OUTPUT_DIR`. `build_command`, `start_command`, dan `release_command` project dijalankan dengan `cd '<package>' && ...`, jadi `pnpm run build` di root directory `apps/web` berjalan di `apps/web` dengan pnpm tersedia. Variabel project `RAILPACK_BUILD_CMD`, `RAILPACK_START_CMD`, dan `RAILPACK_SPA_OUTPUT_DIR` tetap menang atas perintah turunan. Build Dockerfile dan repository tanpa workspace tidak berubah. Deployment seperti ini mengirim event baru `deployment.workspace.detected`, dan hasil `InspectProject` selalu membawa field baru `workspace` (`null` tanpa workspace). Uji Docker nyata dengan fixture monorepo pnpm di `tests/fixtures/pnpm-workspace` membuktikan `apps/web` dibangun dan disajikan serta `apps/api` berjalan. Aditif di protocol revision 4; lihat [Monorepo JavaScript](docs/DOCKER_RUNTIME.md#monorepo-javascript) dan [Compatibility](docs/COMPATIBILITY.md#revision-4-additions-for-javascript-monorepos).
- Dependency baru `globset` (pencocokan pola workspace seperti pnpm dan npm) dan `yaml-rust2` (membaca `pnpm-workspace.yaml` dari event parser, dengan alias YAML ditolak) di `sakala-agent-runtime`. `serde-saphyr` sempat dipertimbangkan karena punya batas ekspansi alias, tetapi MSRV-nya 1.89, di atas `rust-version = "1.88"` workspace.
- Deployment yang framework-nya menolak domain project kini gagal dengan kode baru `runtime_app_host_blocked`, bukan berhasil sambil menjawab 403 ke setiap pengunjung. Pada uji production 2026-11-19, `vite preview` lolos health check karena status di bawah 500 dihitung sehat, padahal Vite menolak `Host` di luar `preview.allowedHosts`. Saat deploy, bila probe readiness mendapat 400 atau 403, Agent kini mengirim satu request lagi lewat container Caddy dengan `Host` pengunjung (BusyBox `nc`, paling banyak 4096 byte dibaca) dan menggagalkan deployment hanya bila jawabannya penolakan host Vite preview, Vite dev server, atau halaman debug Django (`DisallowedHost`). Pesannya menyebut domain dan perbaikannya, misalnya `preview: { allowedHosts: ['<domain>'] }` atau mengosongkan start command supaya hasil build disajikan sebagai situs statis. Deployment sebelumnya tetap melayani. 403 lain, misalnya halaman login, dan aplikasi yang hanya menolak nama container tetap lolos. Uji Docker nyata dengan `caddy:2.11.3-alpine` membuktikan ketiganya, dan teks Vite dicocokkan dengan jawaban asli Vite 8.3.5. Aditif di protocol revision 4; lihat [Host yang ditolak framework](docs/DOCKER_RUNTIME.md#host-yang-ditolak-framework) dan [Compatibility](docs/COMPATIBILITY.md#revision-4-additions-for-host-checks).

## [0.12.0] - 2026-11-19

### Added

- Peran host database untuk PostgreSQL terkelola tahap pertama (ADR-020, gmedia/sakala-api#125). Dengan `SAKALA_DATABASE_DRIVER=postgres` (baru, default `none`), binary yang sama mengelola satu instance PostgreSQL lokal sebagai superuser lewat `SAKALA_DATABASE_ADMIN_DSN` (baru, hanya socket Unix atau loopback, tidak pernah dicetak), mengiklankan capability baru `managed-postgres`, dan bisa berjalan di samping driver runtime mana pun. Enam command baru: `CreateDatabase`, `DropDatabase`, `RotateDatabasePassword`, `LockDatabase`, `UnlockDatabase`, dan `InstallDatabaseExtension`. Nama database dan role wajib cocok dengan `^p_[a-z0-9]{16,40}$`, dan semua command idempotent. Setiap database tenant mendapat role `LOGIN` tanpa `CREATEDB`/`CREATEROLE` dengan `CONNECTION LIMIT 5`, default `statement_timeout` dan `idle_in_transaction_session_timeout` 60 detik serta `idle_session_timeout` 30 menit, `temp_file_limit` 1 GB yang tidak bisa dinaikkan tenant, database milik role itu dengan `CONNECTION LIMIT 5`, `REVOKE ALL ... FROM PUBLIC`, dan tanpa `CREATE` di schema `public` untuk `PUBLIC`. `postgres` dan `template1` ditutup untuk `PUBLIC`. Password disimpan sebagai verifier SCRAM-SHA-256 yang dihitung Agent, sehingga plaintext tidak pernah sampai ke server atau log statement-nya. Lock mematikan `LOGIN` role owner dan mengakhiri sesinya, bukan `ALLOW_CONNECTIONS false`, supaya backup tetap jalan dan tenant tidak bisa membukanya sendiri. Extension hanya dari allow-list `pgcrypto`, `citext`, `pg_trgm`, `unaccent`, `hstore`, `vector`. Delapan kode kegagalan baru berawalan `database_`. Heartbeat node database membawa `metadata.database` (`endpoint` dari `SAKALA_DATABASE_PUBLIC_HOST` dan `SAKALA_DATABASE_PUBLIC_PORT`, keduanya baru, serta `available`) dan, paling sering sekali per 60 detik, `metadata.databases` berisi `{ name, size_bytes }` untuk paling banyak 1000 database terkelola; instance yang tidak terbaca membuat node `degraded`. Uji dengan `postgres:18-alpine` sungguhan membuktikan isolasi antar-tenant, default role, batas koneksi, lock, rotasi, allow-list extension, drop, dan log server tanpa password. Aditif di protocol revision 4, tetapi API hanya boleh mengirim command baru ke node `managed-postgres`; lihat [Database Host](docs/DATABASE_HOST.md), [Sakala Agent API](docs/AGENT_API.md#database-host), dan [Compatibility](docs/COMPATIBILITY.md#revision-4-additions-for-managed-postgresql).
- Dependency baru `tokio-postgres` dan `postgres-protocol` di `sakala-agent-runtime`, client PostgreSQL async yang matang dengan dependency transitif kecil; `postgres-protocol` sudah dibawa `tokio-postgres` dan dipakai langsung untuk hashing SCRAM dan escaping identifier/literal ala libpq.

## [0.11.0] - 2026-11-19

### Added

- Metrik dasar per container workload di heartbeat, untuk grafik memori, CPU, dan restart per project (gmedia/sakala-api#128). Node dengan capability baru `workload-metrics` mengirim `metadata.workloads.metrics` dan `metadata.detail_counts.metrics`: satu entri per container workload yang berjalan, termasuk setiap replika, berisi `project_id`, `deployment_id`, `replica`, `sampled_at`, `cpu_percent` (persen dari satu core, bisa di atas 100), `memory_bytes`, `memory_limit_bytes` (`null` tanpa batas), dan `restart_count` (`RestartCount` Docker). Agent mengambil satu sampel `docker stats --no-stream` paling sering sekali per `SAKALA_METRICS_INTERVAL_SECONDS` (baru, default 60, antara 10 dan 3600) dan mengirimnya sekali di heartbeat berikutnya. Sampel yang gagal tidak pernah dikirim sebagian: kedua key dihilangkan dan Agent mencatat satu `WARN` per rentetan kegagalan. Container release dan container lain di node tidak masuk. List dibatasi `SAKALA_MAX_ACTIVE_CONTAINERS` × `SAKALA_MAX_REPLICAS_PER_PROJECT` item. Aditif di protocol revision 4; lihat [Metrik workload](docs/AGENT_API.md#metrik-workload) dan [Compatibility](docs/COMPATIBILITY.md#revision-4-additions-for-workload-metrics).

## [0.10.0] - 2026-11-18

### Added

- `sakala-edge` kini melaporkan keadaannya di setiap request tabel route, supaya `sakala-api` bisa memberi tahu admin saat sebuah Edge basi atau terus gagal. Header `Sakala-Edge-Applied-Revision` berisi revision yang sedang dilayani Caddy (tabel terakhir yang berhasil ditulis, divalidasi, dan di-reload, atau snapshot yang diterima saat start), `none` sebelum ada yang diterapkan. Header `Sakala-Edge-Status` berisi `ok` bila siklus terakhir berhasil, termasuk `304`, atau kode error siklus terakhir yang sama dengan `last_error.code` di `status.json`. Tabel yang ditolak atau di-rollback tidak menggeser revision yang dilaporkan. `User-Agent` tetap `sakala-edge/<versi>`. `status.json` tidak berubah. Lihat [Laporan ke API](docs/EDGE.md#laporan-ke-api).
- Release command per deployment, untuk migrasi database yang cukup berjalan sekali per deploy, seperti release phase Heroku atau pre-deploy command Railway. Payload `DeployProject` boleh membawa `release_command` (string 1 sampai 1000 karakter tanpa control character; tidak ada, `null`, atau kosong berarti tidak ada). Setelah build dan sebelum replika pertama, Agent menjalankan satu container dari image baru dengan environment, network project, batas resource, runtime container (`runsc` bila dikonfigurasi), dan opsi keamanan yang sama dengan replika, tanpa route dan tanpa restart policy, lalu menunggu exit-nya. Exit code 0 melanjutkan deploy seperti biasa. Exit code lain menggagalkan deploy dengan kode baru `release_command_failed` (pesan menyebut exit code, dan batas memory bila container kehabisan memory), dan melewati batas waktu menggagalkannya dengan `release_command_timeout`. Pada kedua kegagalan itu tidak ada bagian versi baru yang dinyalakan dan versi lama tetap melayani. Output perintah masuk log deployment berawalan `[release]` dengan redaksi yang sama, disertai event `deployment.release.started` dan `deployment.release.completed`. Container release selalu dihapus, termasuk saat deploy dibatalkan atau Agent shutdown, dan sisa container release dihapus saat Agent start. `WakeProject`, `RestartProject`, `RefreshRoute`, `ReconcileWorkload`, dan pemulihan replika tidak pernah menjalankannya. Uji Docker nyata: release command yang exit 0 menulis penanda ke log dan aplikasi lalu menyala; exit 3 menggagalkan deploy dan container lama tetap berjalan dengan route-nya; `sleep 999` dengan batas 5 detik dihentikan, dan deploy-nya gagal 6,9 dan 10,3 detik setelah dimulai pada dua run. Lihat [Release command](docs/DOCKER_RUNTIME.md#release-command).
- Konfigurasi `SAKALA_RELEASE_COMMAND_TIMEOUT_SECONDS` (default 600, harus lebih pendek dari `SAKALA_COMMAND_TIMEOUT_SECONDS`).
- Capability node `release-command`, diiklankan hanya oleh driver `docker`. Semua tambahan ini aditif di protocol revision 4; lihat [Compatibility](docs/COMPATIBILITY.md#revision-4-additions-for-release-commands).

### Fixed

- Project yang tidur kini bisa dibangunkan dari URL-nya pada instalasi yang menghubungi API lewat alamat internal. Halaman route tidur sebelumnya selalu menyuruh browser pengunjung memanggil endpoint wake di bawah `SAKALA_API_URL`; pada self-host nilainya alamat internal seperti `http://127.0.0.1:8100`, sehingga browser tidak bisa menjangkaunya dan project tetap tidur. Konfigurasi opsional baru `SAKALA_PUBLIC_API_URL` (URL absolut `http` atau `https` tanpa kredensial, query, atau fragment) kini dipakai khusus untuk URL yang dibuka browser, dan defaultnya `SAKALA_API_URL`, jadi instalasi hosted tidak berubah. Nilai yang tidak valid membuat Agent berhenti saat start. Lihat [Configuration](docs/CONFIGURATION.md).

### Security

- Setiap route project di router node kini menghapus atribut `Domain` dari setiap header `Set-Cookie` aplikasi (`header_down Set-Cookie "(?i);\s*domain\s*=[^;]*" ""`), baik untuk satu replika maupun beberapa. Selama `sakala.app` belum ada di Public Suffix List, browser menganggap semua `<slug>.sakala.app` satu site, sehingga cookie `Domain=sakala.app` dari satu aplikasi ikut terkirim ke aplikasi lain (cookie tossing dan session fixation antar-aplikasi). Nilai dan atribut cookie lainnya tidak berubah, termasuk nilai yang memuat kata domain seperti `pref=domain%3Dx`. Cookie yang ditulis JavaScript lewat `document.cookie` tidak tertangani; perbaikan sebenarnya tetap pendaftaran ke PSL. Route yang sudah ada di node mendapat baris ini setelah project dideploy, di-restart, dibangunkan, atau menerima `RefreshRoute`/`restore_route`; restart Agent saja tidak menulis ulang route. Uji Docker dengan `caddy:2.11.3-alpine` memastikan sembilan variasi cookie diterima klien tanpa `Domain` dan atribut lain utuh. Lihat [Cookie host-only](docs/RUNTIME_HARDENING.md#cookie-host-only-di-route-project).

## [0.9.1] - 2026-11-18

### Fixed

- Replika yang berhenti kini dinyalakan lagi oleh Agent tanpa menunggu deploy berikutnya. Di bawah `--restart unless-stopped`, Docker tidak menyalakan lagi container yang dihentikan dengan `docker stop` atau `docker kill`, sehingga pada uji staging v0.9.0 replika yang di-kill tetap `Exited (137)` sekitar 4,5 menit sampai deploy berikutnya. Setiap interval health worker (`SAKALA_RUNTIME_HEALTH_INTERVAL_SECONDS`) kini menjalankan `docker start` untuk replika `exited` dari deployment yang masih punya replika lain yang `Up` dan merupakan satu-satunya deployment project itu yang melayani. Project yang tidur, deployment satu replika, project yang sedang deploy bergulir atau menjalankan command lain, dan node yang penuh (`SAKALA_MAX_ACTIVE_CONTAINERS`) tidak disentuh; command project dan pemulihan saling menunggu lewat kunci per project. Setiap replika mendapat paling banyak 5 kali start (langsung, lalu setelah 30, 60, 120, dan 240 detik), hitungan direset setelah replika bertahan 5 menit, dan setelah itu Agent mencatat `WARN` dan berhenti mencoba. Route tidak berubah. Container replika yang hilang tidak dibuat ulang dan kembali pada deploy berikutnya. Uji Docker nyata: replika yang di-`docker kill` kembali menerima request 10,5 dan 11,7 detik (dua run) setelah putaran pemulihan tanpa satu request pun gagal. Lihat [Pemulihan replika](docs/RUNTIME_HARDENING.md#pemulihan-replika).

## [0.9.0] - 2026-11-18

### Added

- Binary baru `sakala-edge`, service sinkronisasi route untuk Sakala Edge (ADR-019, tahap 2). Service ini memoll `GET {SAKALA_API_URL}/api/edge/v1/routes` dengan `SAKALA_EDGE_TOKEN` setiap 5 detik (`SAKALA_EDGE_POLL_INTERVAL_SECONDS`), dengan backoff berlipat dua sampai 60 detik (`SAKALA_EDGE_MAX_BACKOFF_SECONDS`) saat gagal. Tabel ditolak seluruhnya, dan route yang sedang dilayani tetap, bila API tidak menjawab `200`, JSON rusak, tabel kosong, route lebih sedikit dari `expected_minimum_routes`, host di luar `SAKALA_EDGE_DOMAIN_SUFFIXES` (aturan satu label yang sama dengan Agent), host ganda, atau upstream yang bukan `IP:port` di dalam `SAKALA_EDGE_UPSTREAM_CIDRS` (default jaringan privat RFC 1918 dan `fc00::/7`). Tabel valid dirender menjadi `routes.caddy` (satu matcher host dan `reverse_proxy` per project, `lb_policy least_conn` dengan passive health seperti route replika), lalu divalidasi dan di-reload dengan `SAKALA_EDGE_CADDY_VALIDATE_CMD` dan `SAKALA_EDGE_CADDY_RELOAD_CMD`; bila Caddy menolak, file lama dikembalikan dan Caddy di-reload lagi. Tabel yang diterima disimpan di `routes.json` dan dirender lagi saat start sebelum polling pertama, jadi restart saat API mati tetap melayani semua route. Status untuk monitoring ditulis ke `status.json`, tanpa listener jaringan. Contoh unit systemd, aturan polkit, env file, dan Caddyfile host ada di `examples/edge/`. Uji Docker dengan `caddy:2.11.3-alpine` memvalidasi file hasil render, membagi request ke dua upstream, dan tidak menggagalkan satu pun dari 20 request setelah satu upstream dimatikan. Lihat [Sakala Edge](docs/EDGE.md).
- Rilis kini menerbitkan `sakala-edge-linux-x86_64` di samping `sakala-agent-linux-x86_64`. `SHA256SUMS` memuat satu baris per asset dalam format yang sama seperti sebelumnya.
- Konfigurasi opsional `SAKALA_AGENT_REGION` dan `SAKALA_AGENT_EDGE_ADDRESS`. Heartbeat mengirimnya sebagai `metadata.runtime.region` dan `metadata.runtime.edge_address`, supaya `sakala-api` bisa menempatkan project berdasarkan region dan menyusun tabel route Sakala Edge. Object `metadata.runtime` hanya dikirim bila minimal satu nilainya diisi. Keduanya diperiksa saat start dengan aturan yang sama dengan API (region `^[a-z0-9]+(?:-[a-z0-9]+)*$` paling panjang 32 karakter; alamat Edge `host:port` dengan IPv4, IPv6 dalam kurung siku, atau nama DNS, port 1 sampai 65535, paling panjang 255 karakter), dan nilai yang tidak valid membuat Agent berhenti dengan pesan konfigurasi, karena API akan menolak setiap heartbeat yang memuatnya.
- Heartbeat kini memuat `metadata.workloads.maximum_active_workloads`, yaitu `SAKALA_MAX_ACTIVE_CONTAINERS`, untuk penempatan berdasarkan kapasitas. Field ini tidak dikirim di driver `noop` atau saat kapasitas runtime tidak terbaca. Ketiga field aditif di protocol revision 4; lihat [Compatibility](docs/COMPATIBILITY.md#revision-4-additions-for-placement).

## [0.8.0] - 2026-11-18

### Added

- Replika dan load balancing di satu node. Payload `DeployProject` boleh membawa `replicas` (bilangan bulat, default 1), dan Agent menjalankan sebanyak itu container dari image yang sama di network project. Router Caddy node membagi request ke semuanya dengan `lb_policy least_conn`, mengulang request ke replika lain selama 5 detik, dan tidak mengirim request ke replika yang gagal selama 10 detik. Deploy berjalan bergulir: semua replika baru harus ready sebelum route pindah, lalu set lama dihentikan; satu replika gagal berarti deploy gagal dan route lama tetap. Sleep, wake, stop, restart, health check, dan reconcile bekerja pada seluruh replika. Uji Docker nyata: dari 30 request ketiga replika menerima bagian, mematikan satu replika dengan `docker kill` tidak menggagalkan satu request pun, dan redeploy dari 3 ke 2 replika melayani 78 request tanpa gagal. Lihat [Replika](docs/AGENT_API.md#replika).
- Konfigurasi `SAKALA_MAX_REPLICAS_PER_PROJECT` (default 4, maksimum 64). Deployment yang meminta lebih gagal dengan kode baru `runtime_replicas_exceeded`. Nilai `replicas` yang tidak terbaca, misalnya `0` atau `"3"`, dijalankan sebagai satu replika dengan peringatan di log, tanpa menolak deploy.
- Capability node `replicas`, diiklankan hanya oleh driver `docker`.
- Heartbeat kini memuat `metadata.workloads.replicas` berisi `{ project_id, deployment_id, desired, ready }` untuk deployment berjalan dengan lebih dari satu replika, `detail_counts.replicas`, dan `workloads.maximum_replicas_per_project`. Field ini aditif di protocol revision 4; lihat [Compatibility](docs/COMPATIBILITY.md#revision-4-additions-for-replicas).

### Changed

- Setiap container yang dijalankan Agent kini membawa label `dev.sakala.replica` dan `dev.sakala.replicas`. Deployment satu replika tetap memakai nama container dan isi route yang sama seperti sebelumnya, jadi workload yang sedang berjalan atau tidur tetap bekerja setelah upgrade, dan node yang di-rollback ke Agent lama tetap menemukannya.
- `crashed_details` hanya melaporkan deployment beberapa replika bila semua replikanya crash. Deployment satu replika dilaporkan seperti sebelumnya.
- `SleepProject`, `StopProject`, dan pembersihan container lama setelah deploy kini menghentikan container dalam satu `docker stop`, sehingga beberapa replika berbagi satu masa tenggang 10 detik, bukan menunggu satu per satu. Pada deploy 3 ke 2 replika di uji Docker nyata, waktunya turun dari 37,6 menjadi 15,9 detik.
- Pengecekan kapasitas kini menghitung container berjalan milik project lain ditambah semua replika baru. Untuk satu replika hasilnya sama seperti sebelumnya, kecuali node yang sudah melebihi `SAKALA_MAX_ACTIVE_CONTAINERS` (misalnya setelah batasnya diturunkan): redeploy di node seperti itu kini ditolak, bukan diizinkan.

- Default `SAKALA_PROJECT_DOMAIN_SUFFIXES` kini `run.sakala.localhost`, `staging.sakala.app`, dan `sakala.app`, sesuai domain project Sakala hosted yang pindah ke registrable domain terpisah (ADR-016): production `<slug>.sakala.app`, staging `<slug>.staging.sakala.app`. `run.staging.sakala.dev` dan `run.sakala.dev` tidak lagi diterima tanpa konfigurasi, jadi deploy dengan domain lama ditolak dengan `invalid_runtime_command`. Instalasi yang mengisi `SAKALA_PROJECT_DOMAIN_SUFFIXES` sendiri tidak terpengaruh. Aturan satu label tetap: node yang hanya memakai `sakala.app` menolak `x.staging.sakala.app`.
- Setelah domain project dipindah di `sakala-api` (`projects:move-domain`), setiap project harus dideploy ulang, termasuk yang sedang tidur. `WakeProject` dan `RestartProject` membangun route dari label `dev.sakala.domain` di container, bukan dari payload baru, jadi container lama tetap memakai domain lama sampai dideploy ulang. Bila container dengan domain yang tidak lagi diizinkan ditidurkan, Agent tidak bisa membuat halaman bangun untuknya dan menghapus route-nya. Lihat [Domain project](docs/CONFIGURATION.md#domain-project).

### Security

- `quinn-udp` 0.5.14 → 0.5.16 (transitive) to fix RUSTSEC-2026-0538, which had turned the scheduled security audit red since 2026-11-16.

## [0.7.1] - 2026-11-13

### Added

- Payload `SleepProject` kini boleh membawa `page`, berisi `waking` (default) atau `unavailable`. Dengan `unavailable`, domain project menampilkan halaman statis "Proyek tidak tersedia" dengan HTTP 503, tanpa script, tanpa permintaan wake, tanpa muat ulang, dan tanpa `Retry-After`. API bisa memakainya untuk project yang di-suspend, yang sebelumnya ikut menampilkan "Proyek sedang dinyalakan" dan memuat ulang terus walaupun tidak akan pernah bangun. Route-nya memakai penanda `(sleeping)` yang sama, jadi `WakeProject`, `StopProject`, redeploy, dan cleanup route basi tidak berubah. Payload tanpa `page` berperilaku seperti sebelumnya, dan nilai yang tidak dikenal dibaca sebagai `waking` dengan peringatan di log, tanpa menolak sleep. Field ini aditif di protocol revision 4; lihat [Compatibility](docs/COMPATIBILITY.md#revision-4-additions-after-v070).

### Fixed

- Halaman "Proyek sedang dinyalakan" tidak lagi memuat ulang tanpa henti. Sebelumnya, project yang tidak bisa bangun, misalnya karena batas container workspace penuh, membuat setiap tab pengunjung memuat ulang setiap 3 detik selamanya dan memanggil endpoint wake sekitar 20 kali per menit. Kini halaman meminta wake sekali per kunjungan, memuat ulang setiap 3 detik paling lama 2 menit (dihitung di `sessionStorage`), lalu menampilkan "Proyek belum bisa dinyalakan" dengan tombol "Coba lagi" yang mengirim satu permintaan wake lagi. Tanpa JavaScript atau bila `sessionStorage` diblokir, halaman memuat ulang setiap 15 detik, bukan 5 atau 3 detik. Lihat [Caddy Integration](docs/CADDY_INTEGRATION.md#route-tidur-dan-counter-request).

## [0.7.0] - 2026-11-13

### Added

- Dasar untuk idle sleep. Saat `SleepProject`, domain project kini menampilkan halaman "Proyek sedang dinyalakan" dengan HTTP 503, `Retry-After: 5`, dan `Cache-Control: no-store`, bukan 404 bawaan router. Halaman itu meminta `sakala-api` membangunkan project lewat `POST <SAKALA_API_URL>/api/v1/public/projects/<slug>/wake`, lalu memuat ulang sendiri setiap 3 detik. Route tidur memakai penanda kepemilikan `(sleeping)`, tetap dipertahankan selama container deployment-nya masih ada, dan dihapus oleh `StopProject`, redeploy, atau cleanup route basi bila containernya sudah hilang. Bila halaman tidak bisa dibangun, route dihapus seperti sebelumnya. Lihat [Caddy Integration](docs/CADDY_INTEGRATION.md#route-tidur-dan-counter-request).
- Heartbeat kini memuat `metadata.workloads.traffic`, yaitu kapan router terakhir melayani request untuk setiap workload yang berjalan, plus `detail_counts.traffic`. Nilainya dibaca dari counter `caddy_http_requests_total` milik Caddy runtime, yang membutuhkan `metrics { per_host }` di Caddyfile runtime. Restart Agent memberi setiap host waktu start Agent, bukan "tidak pernah", dan list dihilangkan saat counter tidak bisa dibaca. Field ini aditif di protocol revision 4; lihat [Compatibility](docs/COMPATIBILITY.md#revision-4-additions-for-idle-sleep).
- Capability node `idle-sleep`, diiklankan hanya oleh driver `docker`.

### Changed

- `WakeProject` kini mengecek kapasitas node seperti deploy, di bawah lock admission yang sama. Node yang sudah menjalankan `SAKALA_MAX_ACTIVE_CONTAINERS` workload menolak wake dengan `runtime_capacity_exceeded`, dan halaman tidur tetap terpasang.
- Wake yang sudah menjalankan container tetapi aplikasinya tidak ready, atau route-nya ditolak Caddy, kini menghentikan container itu lagi (`docker stop --time 10`) supaya tidak memakai slot kapasitas. Halaman tidur tetap terpasang, container, image, dan network tidak dihapus, dan command gagal dengan error aslinya; bila stop itu gagal, Agent mencatat peringatan di log operator.

## [0.6.0] - 2026-11-12

### Added

- `SAKALA_PROJECT_DOMAIN_SUFFIXES` mengatur suffix domain yang boleh dipakai deployment, dipisah koma, misalnya `run.example.ac.id`. Sebelumnya Agent hanya menerima domain di bawah `run.sakala.localhost`, `run.staging.sakala.dev`, dan `run.sakala.dev`, sehingga instalasi self-hosted di domain lain ditolak di setiap deploy. Tanpa variabel ini ketiga suffix itu tetap dipakai, jadi Sakala hosted berperilaku sama. Setiap suffix divalidasi saat start (huruf kecil, minimal dua label, tanpa wildcard, label internasional dalam bentuk `xn--`), dan nilai yang tidak valid menghentikan Agent dengan pesan yang menyebut variabel dan alasannya. Suffix yang berlaku dicatat di log preflight sebagai check `project-domains`. Lihat [Domain project](docs/CONFIGURATION.md#domain-project).

### Security

- Domain deployment kini harus tepat satu label DNS di bawah suffix yang diizinkan. Sebelumnya subdomain bertingkat seperti `api.portfolio.run.sakala.dev` ikut diterima, sehingga satu project bisa meminta route di bawah domain project lain. `sakala-api` selalu mengirim `<slug>.<SAKALA_PROJECT_DEFAULT_DOMAIN>`, jadi domain yang dibuat API tidak terpengaruh. Label `xn--` yang bukan punycode valid juga ditolak.

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

[Unreleased]: https://github.com/gmedia/sakala-agent/compare/v0.16.1...HEAD
[0.16.1]: https://github.com/gmedia/sakala-agent/compare/v0.16.0...v0.16.1
[0.16.0]: https://github.com/gmedia/sakala-agent/compare/v0.15.0...v0.16.0
[0.15.0]: https://github.com/gmedia/sakala-agent/compare/v0.14.0...v0.15.0
[0.14.0]: https://github.com/gmedia/sakala-agent/compare/v0.13.0...v0.14.0
[0.13.0]: https://github.com/gmedia/sakala-agent/compare/v0.12.0...v0.13.0
[0.12.0]: https://github.com/gmedia/sakala-agent/compare/v0.11.0...v0.12.0
[0.11.0]: https://github.com/gmedia/sakala-agent/compare/v0.10.0...v0.11.0
[0.10.0]: https://github.com/gmedia/sakala-agent/compare/v0.9.1...v0.10.0
[0.9.1]: https://github.com/gmedia/sakala-agent/compare/v0.9.0...v0.9.1
[0.9.0]: https://github.com/gmedia/sakala-agent/compare/v0.8.0...v0.9.0
[0.8.0]: https://github.com/gmedia/sakala-agent/compare/v0.7.1...v0.8.0
[0.7.1]: https://github.com/gmedia/sakala-agent/compare/v0.7.0...v0.7.1
[0.7.0]: https://github.com/gmedia/sakala-agent/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/gmedia/sakala-agent/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/gmedia/sakala-agent/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/gmedia/sakala-agent/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/gmedia/sakala-agent/compare/v0.2.3...v0.3.0
[0.2.3]: https://github.com/gmedia/sakala-agent/compare/v0.2.2...v0.2.3
[0.2.2]: https://github.com/gmedia/sakala-agent/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/gmedia/sakala-agent/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/gmedia/sakala-agent/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/gmedia/sakala-agent/releases/tag/v0.1.0
