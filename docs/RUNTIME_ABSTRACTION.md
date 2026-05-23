# Runtime Abstraction

Crate `sakala-agent-runtime` memisahkan orchestrator agent dari operasi host.

## Trait

`RuntimeExecutor` menerima `AgentCommand` dan mengembalikan `ExecutionOutcome` yang terdiri dari events serta logs untuk dilaporkan core.

Boundary ini memungkinkan:

- lifecycle worker diuji tanpa Docker;
- runtime implementation diganti tanpa mengubah protocol;
- validasi keamanan dilakukan sebelum executor privileged aktif.

## Noop Runtime

`NoopRuntimeExecutor` adalah implementasi aktif pada foundation:

- menerima semua command protocol;
- menulis tracing metadata command;
- menghasilkan satu event sukses dan satu system log;
- tidak melakukan perubahan host atau network.

## Future Implementation

Docker, Railpack, Caddy, health checks, sleep/wake, dan log streaming harus diterapkan di crate runtime melalui trait ini. Jangan meletakkan command shell atau Docker access di dashboard client/core worker.
