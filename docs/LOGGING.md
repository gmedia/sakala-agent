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

Bearer token dan prefix token GitHub umum (`ghp_`, `gho_`, `github_pat_`) juga disamarkan. Redaction ini bersifat defense-in-depth, bukan pengganti desain yang mencegah secret masuk output sejak awal. Jangan log bearer token maupun environment dump.

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
lambat 200 ms setelah baris pertama masuk buffer. Urutan baris dipertahankan.
Setiap request membawa `Idempotency-Key` unik; kegagalan transport dan respons
`408`/`429`/`5xx` di-retry dengan key yang sama (backoff 500 ms–5 s, maksimum
tiga percobaan) sehingga API mendeduplikasi item yang terkirim ganda. Sebelum
`complete`/`fail`, core mem-flush sisa buffer; kegagalan flush hanya menjadi
warning karena hasil command ditentukan runtime, bukan transport log.

Follower bukan storage. Respons `409` (command terminal atau bukan node
pemilik) dan `422` (budget `max_total_bytes` habis) menghentikan delivery log
command tersebut tanpa retry: reporter menolak baris berikutnya sehingga
follower berhenti dan menulis warning pada log operator. Kegagalan transient
yang tetap gagal setelah retry berakhir dengan cara yang sama agar kegagalan
reporting tidak tumbuh menjadi retry loop tanpa batas. Batch yang masih berada
di buffer saat follower dibatalkan pada shutdown Agent (paling banyak 200 ms
output) tidak dikirim. Resume cursor dan reconnect terkontrol menjadi pekerjaan
lanjutan setelah endpoint log menyediakan kontrak sequence/cursor.
