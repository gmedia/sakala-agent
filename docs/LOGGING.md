# Logging

Agent memakai `tracing` dan `tracing-subscriber` dengan output JSON agar log mudah dikonsumsi ketika berjalan sebagai process node.

## Log Level

Atur filter melalui:

```dotenv
SAKALA_LOG_LEVEL=info
```

Nilai dapat memakai filter tracing yang lebih spesifik untuk debugging lokal.

## Redaction

Sebelum deployment log dikirim ke `sakala-api`, core meredaksi key sensitif secara case-insensitive, baik dalam format env, header, maupun JSON. Cakupan awal meliputi:

```txt
TOKEN=
PASSWORD=
SECRET=
APP_KEY=
DATABASE_URL=
AUTHORIZATION:
API_KEY=
ACCESS_TOKEN=
REFRESH_TOKEN=
CLIENT_SECRET=
```

Contoh:

```txt
DATABASE_URL=postgres://user:pass@db/app
DATABASE_URL=[REDACTED]
```

Bearer token dan prefix token GitHub umum (`ghp_`, `gho_`, `github_pat_`) juga disamarkan. Untuk `DeployProject`, nilai environment command itu ikut disamarkan pada log, event, dan pesan `fail`, kecuali nilai di bawah 8 karakter dan kata umum seperti `production`. Redaction ini bersifat defense-in-depth, bukan pengganti desain yang mencegah secret masuk output sejak awal. Jangan log bearer token maupun environment dump.

## Retention Contract Draft

Agent tidak menyimpan deployment log secara persisten. Agent mengirim baris yang sudah diredaksi ke `sakala-api`, dan capture internal setiap stream dibatasi 1 MiB agar output subprocess tidak menghabiskan memory node.

Draft pilot untuk control plane:

- `sakala-api` menjadi source of truth retention, bukan agent.
- Simpan maksimal 7 hari atau 5 MiB per deployment, mana yang tercapai lebih dahulu.
- Penghapusan harus berjalan sebagai job terjadwal dan dapat dikonfigurasi operator.
- Metadata failure summary boleh disimpan lebih lama daripada raw log.
- User harus diberi tahu bahwa log lama dapat dihapus dan bukan archival storage.

Angka tersebut adalah default pilot yang harus divalidasi berdasarkan kapasitas storage dan kebutuhan debugging sebelum layanan publik.

## Foundation Behavior

Setiap lifecycle command menghasilkan log terstruktur dengan `command_id`,
`project_id`, `deployment_id`, `command_type`, outcome, dan `elapsed_ms`.
Payload command, nilai environment, dan credential repository tidak menjadi
field telemetry. Ini memungkinkan korelasi log node dengan event/deployment di
control plane tanpa menambah endpoint observability publik.

Local mode hanya menulis startup, heartbeat tick, polling tick, dan shutdown. Noop executor baru menghasilkan deployment logs jika dipanggil dalam connected command lifecycle.

Docker runtime mengambil maksimal 100 baris startup setelah health check, lalu menjalankan `docker logs --follow --tail 0` sebagai task background. Follower memakai reporter command yang sama, tetap melewati redaction core, dan tidak memiliki subprocess timeout karena lifecycle-nya mengikuti container. `RuntimeExecutor::shutdown` membatalkan seluruh follower dan process group sebelum binary berhenti.

Baris log tidak dikirim satu per satu. Reporter mem-buffer baris yang sudah
diredaksi dan mengirim batch `{ "logs": [...] }` saat mencapai
`min(log_bounds.max_batch_lines, 100)` baris atau 512 KiB message, atau paling
lambat 200 ms setelah baris pertama masuk buffer. Memasukkan baris ke buffer
tidak pernah menunggu jaringan, jadi API yang lambat atau mati tidak menahan
output build atau container. Satu worker per command mengirim batch satu per
satu sehingga urutan baris dipertahankan.

Setiap batch membawa `Idempotency-Key` unik dan isinya tidak berubah sampai
di-ack. Kegagalan transport (koneksi, timeout, body tidak terbaca) dan respons
`408`/`429`/`5xx` di-retry dengan key dan isi yang sama, jadi bila response
pertama hilang setelah API menyimpan batch, API menjawab sequence yang sama
dan tidak menyimpan baris dua kali. Jeda retry dimulai 1 detik, berlipat dua
sampai 15 detik, ditambah jitter acak sampai sebesar jeda itu (jadi paling lama
30 detik), dan tidak kurang dari `Retry-After` dari server (dibatasi 5 menit).
Retry berjalan tanpa batas jumlah selama command berjalan. Pada staging
2026-11-19 API tidak terjangkau sekitar 80 detik; reporter lama berhenti
setelah tiga percobaan dan deployment kehilangan semua baris berikutnya,
termasuk baris `[release]`.

Buffer per command menampung paling banyak 10.000 baris dan 2 MiB message di
luar satu batch yang sedang dikirim (paling besar 512 KiB). Bila penuh, baris
**tertua** di buffer yang dibuang, karena bagian akhir output (langkah yang
gagal, release command) paling menjelaskan sebuah deployment. Di titik itu
timeline mendapat satu baris `system`: `<n> log lines are missing here: the
Sakala Agent could not reach the control plane in time and dropped them.`
Byte baris yang dibuang dikembalikan ke budget `max_total_bytes`.

Sebelum `complete`/`fail`, core menunggu buffer kosong paling lama 120 detik,
tetapi tidak melewati batas waktu command itu sendiri (minimal 10 detik), supaya
lease API (`command_timeout_seconds` + `SAKALA_AGENT_LEASE_GRACE_SECONDS`) tidak
habis selama menunggu. Command `Failed` tidak menerima baris baru setelah
`fail`, jadi flush ini satu-satunya kesempatan mengirim log kegagalan. Baris
yang masih tertahan saat waktu tunggu habis dianggap hilang. Bila ada baris
command itu yang hilang, karena buffer penuh, ditolak, atau waktu tunggu habis,
Agent mencatat warning `deployment logs could not be fully delivered` dengan
jumlahnya (`<n> log lines were not delivered`). Hasil command tetap ditentukan
runtime, bukan transport log. Setelah itu reporter tetap menerima baris baru,
karena output container deployment yang sukses terus diikuti di bawah command
yang sama.

Follower bukan storage. Respons `409` (command terminal atau bukan node
pemilik), `413`, `422` (budget `max_total_bytes` habis), `4xx` lain, dan `200`
tanpa acknowledgement yang valid menghentikan delivery log command tersebut
tanpa retry: baris di buffer dibuang (warning `deployment log delivery
stopped` dengan `dropped_lines`) dan reporter menolak baris berikutnya. Baris
yang masih di buffer saat Agent shutdown tidak dikirim. Resume cursor setelah
restart Agent tetap menjadi pekerjaan lanjutan.
