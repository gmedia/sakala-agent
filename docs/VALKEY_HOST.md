# Valkey Host

Agent bisa menjalankan Valkey terkelola untuk project (ADR-023): satu container
Valkey per store, di network internal milik store itu dengan alamat IP tetap, dan
setiap container project yang memasangnya ikut bergabung ke network itu saat dibuat.
Peran ini dinyalakan dengan `SAKALA_VALKEY_DRIVER=docker`, memakai binary yang sama
dengan runtime node, dan mati secara default.

## Kenapa binary yang sama, dan kenapa harus driver Docker

Seperti [Database Host](DATABASE_HOST.md) dan [Object Storage Host](OBJECT_STORAGE_HOST.md),
peran ini punya port sendiri, `ValkeyAdmin` di `sakala-agent-core`, dengan adapter
`DockerValkeyAdmin` di `sakala-agent-runtime`, karena ia memegang hak lain: password
admin setiap store. Polling, claim, heartbeat, redaksi, dan shutdown tetap kode yang
sama.

Bedanya, store berbagi bridge network dengan workload, jadi peran ini mensyaratkan
`SAKALA_RUNTIME_DRIVER=docker`. Kombinasi lain membuat Agent berhenti saat start.
Node dengan peran ini mengiklankan capability `managed-valkey` setelah capability
runtime, `managed-postgres`, dan `managed-s3`.

## Bentuk satu store

| Bagian | Nama | Isi |
| --- | --- | --- |
| Network | `sakala-valkey-<valkey_id>` | Bridge `--internal` (tanpa rute keluar) dengan satu /27 dari `SAKALA_VALKEY_NETWORK_POOL`. Docker memakai `.1` untuk bridge, store selalu di `.2`, sisanya untuk container project. Label `dev.sakala.managed=true`, `dev.sakala.network-kind=valkey`, `dev.sakala.valkey-id`, `dev.sakala.agent-id`. |
| Container | `sakala-valkey-<valkey_id>` | Image `SAKALA_VALKEY_IMAGE`, `valkey-server /etc/valkey/valkey.conf`. Label `dev.sakala.valkey-id`, `dev.sakala.valkey-mode`, `dev.sakala.agent-id`. |
| Volume | `sakala-valkey-<valkey_id>` | `/data` store. Docker mengisinya dari `/data` image, jadi pemiliknya langsung uid 999 image. Mode cache tidak menulis apa pun ke sana. |
| File di node | `<SAKALA_VALKEY_DATA_DIR>/<valkey_id>/` | `store.json` (catatan Agent: mode, ukuran, lock, subnet, hash password tenant), `admin.secret` (password `sakala-admin`, 0600, tidak pernah di-mount), dan `conf/` (0755, di-mount read-only ke `/etc/valkey`) berisi `valkey.conf` dan `users.acl` (0644, hanya hash SHA-256). |

Catatan di `store.json` adalah sumber kebenaran Agent tentang store itu. Setiap
command menulis catatan dan file konfigurasi lebih dulu, lalu menyamakan Docker dan
Valkey dengannya, jadi command yang gagal di tengah selesai bila dikirim ulang.

Kenapa volume Docker untuk data, bukan direktori di `SAKALA_VALKEY_DATA_DIR`: Agent
berjalan sebagai user biasa (`ubuntu` di unit systemd deployment, dengan Docker
rootless di staging), jadi ia tidak bisa memberikan direktori host kepada uid 999.
Volume bernama diisi Docker dari image dengan pemilik yang benar, di Docker rootful
maupun rootless, dan `docker volume rm` menghapusnya apa pun pemilik file di dalamnya.

Kenapa label `dev.sakala.valkey-id`, bukan `dev.sakala.role=valkey` seperti di
dokumen desain: guard kapasitas menghitung setiap container ber-label
`dev.sakala.role` sebagai container sekali jalan, sedangkan store tidak dihitung ke
`SAKALA_MAX_ACTIVE_CONTAINERS` (ADR-023). Store juga tidak membawa
`dev.sakala.managed`, jadi tidak ada listing workload yang melihatnya.

## Konfigurasi

| Variable | Default | Keterangan |
| --- | --- | --- |
| `SAKALA_VALKEY_DRIVER` | `none` | `none` atau `docker`. `docker` butuh `SAKALA_RUNTIME_DRIVER=docker`. |
| `SAKALA_VALKEY_IMAGE` | `valkey/valkey:9.2.0-alpine@sha256:c8e2d17498838af5d91e12eb7c11220b3434a7b7131a333e3af52ad5e0b80882` | Wajib dipin dengan digest: `<name>[:tag]@sha256:<64 hex huruf kecil>`. Digest di atas adalah index multi-platform `9.2.0-alpine`, sama dengan `9-alpine` per 2026-11-25. |
| `SAKALA_VALKEY_NETWORK_POOL` | tidak ada | Wajib. Rentang IPv4 dalam bentuk CIDR yang ditulis di alamat network-nya, prefix /27 atau lebih pendek, di luar `0.0.0.0/8`, `127.0.0.0/8`, `169.254.0.0/16`, dan `224.0.0.0/3`. Contoh `10.213.0.0/16` (2.048 store). Production memakai LAN `10.0.47.0/24`, jadi periksa rute host dan `default-address-pools` Docker sebelum memilih. |
| `SAKALA_VALKEY_TOTAL_MEMORY_MB` | tidak ada | Wajib, bilangan bulat di atas 0. Σ(`maxmemory_mb` + 64) semua store di node ini tidak boleh melewatinya. Dikirim sebagai `valkey.capacity_mb`. Keputusan production: 384. |
| `SAKALA_VALKEY_DATA_DIR` | `/var/lib/sakala/valkey` | Path absolut tanpa koma, tanda sama dengan, tanda kutip, atau baris baru (dipakai di opsi `--mount`). Harus bisa ditulis Agent; unit systemd deployment sudah memberi `ReadWritePaths=/var/lib/sakala`. |

Store memakai runtime workload, `SAKALA_CONTAINER_RUNTIME` (`runsc` di node hosted).
Nilai yang hilang atau tidak valid membuat Agent berhenti saat start. Docker yang
belum menjawab tidak menghentikan Agent: Agent mencatat `WARN`, heartbeat
melaporkan `valkey.available: false` dan node `degraded`, dan command Valkey gagal
dengan `valkey_unavailable` sampai Docker menjawab.

Saat start Agent juga memeriksa apakah pool bertumpang tindih dengan
`default-address-pools` Docker atau rute host (`/proc/net/route`) dan mencatat `WARN`
bila ya. Saat membuat store, setiap /27 yang dipakai network Docker mana pun, rute
host, atau store lain dilewati, jadi tumpang tindih tidak merusak, hanya mengurangi
isi pool.

## Memori: batas container dan anggaran node

Ada dua angka yang berbeda:

| Angka | Rumus | Store 32 / 64 / 128 MB | Gunanya |
| --- | --- | --- | --- |
| Batas memori container (`--memory`, `--memory-swap`) | `2 × maxmemory + 64` MiB, kedua mode | 128 / 192 / 320 | Plafon sebelum OOM kill. Bukan reservasi. |
| Hitungan anggaran node | `maxmemory + 64` MiB per store | 96 / 128 / 192 | Dijumlahkan dan dibandingkan dengan `SAKALA_VALKEY_TOTAL_MEMORY_MB`. |

Dasar rumus batas container, diukur di bawah gVisor di staging (spike S1,
2026-11-24): store persisten `maxmemory 64mb` yang diisi sampai menolak tulis memakai
155 MiB, dan AOF rewrite dengan data 26 MB memuncak di 103 MiB. `2 × 64 + 64 = 192` MiB
menyisakan 37 MiB di atas store penuh. ADR-023 memakai `maxmemory + 64` untuk mode
cache, yaitu 128 MiB untuk store 64 MB, di bawah 155 MiB tadi. Store cache memang
tidak fork dan tidak punya cache file AOF, tetapi pemakaian penuh dengan eviction
terus-menerus belum diukur, jadi mode cache memakai rumus yang sama. Batas yang lebih
longgar tidak mengubah anggaran node, yang tetap Σ(`maxmemory` + 64) seperti ADR-023.

Contoh dengan anggaran production 384 MB: dua store 64 MB terhitung 256, store ketiga
64 MB tepat 384 dan diterima, store ketiga 128 MB (448) ditolak dengan
`valkey_capacity_exceeded`. Pemakaian nyata dua store penuh sekitar 2 × 155 = 310 MiB.

CPU setiap store 250 millicore (`--cpus 0.25`), PID 64, `maxclients 64`.

## Container store

```text
docker run --detach --name sakala-valkey-<id> --network sakala-valkey-<id> --ip <subnet>.2
  [--runtime runsc] --restart unless-stopped --read-only --tmpfs /tmp:rw,noexec,nosuid,size=16m
  --security-opt no-new-privileges:true --cap-drop ALL --user 999:1000
  --memory <limit>m --memory-swap <limit>m --cpus 0.25 --pids-limit 64
  --mount type=bind,source=<data_dir>/<id>/conf,target=/etc/valkey,readonly
  --mount type=volume,source=sakala-valkey-<id>,target=/data
  --label dev.sakala.valkey-id=<id> --label dev.sakala.valkey-mode=<mode> --label dev.sakala.agent-id=<agent>
  <SAKALA_VALKEY_IMAGE> valkey-server /etc/valkey/valkey.conf
```

- `--user 999:1000` wajib: sebagai root, entrypoint image berganti user (`su-exec` di spike, `setpriv` di 9.2.0), dan itu gagal di bawah `--cap-drop ALL`. Dengan uid bukan root entrypoint langsung menjalankan Valkey. Store tidak menyimpan capability apa pun.
- Network `--internal`: store, dan Lua tenant yang lolos dari sandbox, tidak punya jalan ke mana pun selain container di network itu.

`valkey.conf` yang ditulis Agent:

| Setelan | Cache | Persisten |
| --- | --- | --- |
| `maxmemory` | ukuran store dalam byte | sama |
| `maxmemory-policy` | `allkeys-lru` | `noeviction` |
| `appendonly` | `no` | `yes`, dengan `appendfsync everysec` |
| `save` | `""` | `""` (snapshot RDB hanya lewat backup) |
| `aclfile` | `/etc/valkey/users.acl` | sama |
| `maxclients` | 64 | 64 |

`users.acl`:

```text
user default on #<sha256 password tenant> ~* &* +@all -@admin
user sakala-admin on #<sha256 password admin> ~* &* +@all
```

Tenant memakai user `default` dengan semua perintah kecuali `@admin` (CONFIG, DEBUG,
SHUTDOWN, REPLICAOF, SAVE, BGSAVE, MONITOR, ACL SETUSER, CLIENT KILL, dan seterusnya),
jadi ia tidak bisa menaikkan `maxmemory`, mematikan persistensi, atau mengganti
password. `FLUSHDB`, `FLUSHALL`, `KEYS`, `SCAN`, `INFO`, `SELECT`, dan Lua tetap boleh.
Lock mengganti `on` menjadi `off`. Password admin per store dibuat Agent (64 karakter
heksadesimal dari dua UUID acak) saat store pertama kali dibuat.

## Cara Agent berbicara dengan Valkey

```text
docker exec -e VALKEYCLI_AUTH sakala-valkey-<id> timeout <detik> valkey-cli --no-auth-warning -e --user sakala-admin <perintah>...
```

- Password admin dibaca dari `admin.secret` dan masuk lewat environment proses anak (`-e VALKEYCLI_AUTH`, nama saja). Tidak pernah di argumen proses.
- `-e` membuat balasan error keluar dengan exit 1; Valkey 9.2.0 tanpa `-e` keluar 0 dan mencetak error di stdout. Agent tetap memeriksa isi balasan (`OK`, `PONG`, angka).
- `timeout` milik container menghentikan `valkey-cli` 5 detik sebelum Agent berhenti menunggu, karena membunuh `docker exec` tidak menghentikan proses di container.
- Setiap command mendapat jatah waktu `SAKALA_COMMAND_TIMEOUT_SECONDS` dikurangi 15 detik: paling lama 30 detik per panggilan Docker, 300 detik untuk `docker run` (bisa menarik image), 15 detik per `valkey-cli`, dan 60 detik menunggu store menjawab `PING` setelah start.
- Command Valkey, pembacaan heartbeat, dan rekonsiliasi berjalan satu per satu per node, dengan lock sendiri.

## Yang dikerjakan setiap command

`CreateValkey` untuk store baru memeriksa anggaran node, memilih /27 bebas, menulis
catatan dan file, membuat network, volume, dan container, lalu menunggu `PING`. Untuk
store yang sudah ada: mode atau ukuran yang berbeda gagal dengan `valkey_conflict`;
selain itu password dari payload dipasang lagi (`ACL LOAD`, dan koneksi tenant
ditutup bila hash-nya berubah), lock tetap, dan bagian yang hilang dibuat lagi di
subnet yang sama. Container yang image atau batas memorinya tidak sama dengan
setelan node dibuat ulang; ini juga cara menerapkan image baru (misalnya rilis
keamanan) ke store yang sudah ada: kirim ulang `CreateValkey` yang sama. Data store
persisten tetap di volume. Store cache kosong setelah dibuat ulang.

`RotateValkeyPassword` menulis hash baru, `ACL LOAD`, lalu `CLIENT KILL USER default`,
karena di Valkey 9.2.0 `ACL LOAD` tidak memutus koneksi yang sudah login (diamati di
smoke test). Result membawa jumlah koneksi yang ditutup.

`LockValkey` memasang `off` pada `default`, `ACL LOAD`, lalu menutup koneksi tenant.
`UnlockValkey` memasang `on`. Data tetap ada dan user admin tetap bisa masuk, jadi
backup tetap jalan.

`ResizeValkey` memeriksa anggaran dengan ukuran baru menggantikan ukuran lama. Store
persisten menolak ukuran di bawah `used_memory` dengan `valkey_size_below_usage`;
store cache menerima dan mengusir key. Saat mengecil, `CONFIG SET maxmemory` dulu lalu
`docker update --memory`; saat membesar urutannya dibalik, supaya Valkey tidak pernah
melewati batas cgroup-nya. Bila runtime menolak `docker update`, container dibuat
ulang dengan batas baru (`restarted: true`). Apakah `runsc` mendukung `docker update`
belum diuji.

`DeleteValkey` menolak dengan `valkey_in_use` selama ada container lain yang berjalan
(`running`, `restarting`, `paused`) di network store. Container lain yang berhenti,
misalnya container project yang sedang tidur, diputus dari network
(`docker network disconnect --force`), supaya ia tetap bisa dinyalakan setelah network
hilang. Lalu container, network, volume, dan direktori store dihapus. Backup tidak
disentuh.

Command untuk store yang container-nya tidak berjalan menulis file-nya lalu gagal
dengan `valkey_unavailable`; perubahan itu berlaku saat store menyala lagi.

## Field `valkey` di `DeployProject` dan `RunJob`

API mengirim `valkey: { network, host }` untuk project yang memasang store. Sebelum
apa pun dibuat, dan untuk deploy sebelum build, Agent memeriksa dengan satu
`docker network inspect` bahwa network itu ada, ber-label `network-kind=valkey` dan
`agent-id` Agent ini, dan store-nya ada di `host`. Bila tidak, deploy atau job gagal
dengan `valkey_network_unavailable` dan tidak ada container yang dibuat.

Setiap container deployment (replika web, worker, release command) dan setiap
container job dibuat dengan dua network sekaligus:
`docker run --network sakala-project-<p> --network sakala-valkey-<id>`. Network
project tetap memegang satu-satunya rute keluar, jadi filter egress tidak berubah.
Agent tidak pernah memakai `docker network connect` untuk container yang sudah
berjalan, karena di bawah `runsc` hanya pembuatan dengan semua network sekaligus yang
terbukti (spike S1). Akibatnya memasang atau melepas store berlaku di deployment
berikutnya, sama seperti perubahan variabel. Sleep, wake, dan restart memakai
container yang sama, jadi network-nya ikut.

Variabel `REDIS_*` dibangun API dan masuk lewat `environment` seperti variabel lain;
Agent tidak membuatnya.

## Heartbeat

`metadata.valkey` (`capacity_mb`, `available`) dikirim di setiap heartbeat.
`metadata.valkeys` dikirim paling sering sekali per 60 detik, setelah satu
`docker inspect` semua container store dan satu `INFO memory stats clients errorstats
persistence` per store yang berjalan (batas 5 detik per store). Store yang tidak
berjalan atau tidak menjawab tetap dilaporkan, dengan bacaan `INFO` bernilai `null`.
Pembacaan yang menemukan command sedang berjalan menunggu 2 detik lalu dilewati tanpa
dianggap gagal. Kontraknya ada di [Sakala Agent API](AGENT_API.md#heartbeat-valkey-host).

## Rekonsiliasi

Setiap 60 detik, bersama rekonsiliasi network project dan hanya saat tidak ada
command yang berjalan, Agent menghapus network store miliknya yang tidak punya
catatan store dan tidak dipakai container apa pun. Container dan volume store tanpa
catatan hanya dicatat sebagai `WARN` dan tidak pernah dihapus otomatis, karena volume
bisa berisi data; `DeleteValkey` untuk ID itu menghapusnya. Store yang punya catatan
tetapi container-nya hilang dilaporkan `status: "missing"`; `CreateValkey` yang sama
membuatnya lagi.

## Backup dan restore (mode persisten)

Backup dikerjakan skrip deployment, bukan Agent (rencana ADR-023 langkah 5). Yang
disediakan Agent:

- Store persisten ber-label `dev.sakala.valkey-mode=persistent`; `store.json` menyimpan mode yang sama.
- Snapshot RDB lewat user admin, tanpa menyentuh file di volume:

  ```bash
  VALKEYCLI_AUTH="$(cat /var/lib/sakala/valkey/<id>/admin.secret)" \
    docker exec -e VALKEYCLI_AUTH sakala-valkey-<id> \
    valkey-cli --no-auth-warning --user sakala-admin --rdb - > <id>.rdb
  ```

  `--rdb -` menulis ke stdout di Valkey 9.2.0 (diamati: diawali `REDIS0011`). Skrip
  lalu mengenkripsinya dengan age ke kunci yang sama dengan backup lain, menyimpan 7 set
  di node, dan app host menarik 14 set.

**RDB tidak pernah dimuat di luar sandbox.** RDB membawa library Lua tenant
(`FUNCTION`) yang dijalankan saat dimuat. Restore dan drill memuatnya di container
`runsc` sekali pakai tanpa network, dengan hardening yang sama dengan store. Prosedur
operator yang diusulkan, belum diuji (skrip dan drill-nya bagian repo deployment):

1. Dekripsi backup ke direktori sementara milik root.
2. Hentikan store (`docker stop sakala-valkey-<id>`), lalu jalankan container sekali pakai dengan volume store:
   `docker run --rm --runtime runsc --network none --read-only --cap-drop ALL --user 999:1000 --security-opt no-new-privileges:true -v sakala-valkey-<id>:/data -v <dir>/<id>.rdb:/restore/dump.rdb:ro <SAKALA_VALKEY_IMAGE> sh -c 'rm -rf /data/appendonlydir && cp /restore/dump.rdb /data/dump.rdb'`.
3. Muat di sandbox yang sama tanpa network: `valkey-server --dir /data --dbfilename dump.rdb --appendonly no`, periksa `DBSIZE`, lalu `CONFIG SET appendonly yes`, yang menulis AOF baru dari isi yang dimuat; tunggu `aof_rewrite_in_progress:0` di `INFO persistence`, kemudian hentikan.
4. Nyalakan store lagi (`docker start sakala-valkey-<id>`); ia memuat AOF baru.

Drill memakai langkah 3 pada volume sementara dan membandingkan jumlah key dan nilai
penanda.

## Rollback Agent

Agent tanpa `managed-valkey` tidak mengenal command Valkey dan mengabaikan field
`valkey`, jadi deployment berikutnya kehilangan network store tanpa error. Sebelum
menurunkan Agent: lepas dan hapus store, atau hentikan project yang memasangnya, lalu
pastikan `docker ps -a --filter label=dev.sakala.valkey-id` kosong.

## Yang dibuktikan di Docker sungguhan

```bash
cargo test -p sakala-agent-runtime --test valkey_smoke_test -- --ignored --nocapture
```

Test ini memakai image yang dipin, `caddy:2.11.3-alpine`, dan `busybox:1.37`, lewat
dispatcher yang sama dengan Agent dan kode deploy, job, sleep, dan wake executor.
Hasil pada 2026-11-25 (Valkey 9.2.0, Docker 29.10, runc di laptop WSL2):

| Yang diuji | Hasil |
| --- | --- |
| Store cache dan persisten di network internal, alamat `.2` sebuah /27 | Lolos; `--user 999:1000`, root read-only, `[ALL]` di-drop, PID 64, 0,25 CPU; policy dan `appendonly` sesuai mode. |
| `CreateValkey` diulang | `created: false`, alamat sama, tanpa container baru. |
| Replika web yang dideploy dengan `valkey` | Dibuat dengan network project dan network store; `AUTH`, `SET`, `GET`, `EVAL` jalan; password salah `WRONGPASS`, tanpa password `NOAUTH`, `CONFIG SET` `NOPERM`; store lain tidak terjangkau. |
| Job dengan `valkey` | Menjangkau store dengan variabel `REDIS_*`, exit 0. |
| Container di network lain dan di bridge default Docker | Tidak mendapat jawaban. |
| Rotasi dengan koneksi yang sedang login | Koneksi ditutup (`disconnected_clients ≥ 1`), password lama `WRONGPASS`, password baru `PONG`. |
| Lock | Tenant `WRONGPASS`, data tetap terbaca admin, `CreateValkey` diulang tetap `locked: true`, unlock memulihkan. |
| Restart container | Store persisten menyimpan data dan password; store cache kosong. |
| Resize 32 ke 64 MB | `maxmemory` 67108864 dan batas container 192 MiB di tempat, `restarted: false`. |
| Resize store persisten berisi sekitar 30 MB ke 16 MB | `valkey_size_below_usage`, `maxmemory` tetap. |
| Heartbeat, juga dari Agent yang baru start | Semua field kedua store; `aof_last_write_status` `ok` untuk persisten dan `null` untuk cache; `errorstat_oom` 0. |
| Delete selama replika web berjalan di network store | `valkey_in_use` yang menyebut container itu; store tetap berjalan. |
| Delete setelah project tidur | Container tidur diputus dari network; container, network, volume, dan direktori store hilang; project bangun hanya dengan network project-nya; delete diulang `deleted: false`. |

Yang belum dibuktikan di sini: semua hal di bawah `runsc` (network internal, alamat
tetap, workload dua network, dan memori sudah dibuktikan manual di spike S1 staging,
tetapi bukan kode Agent ini), `docker update` di bawah `runsc`, store cache yang penuh
dengan eviction terus-menerus, dan prosedur restore.
