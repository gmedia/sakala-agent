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
- MVP menerima repository publik GitHub tanpa credential dan immutable commit SHA lengkap.
- Domain command dibatasi ke `*.run.sakala.localhost` dan `*.run.sakala.dev`.
- Container memakai resource limits dari command API yang telah diverifikasi terhadap hard maximum node, `no-new-privileges`, dan drop seluruh Linux capabilities.
- Runtime environment ditulis sementara dengan mode `0600`, tidak dimasukkan ke command arguments, lalu dihapus setelah container dibuat.
- Deployment logs melewati basic secret redactor.
- Output subprocess dikirim per baris dan capture internal dibatasi 1 MiB per stream.

## Batas MVP Sebelum Pilot Tidak Tepercaya

- Tambahkan autentikasi/lease/idempotency tests dengan agent API.
- Review redaction untuk format secret tambahan.
- Dokumentasikan recovery jika command berhenti di tengah eksekusi.
- Tambahkan timeout, cancellation, dan orphan reconciliation.
- Evaluasi rootless Docker atau builder yang lebih terisolasi. Dockerfile user pada daemon host belum merupakan boundary multi-tenant yang kuat.
- Gunakan secret delivery yang tidak menulis value ke build context; Phase 8 hanya mendukung runtime environment.
