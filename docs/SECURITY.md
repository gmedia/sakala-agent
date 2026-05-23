# Security Notes

Dokumen ini melengkapi [../SECURITY.md](../SECURITY.md) dengan catatan implementasi untuk contributor runtime.

## Privilege Boundary

Agent adalah satu-satunya komponen Sakala yang pada masa depan boleh menyentuh runtime node. Hak tersebut tidak boleh mengalir ke dashboard atau aplikasi user.

## Foundation Guarantees

- Default `local` mode tidak menghubungi dashboard.
- Connected mode memerlukan token non-placeholder.
- Request connected membawa bearer token dan `X-Agent-Id`.
- Executor aktif adalah noop.
- Tidak ada Docker client crate atau socket mount.
- Deployment logs melewati basic secret redactor.

## Sebelum Mengaktifkan Runtime Nyata

- Tambahkan autentikasi/lease/idempotency tests dengan dashboard API.
- Tentukan pembatasan filesystem, network, container resource, dan route ownership.
- Review redaction untuk format secret tambahan.
- Dokumentasikan recovery jika command berhenti di tengah eksekusi.
