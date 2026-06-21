# Logging

Agent memakai `tracing` dan `tracing-subscriber` dengan output JSON agar log mudah dikonsumsi ketika berjalan sebagai process node.

## Log Level

Atur filter melalui:

```dotenv
SAKALA_LOG_LEVEL=info
```

Nilai dapat memakai filter tracing yang lebih spesifik untuk debugging lokal.

## Redaction

Sebelum deployment log dikirim ke `sakala-api`, core meredaksi value setelah key berikut:

```txt
TOKEN=
PASSWORD=
SECRET=
APP_KEY=
DATABASE_URL=
```

Contoh:

```txt
DATABASE_URL=postgres://user:pass@db/app
DATABASE_URL=[REDACTED]
```

Redaction ini adalah guard dasar, bukan pengganti desain yang mencegah secret masuk output sejak awal. Jangan log bearer token maupun environment dump.

## Foundation Behavior

Local mode hanya menulis startup, heartbeat tick, polling tick, dan shutdown. Noop executor baru menghasilkan deployment logs jika dipanggil dalam connected command lifecycle.
