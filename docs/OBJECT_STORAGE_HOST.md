# Object Storage Host

Agent bisa berperan sebagai host object storage untuk bucket terkelola (ADR-021): ia
mengelola satu instance RustFS lokal, membuat satu bucket dan satu access key untuk
setiap bucket tenant, dan melaporkan pemakaian bucket ke `sakala-api`. Peran ini
dinyalakan dengan `SAKALA_OBJECT_STORAGE_DRIVER=rustfs` dan memakai binary yang sama
dengan runtime node.

## Kenapa binary yang sama

Seperti [Database Host](DATABASE_HOST.md), peran ini bukan runtime driver ketiga.
Ia memegang hak lain, yaitu root key instance RustFS, sehingga punya port sendiri,
`ObjectStorageAdmin` di `sakala-agent-core`, dengan adapter `RcObjectStorageAdmin`
di `sakala-agent-runtime`. Polling, claim, heartbeat, redaksi, dan shutdown tetap
kode yang sama.

| Topologi | Konfigurasi |
| --- | --- |
| Co-located (staging, pilot production, self-host) | `SAKALA_RUNTIME_DRIVER=docker`, `SAKALA_OBJECT_STORAGE_DRIVER=rustfs`, boleh juga `SAKALA_DATABASE_DRIVER=postgres` |
| VM storage khusus | `SAKALA_RUNTIME_DRIVER=noop`, `SAKALA_OBJECT_STORAGE_DRIVER=rustfs`. Docker CLI tetap harus menjangkau daemon yang menjalankan RustFS. |

Node dengan peran ini mengiklankan capability `managed-s3` di samping capability
runtime-nya. Namanya `managed-s3`, bukan nama engine, karena tenant berbicara S3.

## Konfigurasi

Nama variabel sama dengan `object-storage.env` di repo deployment.

| Variable | Default | Keterangan |
| --- | --- | --- |
| `SAKALA_OBJECT_STORAGE_DRIVER` | `none` | `none` atau `rustfs`. |
| `SAKALA_OBJECT_STORAGE_CONTAINER` | `sakala-object-storage-rustfs-1` | Nama container RustFS, cocok dengan `^[A-Za-z0-9][A-Za-z0-9_.-]{0,127}$`, jadi tidak pernah terbaca sebagai opsi `docker exec`. |
| `SAKALA_OBJECT_STORAGE_PUBLIC_ENDPOINT` | tidak ada | Wajib. Alamat S3 yang dipakai aplikasi dan browser, misalnya `https://s3.sakala.app`: `https://host[:port]` tanpa path, query, fragment, atau kredensial, paling panjang 255 karakter. `http://` hanya diterima untuk host loopback (pengembangan lokal). Dikirim sebagai `object_storage.endpoint` dalam bentuk origin yang dinormalkan (host huruf kecil, port default dibuang). |
| `SAKALA_OBJECT_STORAGE_REGION` | `us-east-1` | Region tanda tangan S3, cocok dengan `^[a-z0-9]+(-[a-z0-9]+)*$` dan paling panjang 32 karakter. Harus sama dengan `RUSTFS_REGION` container. |
| `SAKALA_OBJECT_STORAGE_TOTAL_QUOTA_MB` | tidak ada | Wajib, bilangan bulat di atas 0. Jumlah semua kuota bucket di node ini tidak boleh melewatinya. Dikirim sebagai `object_storage.capacity_mb`. |
| `SAKALA_OBJECT_STORAGE_MIN_FREE_MB` | `1024` | Bilangan bulat ≥ 0. Ruang disk yang harus tetap bebas saat semua bucket penuh. |
| `SAKALA_OBJECT_STORAGE_DATA_DIR` | `/var/lib/sakala/object-storage` | Path absolut direktori data RustFS di host, dibaca dengan `df` untuk ruang bebas. |

Nilai yang hilang atau tidak valid membuat Agent berhenti saat start. Instance yang
belum siap tidak menghentikan Agent: Agent mencatat `WARN`, heartbeat melaporkan node
`degraded` dengan `object_storage.available: false`, dan command bucket gagal dengan
`bucket_admin_unavailable` sampai instance menjawab.

**Tidak ada credential RustFS di environment Agent.** Root key tetap di file
`/run/secrets/rustfs-access-key` dan `/run/secrets/rustfs-secret-key` yang di-mount
ke container RustFS.

## Cara Agent berbicara dengan RustFS

Setiap operasi menjalankan CLI RustFS, `rc`, di dalam container instance:

```text
docker exec [-e SAKALA_BUCKET_SECRET] [-e SAKALA_BUCKET_POLICY] <container> sh -c '<script tetap>' rc <detik> <argumen>...
```

- Script-nya konstanta yang membangun alias `sakala` persis seperti `rc()` di `scripts/object-storage.sh` repo deployment: dari file root key di dalam container, jadi root key tidak pernah muncul di argumen proses dan tidak keluar dari container.
- Bedanya dengan script staging: `rc` dijalankan di bawah `timeout` milik container (`exec timeout <detik> rc ...`). Membunuh `docker exec` hanya menghentikan client di host; proses di container tetap jalan (sudah dibuktikan manual: `sleep` di container tetap hidup setelah client dibunuh, dan hilang bila dijalankan lewat `timeout`). Karena itu container menghentikan `rc` 5 detik sebelum Agent berhenti menunggu, jadi tidak ada `rc` yang masih mengubah instance setelah Agent melaporkan hasil, dan `DeleteBucket` yang diulang tidak menjalankan `rb` kedua bersamaan dengan yang lama.
- Setiap command mendapat jatah waktu `SAKALA_COMMAND_TIMEOUT_SECONDS` dikurangi 15 detik, dibagi per langkah: paling lama 60 detik per panggilan `rc`, dan 600 detik untuk menghapus isi bucket dengan menyisakan 20 detik untuk dua langkah policy sesudahnya. Langkah yang tinggal punya kurang dari 10 detik tidak dimulai, dan command gagal dengan `bucket_admin_unavailable`.
- Argumen hanya berisi nama bucket, access key ID, dan angka yang sudah divalidasi. Tidak ada nilai yang disisipkan ke dalam script.
- Secret tenant masuk lewat `-e SAKALA_BUCKET_SECRET` (nama saja), nilainya di environment proses anak, lalu `rc admin user passwd ... --password-from-env SAKALA_BUCKET_SECRET`. Dokumen policy masuk lewat `SAKALA_BUCKET_POLICY` dan stdin.
- `rc admin user add` menerima secret sebagai argumen, jadi user baru dibuat dengan secret sekali pakai acak lalu langsung diganti dari environment, sama seperti script staging. Output `add` (yang mencetak secret sekali pakai itu) dibuang.
- Stderr `rc` yang masuk ke pesan `fail` dipotong (5 baris, 1000 byte) dan diredaksi.
- Command dan pembacaan pemakaian berjalan satu per satu per node, dengan lock sendiri yang terpisah dari lock database.

Policy setiap bucket bernama sama dengan bucket dan isinya persis dokumen staging:
`s3:GetBucketLocation`, `s3:ListBucket`, `s3:ListBucketMultipartUploads` pada bucket,
lalu `s3:GetObject`, `s3:PutObject`, `s3:DeleteObject`, `s3:AbortMultipartUpload`,
`s3:ListMultipartUploadParts` pada `bucket/*`. Satu-satunya policy user adalah policy
itu.

## Yang dikerjakan setiap command

`CreateBucket` memeriksa semua yang bisa menolak sebelum mengubah apa pun: membaca
kuota semua bucket, membaca user key, menolak dengan `bucket_conflict` bila user itu
membawa policy lain, lalu menjalankan pemeriksaan kapasitas. Setelah itu ia membuat
bucket bila belum ada, memasang kuota, menulis ulang policy, membuat user bila belum
ada lalu menyetel secret dari payload, memasang policy ke user bila belum, dan
menutup akses anonim (`rc anonymous set private`).

Mengulang `CreateBucket` aman dan tidak pernah membuka key yang di-lock:
`rc admin user add` tidak pernah dijalankan untuk user yang sudah ada, karena pada rc
0.1.37 perintah itu diam-diam mengganti secret dan mengaktifkan kembali user yang
dinonaktifkan. `passwd` dan `policy attach` tidak mengubah status user. Bucket yang
setengah jadi (sudah dibuat tetapi belum punya kuota) diselesaikan oleh create
berikutnya.

`DeleteBucket` menonaktifkan lalu menghapus user (key langsung berhenti), menghapus
semua object termasuk upload multipart yang belum selesai lalu bucket-nya
(`rc rb --force --dangerous --yes`), lalu menghapus policy. Bagian yang sudah tidak
ada dilewati.

`RotateBucketKey` mengganti secret user. Access key ID tetap sama. Bila `rc` gagal
atau tidak menjawab tepat waktu, perubahan itu bisa saja sudah diterapkan RustFS.
Agent lalu mengirim satu request S3 bertanda tangan dengan secret baru (di dalam
container, secret lewat environment dan stdin curl) untuk mengetahui secret mana
yang berlaku. RustFS 1.0.6 memeriksa tanda tangan sebelum status key, jadi:

| Jawaban request | Artinya | Hasil command |
| --- | --- | --- |
| `200` | Secret baru berlaku | `complete`, dengan `WARN` di log Agent |
| `403 SignatureDoesNotMatch` | Secret baru belum berlaku, secret lama masih hidup | `fail` dengan kode kegagalan aslinya |
| `403 InvalidAccessKeyId` pada key yang di-lock | Secret baru berlaku pada key yang nonaktif | `complete` |
| Lainnya, atau request tidak bisa dikirim | Tidak diketahui | `fail` dengan `bucket_outcome_unknown` |

`bucket_outcome_unknown` berarti secret lama mungkin sudah mati; API sebaiknya
mengirim ulang `RotateBucketKey` yang sama, yang idempotent.

`LockBucket` dan `UnlockBucket` menonaktifkan dan mengaktifkan user
(`rc admin user disable|enable`), setelah memeriksa bahwa key itu memang milik
bucket ini (`bucket_conflict` bila user membawa policy lain) dan bucket-nya ada
(`bucket_not_found` bila tidak).

## Pemeriksaan kapasitas

Berlaku untuk `CreateBucket` dan kenaikan kuota di `SetBucketQuota`:

- Agent membaca kuota dan pemakaian setiap bucket di instance, terkelola atau tidak.
- Bucket lain tanpa kuota membuat command gagal dengan `bucket_capacity_exceeded`, dan Agent menulis `WARN` yang menyebut namanya. Itu kesalahan operator: ukurannya tidak bisa dihitung. Bucket yang sedang dibuat sendiri tidak dihitung di sini, supaya create yang terputus setelah `mb` bisa diulang.
- `committed_after` = jumlah kuota bucket lain + kuota yang diminta. Kuota bucket ini sendiri diganti, tidak ditambahkan.
- Wajib `committed_after ≤ SAKALA_OBJECT_STORAGE_TOTAL_QUOTA_MB`.
- Wajib `ruang bebas DATA_DIR − (committed_after − terpakai) ≥ SAKALA_OBJECT_STORAGE_MIN_FREE_MB`, yaitu ruang yang tersisa bila semua bucket terisi penuh.

Penurunan kuota tidak diperiksa dan selalu diterima, juga di bawah pemakaian saat
ini; upload baru ditolak sampai pemakaian turun.

## Pemakaian di heartbeat

Paling sering sekali per 60 detik Agent membaca semua bucket dalam satu `docker exec`
(`rc ls` lalu `rc quota info` per bucket, batas waktu 15 detik di dalam batas 20 detik
heartbeat) dan mengirim `metadata.buckets` sekali di heartbeat berikutnya.

Pembacaan ini menunggu paling lama 2 detik bila ada command bucket yang sedang
berjalan, misalnya `DeleteBucket` besar yang bisa memakan beberapa menit. Bila masih
sibuk, pembacaan dilewati tanpa dianggap gagal: `object_storage.available` tetap
nilai terakhirnya, `metadata.buckets` tidak dikirim, dan pembacaan dicoba lagi di
heartbeat berikutnya.

Batas jumlah bucket: satu `rc quota info` butuh sekitar 35 ms pada instance yang
diam, jadi 15 detik cukup untuk sekitar 400 bucket. Di atas itu pembacaan gagal dan
node menjadi `degraded`, walaupun heartbeat boleh membawa sampai 1000 item. Untuk
pilot ini tidak relevan; sebelum satu instance memegang ratusan bucket, pembacaan
perlu dibuat paralel atau memakai sumber lain. Hanya bucket yang namanya cocok
dengan `^b-[a-z0-9]{16,40}$` yang dilaporkan; bucket operator seperti `w-...` tidak
pernah dilaporkan, tetapi kuotanya tetap dihitung di pemeriksaan kapasitas.
`size_bytes` adalah `usage` RustFS, yaitu object yang sudah lengkap. Kontraknya ada di
[Sakala Agent API](AGENT_API.md#object-storage-host).

## Yang dibuktikan di RustFS sungguhan

```bash
cargo test -p sakala-agent-runtime --test object_storage_smoke_test -- --ignored --nocapture
```

Test ini menjalankan `rustfs/rustfs:1.0.6@sha256:ee7170...` (bisa diganti dengan
`SAKALA_SMOKE_RUSTFS_IMAGE`) seperti `compose.object-storage.yaml`: root read-only,
tanpa capability, root key dari file, S3 hanya di loopback container. Command lewat
dispatcher yang sama dengan Agent, dan request tenant ditandatangani dengan
`curl --aws-sigv4` di dalam container, secret lewat environment. Jalankan setiap kali
image RustFS naik versi.

Hasil pada 2026-11-23 (RustFS 1.0.6, rc 0.1.37, Docker 29.10):

| Yang diuji | Hasil |
| --- | --- |
| Key bisa put, get, list, delete di bucket-nya | Lolos. |
| Key membaca, menulis, atau me-list bucket lain; membuat bucket; menghapus bucket-nya; memasang bucket policy, versioning, lifecycle, atau CORS | Semua `403 AccessDenied`. |
| Key memanggil ListBuckets | **Tidak ditolak**: `200`, tetapi daftarnya hanya berisi bucket miliknya. RustFS menyaring hasilnya sesuai policy, jadi tidak ada nama bucket lain yang bocor. Test menerima `403` atau daftar yang hanya berisi bucket sendiri. |
| GET dan list anonim | `403`. `rc anonymous set private` tersedia di rc 0.1.37. |
| Presigned GET | `200`. |
| Satu PUT melewati kuota | Ditolak, `400 InvalidRequest` "Bucket quota exceeded". |
| Upload multipart yang melewati kuota saat complete | Ditolak saat `CompleteMultipartUpload`, `400 InvalidRequest`. Tiap part tidak diperiksa: part 5 MiB diterima di bucket berkuota 1 MiB (diamati manual, bukan bagian test). |
| Part dari upload yang belum selesai | Tidak dihitung di `usage` (7 MiB part, `usage` tetap 4 byte). RustFS membatalkannya setelah `RUSTFS_API_STALE_UPLOADS_EXPIRY`. `rc du --incomplete` hanya bisa menghitungnya lewat scan sisi client (`--fallback`), bukan dari snapshot server (diamati manual). |
| Rotasi | Secret lama langsung `403`, secret baru `200`. |
| Lock | Key dan presigned URL yang ditandatanganinya `403`. `CreateBucket` yang diulang tetap meninggalkan user `disabled`. Unlock memulihkan keduanya. |
| Jawaban `passwd` yang hilang (runner tiruan di atas `docker exec` sungguhan) | Sudah diterapkan: rotasi `complete` setelah request bertanda tangan. Belum diterapkan: `fail`, dan secret lama tetap bekerja. Key yang di-lock: tetap terdeteksi sudah diterapkan. |
| Delete | Object, upload yang belum selesai, user, policy, dan bucket hilang; ulangan melaporkan `deleted: false`. `rc rb --force` saja menolak (exit 6) selama ada upload yang belum selesai; perlu `--dangerous --yes` (diamati manual). |
| Pemeriksaan kapasitas | Bucket tanpa kuota, total node, kenaikan kuota di atas total, dan batas ruang bebas, semuanya `bucket_capacity_exceeded` tanpa membuat user. |
| Container tidak ada | `bucket_admin_unavailable`. Docker 29.10 mengembalikan exit 1 (bukan 125 sampai 127) untuk container yang tidak ada atau berhenti, jadi Agent juga mengenali pesan `Error response from daemon`. |
| Secret di `ps` host dan log RustFS | Tidak muncul (sekitar 800 sampel `ps` selama test, log level `info`). |

Perilaku RustFS 1.0.6 yang perlu diketahui:

- Tulisan pertama ke sebuah bucket setelah kuotanya diset atau diubah dijawab `503 SlowDown` sekali; tulisan berikutnya berhasil. SDK S3 me-retry `503 SlowDown` secara otomatis, dan test ini juga me-retry sekali. Ini terjadi setelah `CreateBucket` dan setiap `SetBucketQuota`.
- `rc mb` pada bucket yang sudah ada tetap sukses, jadi Agent menentukan `created` dari daftar bucket, bukan dari `mb`.
- `rc quota set` pada bucket yang tidak ada gagal dengan exit 3 (seperti error jaringan), jadi Agent memeriksa keberadaan bucket lebih dulu.
- `rc admin policy create` menimpa policy dengan nama yang sama, jadi policy selalu bisa disamakan lagi.
- Kode keluar rc 0.1.37: 3 jaringan, 4 root key ditolak, 5 tidak ditemukan. Perintah `rc admin ...` memakai exit 1 untuk instance yang tidak terjangkau, dengan `"type": "network_error"` di output-nya; Agent mengenali penanda itu (dan `auth_error`) sebagai `bucket_admin_unavailable`. `timeout` di container keluar dengan 124, juga `bucket_admin_unavailable`.
