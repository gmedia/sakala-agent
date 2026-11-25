# Caddy Integration

Agent menulis satu file `<project-id>.Caddyfile` pada `SAKALA_CADDY_SITES_DIR`. Domain hanya diterima bila berupa satu label di bawah suffix pada `SAKALA_PROJECT_DOMAIN_SUFFIXES` (default `run.sakala.localhost`, `staging.sakala.app`, dan `sakala.app`; lihat [Domain project](CONFIGURATION.md#domain-project)); upstream selalu candidate container terkelola dan port tervalidasi.

```txt
write temporary file
-> atomic rename
-> docker exec caddy validate
-> docker exec caddy reload
-> restore route lama bila reload gagal
```

Contract `sakala-infra` memasang folder route read-only ke `/etc/caddy/sites`, mengimport `*.Caddyfile`, dan membatasi Admin API ke loopback container. Port admin tidak dipublish. Docker socket tidak pernah diberikan ke Caddy.

## Topologi yang Didukung Saat Ini

Adapter MVP hanya mendukung Caddy container dari `sakala-infra`. Caddy dan container aplikasi bergabung pada network `sakala-runtime`, sehingga upstream seperti `sakala-app-<project8>-<deployment-id>:3000` dapat ditemukan melalui Docker DNS.

Nama container sekaligus menjadi hostname upstream, sehingga harus tetap berada di bawah batas 63 oktet satu label DNS. Karena itu nama memakai delapan karakter pertama project id ditambah deployment id penuh; dua UUID penuh diterima Docker sebagai nama container tetapi tidak dapat dijawab oleh DNS internalnya, dan route akan gagal resolve.

Instalasi Caddy host pada `/usr/bin/caddy` tidak otomatis kompatibel dengan route tersebut. Proses host tidak menggunakan DNS internal Docker dan tidak dapat mengandalkan nama container sebagai upstream. Karena itu, jangan hanya mengganti reload `docker exec` dengan `caddy reload`.

Untuk pilot yang memakai implementasi sekarang:

1. jalankan `sakala-caddy` dari `sakala-infra`;
2. mount `SAKALA_CADDY_SITES_DIR` ke `/etc/caddy/sites` secara read-only;
3. hubungkan Caddy dan workload ke `sakala-runtime`;
4. nonaktifkan Caddy host atau pastikan tidak bind pada port edge yang sama;
5. pakai image Caddy yang menyediakan `wget`, seperti `caddy:*-alpine`, karena probe readiness berjalan di dalam container ini. Lihat [Docker Runtime](DOCKER_RUNTIME.md#readiness).
6. untuk idle sleep, aktifkan counter request per host di blok global Caddyfile runtime. Lihat [Route tidur dan counter request](#route-tidur-dan-counter-request).

Caddy host juga dapat dipertahankan sebagai outer edge statis:

```caddyfile
http://*.sakala.app {
	reverse_proxy 127.0.0.1:8080
}
```

Dalam pola ini, `sakala-caddy` tetap router dinamis yang dikelola agent dan tetap terhubung ke `sakala-runtime`. Bind port `8080` milik container Caddy ke loopback host. Caddy mempertahankan header `Host` secara default, sehingga router internal tetap dapat memilih file route project. Contoh HTTP di atas tidak menetapkan strategi TLS production; sertifikat dan hardening outer edge harus diputuskan operator secara terpisah.

## Alamat klien dan header forwarded

Pengunjung tidak pernah terhubung langsung ke router. Caddy host atau Sakala Edge
memutus TLS, lalu meneruskan request ke router node dengan `X-Forwarded-Proto: https`,
`X-Forwarded-Host`, dan `X-Forwarded-For` berisi alamat pengunjung yang ia lihat. Hop
pertama itu tidak boleh punya `trusted_proxies` untuk site project, supaya header
`X-Forwarded-*` buatan pengunjung selalu ditimpa.

Router hanya mempercayai header itu dari alamat yang terdaftar di blok global
Caddyfile runtime (sakala-deployment, `runtime/caddy/Caddyfile`):

```caddyfile
{
	servers {
		trusted_proxies static {$SAKALA_RUNTIME_TRUSTED_PROXIES}
		trusted_proxies_strict
		client_ip_headers X-Forwarded-For
	}
}
```

Isinya alamat persis asal koneksi Caddy host atau Edge seperti yang terlihat dari dalam
container router, bukan `private_ranges` atau satu network penuh. Agent menyambungkan
router ke setiap network project, jadi workload juga peer router. Kalau network
workload dipercaya, satu tenant bisa mengirim `X-Forwarded-For` palsu ke aplikasi tenant
lain, misalnya untuk menghindari rate limit per IP.

Dengan blok itu, `{client_ip}` di route adalah alamat pengunjung untuk request dari
Edge, dibaca dari kanan sehingga bagian kiri yang ditulis pengunjung dilewati, dan
alamat peer sendiri untuk koneksi lain. Route menulis nilai itu sebagai satu-satunya isi
`X-Forwarded-For` dan `X-Real-IP`. Tanpa baris ini router menambahkan alamat Edge di
belakang `X-Forwarded-For`, sehingga aplikasi yang hanya mempercayai peer langsungnya,
seperti Laravel `trustProxies(at: '*')` atau Express `trust proxy 1`, melaporkan Edge
sebagai klien, dan `X-Real-IP` kiriman pengunjung sampai ke aplikasi apa adanya.

Yang diterima aplikasi:

| Asal request ke router | `X-Forwarded-For` dan `X-Real-IP` | `X-Forwarded-Proto` | `X-Forwarded-Host` |
| --- | --- | --- | --- |
| Alamat di `trusted_proxies` (Edge) | alamat pengunjung | dari Edge (`https`) | dari Edge |
| Peer lain, misalnya workload | alamat peer itu | `http` | `Host` request |

`Host` diteruskan apa adanya ke aplikasi. `X-Forwarded-Port` dan `Forwarded` tidak
diatur router; aplikasi sebaiknya tidak mempercayainya. Uji Docker nyata ada di
`crates/sakala-agent-runtime/tests/client_ip_smoke_test.rs`.

## Custom domain

Project dengan custom domain (ADR-024) tetap punya satu file route. Custom domain-nya
menjadi alamat site tambahan setelah domain project, terurut, di route biasa, route
replika, maupun route tidur:

```caddyfile
# Managed by sakala-agent for project <project-id> deployment <deployment-id>.
portfolio.sakala.app:80, contoh.com:80, www.contoh.com:80 {
	reverse_proxy sakala-app-<project8>-<deployment-id>:3000 {
		...
	}
}
```

Dengan begitu aplikasi menerima `Host` asli dan `metrics { per_host }` menghitung
request per custom domain, yang Agent petakan ke project lewat file ini. Alamat
pertama selalu domain project; halaman tidur memakainya untuk slug wake, sehingga
halaman yang dibuka lewat custom domain membangunkan project yang benar. Agent menolak
custom domain yang sudah ada di file route project lain, di bawah kunci yang sama
dengan penulisan file, dan hanya membaca kembali file yang bentuknya persis seperti
yang ia tulis. Project tanpa custom domain mendapat file yang sama byte per byte
seperti sebelum fitur ini. Uji Docker nyata ada di
`crates/sakala-agent-runtime/tests/custom_domains_smoke_test.rs`.

## Route untuk beberapa replika

Deployment dengan lebih dari satu replika tetap mendapat satu file dan satu blok site,
dengan semua replika sebagai upstream:

```caddyfile
# Managed by sakala-agent for project <project-id> deployment <deployment-id>.
portfolio.sakala.app:80 {
	reverse_proxy sakala-app-<project8>-<deployment-id>-r0:3000 sakala-app-<project8>-<deployment-id>-r1:3000 sakala-app-<project8>-<deployment-id>-r2:3000 {
		lb_policy least_conn
		lb_try_duration 5s
		lb_try_interval 250ms
		fail_duration 10s
		header_up X-Forwarded-For {client_ip}
		header_up X-Real-IP {client_ip}
		header_down Set-Cookie "(?i);\s*domain\s*=[^;]*" ""
	}
}
```

Deployment satu replika mendapat `reverse_proxy <container>:<port>` dengan blok yang
hanya berisi dua baris `header_up` dan satu baris `header_down`. Ketiganya ada di setiap
route project. Baris `header_up` memberi aplikasi tepat satu alamat klien; lihat
[Alamat klien dan header forwarded](#alamat-klien-dan-header-forwarded). Baris
`header_down` membuat cookie dari aplikasi menjadi host-only; alasannya ada di
[Cookie host-only](RUNTIME_HARDENING.md#cookie-host-only-di-route-project). Opsi load
balancing di atas adalah konstanta di
`crates/sakala-agent-runtime/src/routing/caddy_file.rs`:

- `least_conn` mengirim request baru ke replika dengan request aktif paling sedikit.
- `lb_try_duration 5s` dan `lb_try_interval 250ms`: request yang gagal terhubung ke satu
  replika dicoba ke replika lain selama 5 detik. Request pada koneksi keep-alive ke
  replika yang mati mendadak baru gagal setelah sekitar 3 detik (terukur 2,8 detik),
  jadi jendelanya dibuat lebih panjang dari itu. Caddy hanya mengulang request `GET`
  atau request yang gagal saat dial, sehingga `POST` yang sedang dikirim ke replika
  yang mati mendadak tetap bisa gagal.
- `fail_duration 10s` adalah health check pasif: replika yang gagal satu request tidak
  diberi request selama 10 detik.

Agent tidak memakai health check aktif Caddy (`health_uri`). Di Caddy 2.11,
`health_status` hanya menerima satu kode atau satu kelas status, dan tanpa itu hanya 2xx
yang dianggap sehat, sedangkan Sakala menganggap semua jawaban di bawah 500 sehat.
Aplikasi yang menjawab 404 di `/` akan dikeluarkan dari route-nya sendiri. Agent
sudah memastikan setiap replika menjawab sebelum route ditulis, dan health check pasif
menangani replika yang mati sesudahnya.

Caddy tidak kembali ke replika yang ditandai gagal walaupun semua replika ditandai
gagal. Kegagalan yang mengenai semua replika sekaligus, misalnya DNS Docker yang
sebentar tidak menjawab, membuat site menjawab 503 paling lama `fail_duration`. Karena
itu nilainya pendek. Container Caddy harus berjalan di network user-defined
(`sakala-runtime`) sejak start: di bridge bawaan Docker, resolver-nya memakai name server
host selama beberapa detik setelah bergabung ke network project, dan request pertama
gagal me-resolve semua replika.

## Route tidur dan counter request

Saat `SleepProject`, Agent tidak menghapus file route project, tetapi menggantinya dengan route tidur untuk domain yang sama. Baris pertamanya memakai penanda kepemilikan sendiri:

```caddyfile
# Managed by sakala-agent for project <project-id> deployment <deployment-id> (sleeping).
portfolio.sakala.app:80 {
	header {
		Content-Type "text/html; charset=utf-8"
		Cache-Control "no-store"
		Retry-After "5"
		X-Robots-Tag "noindex"
	}
	respond <<SAKALA_SLEEPING_PAGE
		<!doctype html>
		...
		SAKALA_SLEEPING_PAGE 503
}
```

Isi halamannya dipilih lewat field `page` pada payload `SleepProject` (lihat [Sakala Agent API](AGENT_API.md#workload-lifecycle-commands)). Keduanya menjawab HTTP 503 dengan `Cache-Control: no-store`, `X-Robots-Tag: noindex`, dan `Content-Security-Policy`, dan keduanya memakai penanda `(sleeping)` yang sama. Keduanya berdiri sendiri: style, script, dan simbol Sakala ditulis inline, tidak ada font, gambar, atau file lain yang dimuat. CSP-nya `default-src 'none'` dan hanya mengizinkan blok `<style>` dan `<script>` halaman itu lewat hash SHA-256-nya, ditambah origin endpoint wake untuk `connect-src`, `form-action`, dan `frame-src` di halaman `waking`. Halaman tidak pernah menampilkan nama project, domain, atau teks lain yang ditulis pengguna.

Bila `SAKALA_PUBLIC_CONSOLE_URL` diisi, kedua halaman menautkan pemilik project ke Console ("Kalau ini proyekmu, ... di Console Sakala"). Tanpa itu kalimatnya tetap ada, tanpa tautan. Di bawah halaman ada simbol Sakala dan teks "Di-host di Sakala".

Halaman `waking`, default-nya, berjudul "Aplikasi ini sedang dinyalakan", menyebut bahwa aplikasi biasanya siap dalam kurang dari satu menit, dan mengirim `Retry-After: 5`. Perkiraan itu sama dengan yang ditampilkan Console untuk project yang tidur. Angkanya berasal dari wake yang tercatat (dari permintaan wake sampai Agent melaporkan aplikasi siap): 3 sampai 11 detik di production dan 5 sampai 35 detik di staging, 19 sampai 23 November 2026. Garis progres kecil bergerak selama menunggu dan diam bila browser meminta `prefers-reduced-motion: reduce`.

Script kecil di halaman itu mengirim `POST <SAKALA_PUBLIC_API_URL>/api/v1/public/projects/<slug>/wake` dengan `mode: 'no-cors'`, tanpa membaca jawabannya, pada pemuatan pertama sebuah kunjungan, lalu memuat ulang halaman setiap 3 detik, paling banyak 40 kali (2 menit). Karena halaman tidak bisa tahu apakah permintaan itu sampai, ia mengirim ulang setiap 15 detik (setiap 5 kali muat ulang) selama menunggu. API hanya menyimpan satu wake per project, jadi permintaan ulang tidak memulai wake kedua, dan satu tab tetap di bawah batas API 6 permintaan per menit per alamat pengunjung dan project. Sebelumnya halaman hanya meminta sekali: satu permintaan yang hilang, misalnya karena jaringan pengunjung berganti atau API sedang sibuk, membuat halaman memuat ulang selama 2 menit tanpa ada yang membangunkan project. Jumlah muat ulang disimpan di `sessionStorage` tab itu, di kunci `sakala-wake-reloads` dan `sakala-wake-last-load`. Bila project belum juga bangun, halaman berhenti dan menampilkan "Aplikasi ini belum bisa dinyalakan" dengan tombol "Coba lagi". Tombol itu mengirim satu permintaan wake lagi dan memulai putaran baru. Pemuatan yang datang lebih dari 60 detik setelah pemuatan sebelumnya dianggap kunjungan baru, jadi membuka lagi project itu nanti di tab yang sama tetap membangunkannya.

Bila `sessionStorage` diblokir browser, jumlah muat ulang tidak bisa dihitung, jadi halaman memuat ulang lebih lambat, setiap 15 detik tanpa batas, dan hanya meminta wake pada navigasi yang bukan reload. Tanpa JavaScript, `<noscript>` juga memuat ulang setiap 15 detik tanpa batas, karena batas tidak bisa ditegakkan tanpa script, dan menampilkan tombol "Nyalakan aplikasi": form biasa yang mengirim `POST` yang sama ke frame tersembunyi, jadi halaman tetap di tempat dan muat ulang berikutnya membuka aplikasi begitu siap.

`<slug>` adalah label domain sebelum suffix project, yang di `sakala-api` sama dengan slug project. Karena browser pengunjung yang memanggil endpoint itu, URL-nya harus bisa dijangkau dari internet. `SAKALA_PUBLIC_API_URL` defaultnya sama dengan `SAKALA_API_URL`; instalasi yang menghubungi API lewat alamat internal, misalnya `http://127.0.0.1:8100` pada self-host, wajib mengisinya dengan URL publik API. Tanpa itu halaman tetap tampil tetapi tidak bisa membangunkan project. Lihat [Configuration](CONFIGURATION.md).

Halaman `unavailable` berjudul "Aplikasi ini sedang tidak tersedia", tanpa script, tanpa permintaan wake, tanpa muat ulang, dan tanpa `Retry-After`. API memakainya untuk project yang tidak boleh bangun saat dikunjungi, misalnya yang di-suspend. Alasannya tidak disebut ke pengunjung; pemilik project diarahkan ke Console. Halaman ini tidak butuh URL wake, jadi tetap bisa dipasang walaupun URL API publik tidak bisa dipakai untuk halaman `waking`.

Domain, URL wake, dan URL Console dibatasi ke karakter yang tidak punya arti di Caddyfile, heredoc, placeholder Caddy (`{...}`), HTML, maupun string JavaScript. Teks dan waktu halaman adalah konstanta di `crates/sakala-agent-runtime/src/routing/sleeping_page.rs`, bukan input. Bila halaman tidak bisa dibangun, misalnya URL API publik tidak valid untuk halaman `waking`, atau Caddy menolak route tidur, Agent menghapus route seperti sebelum fitur ini ada dan mencatat alasannya di log operator. Sleep tetap sukses.

Route tidur ikut aturan kepemilikan yang sama dengan route biasa:

- `WakeProject` menulis route biasa di atas route tidur, halaman mana pun yang terpasang, lewat rename atomik yang sama, lalu validate dan reload. Bila Caddy menolaknya, route tidur dipulihkan.
- `StopProject` dan redeploy menghapus atau menimpa route tidur. Command untuk deployment lain tidak menyentuhnya.
- Discovery route basi menganggap route tidur masih dipakai selama container deployment-nya masih ada dalam status apa pun, biasanya `exited`. Route tidur yang containernya sudah hilang dianggap basi. Route biasa tetap butuh container yang berjalan.

Untuk mengukur kapan project terakhir dikunjungi, Caddyfile runtime perlu counter per host:

```caddyfile
{
	admin localhost:2019
	auto_https off
	metrics {
		per_host
	}
}
```

Setiap heartbeat, Agent menjalankan `docker exec <SAKALA_CADDY_CONTAINER> sh -c 'wget -qO- http://127.0.0.1:2019/metrics | grep ^caddy_http_requests_total'` (bentuk persisnya ada di `routing/docker_exec.rs`). Alamatnya `127.0.0.1`, bukan `localhost`, karena BusyBox `wget` mencoba `::1` lebih dulu, sedangkan admin Caddy hanya listen di IPv4 loopback. Hanya host yang punya blok sendiri yang mendapat label `host`; host lain masuk `_other`, termasuk header `Host` yang membawa port. Agent tidak pernah membuka atau meneruskan metrics ini. Detail pembacaannya ada di [Sakala Agent API](AGENT_API.md#traffic-workload).

## Arah Host Caddy

Host Caddy dapat ditambahkan sebagai adapter berbeda tanpa mengubah `RuntimeExecutor`, tetapi membutuhkan lebih dari transport reload. Container perlu mem-publish port unik ke loopback host, route harus mengarah ke loopback tersebut, dan agent perlu mengelola alokasi serta cleanup port secara persisten.

```txt
application container :3000
-> publish 127.0.0.1:31001
-> host Caddy reverse_proxy 127.0.0.1:31001
```

Reload host harus memakai permission terbatas. Agent tidak boleh memperoleh akses `sudo` arbitrer atau mengekspos Docker socket kepada Caddy.

TLS production, host-Caddy adapter, concurrent per-project lease, dan distributed route storage belum termasuk MVP.
