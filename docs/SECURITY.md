# Security Notes

Dokumen ini melengkapi [../SECURITY.md](../SECURITY.md) dengan catatan implementasi untuk contributor runtime.

## Privilege Boundary

Agent adalah satu-satunya komponen Sakala yang pada masa depan boleh menyentuh runtime node. Hak tersebut tidak boleh mengalir ke API, console, atau aplikasi user.

## Runtime Guarantees

- Default `local` mode tidak menghubungi control plane.
- Connected mode memerlukan token non-placeholder.
- Request connected membawa bearer token dan `X-Agent-Id`.
- Executor default tetap noop dan Docker hanya aktif melalui `SAKALA_RUNTIME_DRIVER=docker`.
- Docker diakses oleh proses agent pada runtime node; socket tidak pernah dipasang ke API, console, Caddy, atau container aplikasi user.
- Repository GitHub publik memakai checkout credential-free; repository privat memakai credential sementara melalui `GIT_ASKPASS`, tidak pernah melalui URL remote atau argument proses. Keduanya wajib memakai immutable commit SHA lengkap.
- Domain command harus berupa tepat satu label DNS huruf kecil di bawah suffix pada `SAKALA_PROJECT_DOMAIN_SUFFIXES`, yang default-nya `run.sakala.localhost`, `run.staging.sakala.dev`, dan `run.sakala.dev`. Subdomain bertingkat, huruf besar, Unicode, dan nama di atas batas panjang RFC 1035 ditolak. Suffix divalidasi saat start: wajib nama DNS minimal dua label tanpa wildcard, dan nilai yang tidak valid menghentikan Agent. Lihat [Domain project](CONFIGURATION.md#domain-project).
- Container memakai resource limits dari command API yang telah diverifikasi terhadap hard maximum node, `no-new-privileges`, dan `--cap-drop ALL` dengan allowlist delapan capability: `CHOWN`, `DAC_OVERRIDE`, `FOWNER`, `FSETID`, `KILL`, `SETGID`, `SETUID`, dan `NET_BIND_SERVICE`. Set ini dibutuhkan image yang mulai sebagai root lalu turun ke user tanpa privilege (nginx, httpd, `gosu`, `su-exec`, supervisord, php-fpm) dan biner bertanda file capability seperti FrankenPHP. `NET_RAW` tetap dibuang karena memungkinkan paket palsu ke tenant lain di bridge yang sama, begitu juga `MKNOD`, `SYS_CHROOT`, `AUDIT_WRITE`, `SETFCAP`, dan `SETPCAP`. Lihat [Docker Runtime](DOCKER_RUNTIME.md#capability-workload).
- Setiap project memakai bridge network sendiri, dan hanya container Caddy runtime yang disambungkan ke semua network project. Aplikasi tidak bisa menjangkau aplikasi project lain lewat nama maupun IP tanpa melewati Caddy. Workload dari Agent lama tetap di `sakala-runtime` sampai redeploy berikutnya. Lihat [Network per project](DOCKER_RUNTIME.md#network-per-project).
- Runtime environment ditulis sementara dengan mode `0600`, tidak dimasukkan ke command arguments, lalu dihapus setelah container dibuat.
- Deployment logs melewati redactor case-insensitive untuk env-style, JSON, authorization header, bearer token, dan prefix token GitHub umum.
- Output subprocess diredaksi per baris, dikirim dalam batch terbatas (100 baris/512 KiB per request) dengan `Idempotency-Key`, dan capture internal dibatasi 1 MiB per stream.
- Build dan command memiliki deadline terpisah. Timeout atau pembatalan mematikan process group agar subprocess tidak tertinggal.
- Startup melakukan detection-only reconciliation terhadap container managed yang berhenti atau kehilangan identity label. Agent tidak menghapus orphan otomatis tanpa desired state dari control plane.
- Inspection project dengan Dockerfile tanpa `EXPOSE` membaca `Config.ExposedPorts` base image dari registry, tanpa menarik image atau build. Lookup hanya ke `docker.io`, `ghcr.io`, `quay.io`, `public.ecr.aws`, `mcr.microsoft.com`, dan `registry.gitlab.com`; host lain, alamat IP, `localhost`, dan host dengan port dilewati tanpa network call. Lookup memakai `DOCKER_CONFIG` kosong sekali pakai dan tidak mewarisi `DOCKER_*`, `BUILDX_*`, atau `BUILDKIT_*`, jadi credential registry node maupun tenant tidak pernah terpakai. Batasnya 10 detik, satu referensi per inspection, dan kegagalan hanya berarti tidak ada port. Lihat [Docker Runtime](DOCKER_RUNTIME.md#port-dari-base-image).
- Node menolak project baru saat guard `SAKALA_MAX_ACTIVE_CONTAINERS` tercapai, tetapi kuota user/workspace tetap ditentukan API.

## Klasifikasi Kegagalan Repository

Kegagalan Git dilaporkan dengan code stabil tanpa meneruskan output stderr Git
ke message error. Hal ini menjaga token, URL, atau detail host privat tetap
berada di luar completion report. Code yang tersedia adalah:

```text
repository_not_found
repository_access_denied
repository_auth_failed
repository_credential_expired
repository_commit_not_found
repository_checkout_failed
```

Pemetaan dilakukan dari error Git yang umum; API tetap bertanggung jawab untuk
menentukan proses refresh credential atau retry command.

## Batas MVP Sebelum Pilot Tidak Tepercaya

- Tambahkan autentikasi/lease/idempotency tests dengan agent API.
- Review redaction untuk format secret tambahan.
- Dokumentasikan recovery jika command berhenti di tengah eksekusi.
- Integrasikan desired-state reconciliation dari `sakala-api` sebelum mengaktifkan auto-removal orphan.
- Rootless Docker mengurangi privilege daemon, tetapi tidak otomatis menjadi sandbox multi-tenant. Sebelum menerima workload publik tidak tepercaya, evaluasi rootless BuildKit pada worker terpisah atau isolated remote builder dengan filesystem, network, cache, dan credential boundary sendiri.
- Variabel project sampai ke build hanya sebagai secret BuildKit, tidak lewat build argument atau build context. Lihat [Docker Runtime](DOCKER_RUNTIME.md#variabel-saat-build). Langkah build milik user tetap bisa menulis nilainya ke image.
- Railpack dan mise menjalankan kode repository saat analisis, jadi keduanya hanya berjalan di [sandbox](RAILPACK_STRATEGY.md#sandbox). Build antar-project [diisolasi](DOCKER_RUNTIME.md#isolasi-antar-project-saat-build), dan process anak tidak mewarisi `SAKALA_*`.
- Satu baris output process dibatasi 64 KiB, supaya container yang mencetak tanpa newline tidak bisa menghabiskan memory Agent.
