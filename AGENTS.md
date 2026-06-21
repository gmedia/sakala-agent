# AGENTS.md - Sakala Agent

Dokumen ini berlaku bagi AI agents, Codex CLI, automation tools, dan contributor yang bekerja pada `sakala-agent`.

## Identity

- Project: **Sakala Agent**
- Product: **Sakala**
- Project stewardship: **Sakala Maintainers**
- Founding sponsor: **PT Media Sarana Data / GMEDIA**
- License: **Apache License 2.0**
- Documentation language: **Bahasa Indonesia**
- Rust identifiers and API field names: **English**

Do not frame Sakala as a closed GMEDIA-owned product. Use this framing instead: Sakala is an open-source project maintained by Sakala maintainers and supported by GMEDIA as founding sponsor.

## Architecture Rules

- `sakala-api` adalah control plane; `sakala-console` adalah frontend; agent adalah runtime executor.
- Agent melakukan polling command dan report outbound ke `sakala-api`.
- Jangan membuat direct API-to-agent command server.
- Jangan memberi `sakala-api` atau `sakala-console` akses Docker socket.
- Jangan menambahkan Docker client, Railpack execution, atau Caddy mutation sebelum task secara eksplisit meminta dan threat model diperbarui.
- Jangan memperkenalkan Kubernetes, containerd, Firecracker, atau microVM pada fase foundation.
- Pertahankan tiga workspace crate yang sudah ada kecuali keputusan arsitektur menyatakan lain.

## Dependency Rules

- Tambahkan Rust dependency menggunakan `cargo add`, bukan menulis package baru secara manual di manifest.
- Setelah dependency ditambahkan, shared version boleh dipusatkan di `[workspace.dependencies]`.
- Jangan menambah dependency bila standard library atau dependency yang sudah ada mencukupi.
- Jangan menambahkan Docker client crate pada foundation ini.

## Code Rules

- Jagalah crate boundary: protocol tidak bergantung pada core/runtime; runtime bergantung pada protocol; core mengorkestrasi runtime dan HTTP; app hanya wiring process.
- Jangan mencatat token atau secret.
- Semua log runtime yang dilaporkan harus melalui redaction boundary.
- Gunakan async worker yang dapat dihentikan dengan graceful shutdown.
- Default mode harus tetap `local` dan bebas network call ke control plane.

## Verification

Jalankan pemeriksaan relevan sebelum finalisasi:

```bash
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo build --workspace --all-features
```

## Commit Convention

Gunakan Conventional Commits 1.0.0:

```txt
<type>[optional scope]: <description>
```

Scope umum:

```txt
agent protocol core runtime api logs docs ci security
```

Contoh:

```txt
chore(agent): initialize Rust workspace foundation
feat(protocol): add deployment command payload
fix(logs): redact database url output
docs(runtime): define Docker executor boundary
```
