# Security Policy

Sakala Agent nantinya menjalankan operasi privileged pada runtime node. Foundation saat ini tidak mengakses runtime host, tetapi boundary keamanan berikut wajib dipertahankan.

## Reporting Vulnerabilities

Jangan membuka issue publik untuk kerentanan yang dapat dieksploitasi. Laporkan secara privat kepada maintainer Sakala melalui kanal security repository ketika tersedia, dengan langkah reproduksi dan dampak tanpa membagikan secret nyata.

Pada fase MVP, GMEDIA dapat membantu triage keamanan sebagai founding sponsor dan infrastructure supporter.

## Docker Socket Boundary

- `sakala-api` dan `sakala-console` tidak boleh mengakses Docker socket.
- Caddy atau aplikasi user tidak boleh menerima Docker socket.
- Foundation agent tidak memakai Docker client crate atau Docker socket.
- Akses Docker agent di masa depan harus dibatasi, terdokumentasi, dan direview sebagai perubahan berisiko tinggi.

## Agent Tokens

- Agent melakukan autentikasi menggunakan bearer token dan `X-Agent-Id`.
- Jangan commit token aktual ke `.env`, contoh, test fixture, atau log.
- Connected mode menolak placeholder `change-me`.
- `sakala-api` harus menyimpan token agent dalam bentuk hash dan mendukung rotasi.

## Logs dan Secrets

Agent meredaksi nilai yang muncul sebagai:

```txt
TOKEN= PASSWORD= SECRET= APP_KEY= DATABASE_URL=
```

Redaksi dasar ini bukan jaminan semua format secret tertutup. Runtime implementation mendatang harus menghindari mencetak environment values sejak sumbernya.

## Command Safety

Command berasal dari control plane dan tetap harus divalidasi agent sebelum operasi host. Implementasi runtime mendatang wajib mempertimbangkan idempotensi, route ownership, resource boundaries, path traversal, command injection, dan isolasi workload.

## Scope Saat Ini

Foundation ini belum menyediakan production hardening, mTLS, token rotation, container isolation, Caddy reload, image verification, atau sandbox runtime.
