# Security Policy

Sakala Agent dapat menjalankan operasi privileged pada runtime node ketika executor Docker diaktifkan eksplisit. Default tetap noop.

## Reporting Vulnerabilities

Jangan membuka issue publik untuk kerentanan yang dapat dieksploitasi. Laporkan secara privat kepada maintainer Sakala melalui kanal security repository ketika tersedia, dengan langkah reproduksi dan dampak tanpa membagikan secret nyata.

Pada fase MVP, GMEDIA dapat membantu triage keamanan sebagai founding sponsor dan infrastructure supporter.

## Docker Socket Boundary

- `sakala-api` dan `sakala-console` tidak boleh mengakses Docker socket.
- Caddy atau aplikasi user tidak boleh menerima Docker socket.
- Agent mengakses daemon hanya melalui Docker CLI pada runtime node khusus.
- Aplikasi user, Caddy, API, dan console tidak menerima Docker socket.
- Runtime access harus dianggap setara host privilege dan tidak cocok untuk workload tidak tepercaya tanpa hardening lanjutan.

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

Command berasal dari control plane dan tetap divalidasi agent. Executor membatasi repository host, mewajibkan immutable SHA, memvalidasi hostname/env key, memakai structured process arguments, ownership labels, atomic route write, dan resource limits awal. API menentukan policy resource; agent menolak nilai nol atau request di atas hard maximum node dan melaporkan requested/applied limits.

## Scope Saat Ini

MVP belum menyediakan production-grade sandbox, mTLS, token rotation, image signature verification, build timeout, egress policy, rootless daemon, atau isolation per tenant.
