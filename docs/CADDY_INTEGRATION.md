# Caddy Integration Direction

`sakala-infra` saat ini menyediakan Caddy lokal dan pola domain:

```txt
*.run.sakala.localhost
```

Arah production:

```txt
*.run.sakala.dev
```

## Peran Agent Mendatang

Agent nantinya dapat mengatur route aplikasi setelah container siap, lalu melaporkan event route ke dashboard. Mekanisme penulisan config, validasi, reload, rollback, dan concurrency belum ditetapkan.

## Boundary Keamanan

- Jangan memberi Caddy akses Docker socket untuk discovery otomatis.
- Jangan mengaktifkan route sebelum upstream tervalidasi.
- Jangan membuat asumsi TLS production dari config lokal.
- Route harus terkait dengan command/project yang sah dari control plane.

`crates/sakala-agent-runtime/src/caddy.rs` tetap skeleton sampai desain tersebut siap.
