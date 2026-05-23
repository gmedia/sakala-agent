# Docker Runtime Direction

Docker runtime belum diimplementasikan. File `crates/sakala-agent-runtime/src/docker.rs` sengaja merupakan skeleton tanpa Docker client dependency.

## Arah Mendatang

Executor Docker nantinya dapat menangani:

- create/start/stop/restart container aplikasi;
- koneksi ke network `sakala-runtime` dari `sakala-infra`;
- resource limits dasar;
- health check dan log collection;
- idempotensi deployment command.

## Safety Requirements Sebelum Implementasi

- Tetapkan payload command tervalidasi dan ownership project/node.
- Hindari raw shell execution berdasarkan input user.
- Definisikan allowlist mount/network/port.
- Jangan pernah memasang Docker socket ke dashboard, Caddy, atau aplikasi user.
- Tambahkan tests dan dokumentasi threat boundary.

Foundation ini tidak boleh dipahami sebagai runtime yang aman untuk menjalankan workload user.
