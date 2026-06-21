# Development Workflow

## Requirements

- Rust stable toolchain dengan `rustfmt` dan `clippy`.
- Cargo.
- `cargo-audit` bila menjalankan audit lokal.

Toolchain komponen dideklarasikan di `rust-toolchain.toml`.

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

CI menjalankan format check, clippy, test, dan build. Workflow security menjalankan `cargo audit`.

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
