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

Local mode hanya menulis startup, heartbeat tick, polling tick, dan shutdown. Noop executor baru menghasilkan deployment logs jika dipanggil dalam connected command lifecycle.

Docker runtime mengambil maksimal 100 baris startup setelah health check, lalu menjalankan `docker logs --follow --tail 0` sebagai task background. Follower memakai reporter command yang sama, tetap melewati redaction core, dan tidak memiliki subprocess timeout karena lifecycle-nya mengikuti container. `RuntimeExecutor::shutdown` membatalkan seluruh follower dan process group sebelum binary berhenti.

Follower bukan storage. Jika API tidak dapat menerima log, follower berhenti dan menulis warning pada log operator agar kegagalan reporting tidak tumbuh menjadi retry loop tanpa batas. Resume cursor dan reconnect terkontrol menjadi pekerjaan lanjutan setelah endpoint log menyediakan kontrak sequence/cursor.
