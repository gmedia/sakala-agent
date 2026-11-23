# Development Workflow

## Requirements

- Rust 1.89 atau lebih baru dengan `rustfmt` dan `clippy`. Versi minimum ini (MSRV) adalah `rust-version` di `Cargo.toml`; lihat [Versi Rust minimum](#versi-rust-minimum).
- Cargo.
- `cargo-audit` bila menjalankan audit lokal.
- Git dan Docker Buildx hanya untuk integration test runtime opt-in. Railpack berjalan di image `docker/railpack-sandbox`, yang dibangun oleh smoke test.

Toolchain komponen dideklarasikan di `rust-toolchain.toml`.

## Versi Rust minimum

`rust-version` di `[workspace.package]` `Cargo.toml` adalah versi Rust paling lama
yang masih bisa membangun workspace dengan `Cargo.lock` yang di-commit. Nilai itu
satu-satunya tempat MSRV ditulis. Job CI `msrv` membacanya lalu menjalankan:

```bash
cargo +1.89 check --workspace --all-targets --all-features --locked
```

`rust-toolchain.toml` tetap memilih `stable` untuk build, test, clippy, dan rilis;
`+<versi>` mengalahkan file itu, jadi hanya job ini yang memakai versi minimum.

Kalau update dependency membuat job `msrv` gagal dengan pesan
`<crate>@<versi> requires rustc <versi>`, naikkan `rust-version` ke versi yang
diminta di PR yang sama dan catat di `CHANGELOG.md`, kecuali ada alasan untuk
menahan dependency itu. Untuk melihat kebutuhan tertinggi di dependency tree:

```bash
cargo metadata --format-version 1 --all-features \
  | jq -r '.packages[] | select(.rust_version != null) | "\(.rust_version) \(.name)@\(.version)"' \
  | sort -V -r | head
```

## Menjalankan Agent Lokal

```bash
cargo run -p sakala-agent
```

Output JSON akan menunjukkan startup, heartbeat, dan polling tick. Tekan `Ctrl+C` untuk menghentikan agent.

## Quality Commands

```bash
make fmt
make lint
make test
make build
make audit
```

CI menjalankan format check, clippy, test, dan build dengan `stable`, ditambah `cargo check` dengan versi Rust minimum. Workflow security menjalankan `cargo audit`.

## Release

Release dipicu oleh tag semantic version `v*`. Workflow release memverifikasi
`make check`, membangun binary Linux `x86_64` `sakala-agent` dan `sakala-edge`
dengan toolchain `stable` yang dipilih `rust-toolchain.toml`, sama seperti CI, lalu menerbitkan keduanya dan `SHA256SUMS` (satu baris per
binary) sebagai GitHub Release assets. Isi
`CHANGELOG.md` harus diperbarui sebelum tag dibuat.

## Menambah Dependency

Gunakan `cargo add`:

```bash
cargo add <dependency>@<version> -p sakala-agent-core
```

Jika dependency digunakan oleh beberapa crate, pusatkan version requirement di `[workspace.dependencies]` setelah Cargo menambahkannya.

## Menambah Perilaku Runtime

1. Perbarui protocol hanya bila kontrak agent API berubah.
2. Tambahkan atau perluas trait/runtime implementation.
3. Pastikan core tetap mengurus lifecycle/reporting, bukan detail host.
4. Tambahkan test untuk safety dan failure path.
5. Perbarui dokumentasi security/runtime/API.

Runtime tests di crate `sakala-agent-runtime` memakai fake `ProcessRunner`; jangan membuat unit test bergantung pada daemon Docker contributor.
