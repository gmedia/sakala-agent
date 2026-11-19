# Database Host

Agent bisa berperan sebagai host database untuk PostgreSQL terkelola (ADR-020): ia
mengelola satu instance PostgreSQL lokal, membuat satu database dan satu login role
untuk setiap database tenant, dan melaporkan ukurannya ke `sakala-api`. Peran ini
dinyalakan dengan `SAKALA_DATABASE_DRIVER=postgres` dan memakai binary yang sama
dengan runtime node.

## Kenapa binary yang sama

Peran database tidak menjadi runtime driver ketiga. `SAKALA_RUNTIME_DRIVER` memilih
siapa yang menjalankan container workload, sedangkan peran database memegang hak lain,
yaitu credential superuser instance. Karena itu ia punya port sendiri,
`DatabaseAdmin` di `sakala-agent-core`, dengan adapter
`PostgresDatabaseAdmin` di `sakala-agent-runtime`. Polling, claim, heartbeat,
redaksi, dan shutdown tetap kode yang sama.

Dua kombinasi yang didukung:

| Topologi | Konfigurasi |
| --- | --- |
| Host database khusus per region (production) | `SAKALA_RUNTIME_DRIVER=noop`, `SAKALA_DATABASE_DRIVER=postgres` |
| Single node, self-host, atau staging co-located | `SAKALA_RUNTIME_DRIVER=docker`, `SAKALA_DATABASE_DRIVER=postgres` |

Node dengan peran ini mengiklankan capability `managed-postgres` di samping
capability runtime-nya. Node `noop` tidak punya capability `dockerfile-build`,
`railpack-build`, atau `docker-runtime`, jadi API tidak menempatkan project di sana.

## Konfigurasi

| Variable | Default | Keterangan |
| --- | --- | --- |
| `SAKALA_DATABASE_DRIVER` | `none` | `none` atau `postgres`. |
| `SAKALA_DATABASE_ADMIN_DSN` | tidak ada | Wajib dengan `postgres`. DSN libpq (URL atau `key=value`) ke instance lokal sebagai superuser, misalnya `host=/var/run/postgresql user=postgres dbname=postgres`. Hanya dibaca dari environment, tidak ada CLI flag, dan tidak pernah dicetak. Semua host harus socket Unix atau alamat loopback (`localhost`, `127.0.0.1`, `::1`), karena koneksi ini tanpa TLS dan credential superuser tidak boleh keluar dari host. |
| `SAKALA_DATABASE_PUBLIC_HOST` | tidak ada | Wajib dengan `postgres`. Alamat instance seperti yang dijangkau runtime node lewat jaringan privat: IPv4, IPv6 tanpa kurung siku, atau nama DNS. Dikirim sebagai `metadata.database.endpoint.host`, dan API memakainya untuk `DB_HOST` dan `DATABASE_URL`. |
| `SAKALA_DATABASE_PUBLIC_PORT` | `5432` | Port 1 sampai 65535 untuk alamat di atas. |

Nilai yang hilang atau tidak valid membuat Agent berhenti saat start. Instance yang
belum siap tidak menghentikan Agent: Agent mencatat `WARN`, heartbeat melaporkan node
`degraded` dengan `database.available: false`, dan command database gagal dengan
`database_admin_unavailable` sampai instance menjawab.

Setiap kali membuka koneksi admin baru, Agent memeriksa bahwa user DSN adalah
superuser dan PostgreSQL minimal versi 15, lalu menjalankan
`REVOKE ALL ON DATABASE postgres, template1 FROM PUBLIC`. Tanpa itu tenant bisa
terhubung ke `postgres` dan dari sana mengubah database miliknya sendiri.

## Yang dibuat untuk setiap database

`CreateDatabase` membuat, atau menyamakan lagi bila sudah ada:

- role `LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT NOREPLICATION NOBYPASSRLS CONNECTION LIMIT 5`, dengan password yang disimpan sebagai verifier SCRAM-SHA-256;
- default role `statement_timeout = 60s`, `idle_in_transaction_session_timeout = 60s`, dan `idle_session_timeout = 30min`, yang boleh di-override tenant dengan `SET`;
- `temp_file_limit = 1GB` per sesi, yang hanya bisa diubah superuser;
- database dengan owner role itu dan `CONNECTION LIMIT 5`, lalu `REVOKE ALL ON DATABASE ... FROM PUBLIC`;
- di dalam database: `REVOKE CREATE ON SCHEMA public FROM PUBLIC` dan `GRANT ALL ON SCHEMA public TO <role>`.

Agent menghitung verifier SCRAM sendiri dari password di payload. Password plaintext
tidak pernah dikirim ke server, jadi tidak muncul di log statement PostgreSQL
walaupun `log_statement=all`. Smoke test memeriksa hal ini.

Batas koneksi berlaku dua kali dengan cara yang berbeda. Tenant tidak bisa mengubah
batas di role-nya, tetapi sebagai owner ia bisa menaikkan batas di database-nya.
Batas role tetap menahan tenant di 5 sesi. Batas database menghitung **semua** sesi
di database itu, termasuk sesi superuser seperti `pg_dump` saat backup, jadi backup
yang berjalan memakai satu dari lima slot tenant.

## Lock

`LockDatabase` mematikan login role owner (`NOLOGIN`) dan mengakhiri semua sesi role
itu. Agent sengaja tidak memakai `ALTER DATABASE ... ALLOW_CONNECTIONS false`, karena:

- dengan `ALLOW_CONNECTIONS false` superuser pun tidak bisa terhubung, sehingga backup malam dan pemeriksaan database yang penuh berhenti justru saat datanya paling penting;
- owner database boleh mengubah `ALLOW_CONNECTIONS` miliknya sendiri, sedangkan `LOGIN` hanya bisa diubah role dengan `CREATEROLE`.

Sesi user lain, misalnya backup yang berjalan sebagai superuser, tidak diakhiri.
`CreateDatabase` yang diulang tidak membuka lock, karena role yang sudah ada tidak
disentuh atribut `LOGIN`-nya. `UnlockDatabase` mengembalikan `LOGIN`.

## Extension

Allow-list: `pgcrypto`, `citext`, `pg_trgm`, `unaccent`, `hstore`, dan `vector`
(pgvector, juga diterima sebagai `pgvector`). Agent memasangnya sebagai superuser ke
schema `public` dengan `CREATE EXTENSION IF NOT EXISTS`. Extension lain ditolak
dengan `database_extension_not_allowed` sebelum SQL apa pun dijalankan.

Image `postgres:18-alpine` tidak membawa pgvector. Pada image itu `vector` gagal
dengan `database_extension_unavailable`. Pakai image yang membawanya, misalnya
`pgvector/pgvector:pg18`, atau pasang paketnya di host.

Lima extension pertama ditandai trusted oleh PostgreSQL, sehingga script-nya memang
dirancang aman untuk dipasang di schema milik user biasa. `vector` tidak trusted
(pgvector#288), dan script-nya berjalan sebagai superuser di schema `public` yang
dimiliki tenant. Risiko sisa ini diterima di tahap pertama; jaga pgvector tetap
versi terbaru.

## Hardening instance

Hal berikut bukan dikerjakan Agent, tetapi wajib di host database (repo deployment):

- PostgreSQL berjalan dengan runc, bukan runsc (ADR-020), dan port-nya tidak dipublikasikan ke internet.
- `pg_hba.conf` hanya menerima tenant dari IP privat runtime node, lewat TLS, dengan SCRAM. Koneksi admin Agent lewat socket Unix atau loopback:

  ```conf
  # TYPE   DATABASE  USER      ADDRESS          METHOD
  local    all       postgres                   peer
  host     all       postgres  127.0.0.1/32     scram-sha-256
  hostssl  all       all       10.0.47.0/24     scram-sha-256
  ```

  Ganti `10.0.47.0/24` dengan subnet atau daftar IP runtime node. Di staging co-located, alamatnya adalah jaringan Docker atau loopback yang dipakai container project.
- `password_encryption = scram-sha-256`, default sejak PostgreSQL 14.
- Batas memori instance di level container atau systemd, misalnya `--memory` sekitar 75 % RAM host. Satu tenant bisa `SET work_mem = '2GB'`, dan satu query bisa memakai beberapa kali lipat nilai itu. Batas memori dan batas koneksi membatasinya, tetapi OOM me-restart instance untuk semua tenant. Ini alasan utama opsi D di ADR-020.
- `max_connections`: sekitar 100 untuk host 2 vCPU dan 4 GB dengan `shared_buffers` sekitar 1 GB. Dengan 5 koneksi per database, itu cukup untuk sekitar 20 database yang aktif bersamaan. Sisakan beberapa slot untuk superuser dan backup lewat `superuser_reserved_connections`. Tambahkan PgBouncer bila puncak koneksi melewati sekitar 70 % `max_connections`.
- Disk minimal beberapa ratus operasi fdatasync per detik di `pg_test_fsync` sebelum melayani.
- Backup malam per database dengan `pg_dump -Fc` dan enkripsi age, disimpan di luar host. Database yang di-lock tetap bisa di-backup.

## Ukuran database di heartbeat

Heartbeat membaca `pg_database_size()` untuk setiap database yang namanya cocok
dengan `^p_[a-z0-9]{16,40}$`, paling sering sekali per 60 detik, dan mengirim
hasilnya sekali di heartbeat berikutnya sebagai `metadata.databases`. Database lain,
termasuk `postgres` dan template, tidak pernah dilaporkan. Kontraknya ada di
[Sakala Agent API](AGENT_API.md#database-host).

## Uji dengan PostgreSQL sungguhan

```bash
cargo test -p sakala-agent-runtime --test database_smoke_test -- --ignored --nocapture
```

Test ini menjalankan `postgres:18-alpine` (bisa diganti dengan
`SAKALA_SMOKE_POSTGRES_IMAGE`) di port loopback acak dan mengirim command lewat
dispatcher yang sama dengan Agent. Ia membuktikan: tenant bisa terhubung dan membuat
tabel; tenant ditolak di database tenant lain, `postgres`, dan `template1`; role lain
yang diberi `CONNECT` tetap tidak bisa `CREATE` di `public`; default role berlaku
dan `statement_timeout` membatalkan query; `temp_file_limit` dan batas koneksi role
tidak bisa dinaikkan tenant; sesi keenam ditolak; lock mengakhiri sesi dan menolak
login baru sementara superuser tetap bisa masuk; rotasi password; allow-list
extension; drop membersihkan database dan role; dan log server tanpa password tenant.
