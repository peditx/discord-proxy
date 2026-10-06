<p align="center">
  <img src="src-tauri/icons/icon.png" width="96" alt="Discord Proxy">
</p>

<h1 align="center">Discord Proxy</h1>

<p align="center">
  <b>Discord'un tüm bağlantılarını seçtiğin proxy üzerinden geçir.</b><br>
  Tauri 2 ile hazırlanmış küçük bir Windows uygulaması — HTTP, SOCKS4 ve SOCKS5; kimlik doğrulamalı proxy'ler dahil.
</p>

<p align="center">
  <a href="README.md">English</a> · <a href="readme-fa.md">فارسی</a> · <a href="readme-ru.md">Русский</a>
</p>

![Discord Proxy](docs/screenshot.png)

## Özellikler

- HTTP / HTTPS, SOCKS5 (kullanıcı adı &amp; şifre, IPv4 ve IPv6) ve SOCKS4 / SOCKS4a upstream proxy'ler
- Discord'un tüm bağlantılarının geçtiği yerel bir relay
- Yalnızca senin açabileceğin isteğe bağlı genel Windows proxy'si
- Proxy uygulanmış şekilde Discord'u uygulamanın içinden başlat / durdur
- Canlı durum: relay durumu, aktif proxy, bağlantı sayısı
- Tepsiye küçültme; tepsi menüsünde Connect / Disconnect / Quit
- Kendini güncelleme: yeni sürümü kontrol et ve uygulamadan kur

## Nasıl çalışır

```
Discord ──► 127.0.0.1:17999 (yerel relay) ──► proxy'n ──► discord.com
```

1. Bir proxy ekleyip etkinleştirirsin.
2. **Yerel relay'yi** başlatırsın — `127.0.0.1` üzerinde dinleyen bir HTTP proxy'si.
3. Discord'u uygulamadan başlatırsın (`--proxy-server` bayrağıyla), ya da **Windows system
   proxy** anahtarını açarsın; böylece kendin açtığın Discord da aynı relay'den geçer.

Relay vardır çünkü Chromium proxy kimlik bilgilerini kendi başına gönderemez ve kimlik
doğrulamalı SOCKS5'i desteklemez. Relay ikisini de halleder ve her şeyi tek bir kapıdan
geçirir.

### Proxy türleri

| Tür | Destek |
| --- | --- |
| HTTP / HTTPS | `CONNECT` tünellemesi, Basic kimlik doğrulama ile |
| SOCKS5 | kullanıcı adı / şifre, IPv4 ve IPv6 |
| SOCKS4 / SOCKS4a | ✔ |

## Windows sistem proxy'si

**Windows system proxy** anahtarı, makinedeki her uygulamayı yerel relay'ye yönlendirir:

- **asla kendi kendine açılmaz** — yalnızca senin anahtara, ya da güncelleme penceresindeki
  *Start with system proxy* düğmesine tıklaman açabilir;
- **Windows bir PAC betiği kullanıyorken reddedilir** (`AutoConfigURL`) — zaten bu durumda
  Windows basit bir proxy sunucusunu yok sayar, bu yüzden uygulama bunun yerine hata verir;
- **relay durduğunda veya uygulama kapandığında otomatik olarak geri alınır**, böylece
  sistem ölü bir proxy'de kalmaz.

### Update.exe ve güncelleme oturumu

`Update.exe` bir .NET programıdır, Chromium değil — yalnızca Windows'un kendi proxy
ayarlarını okur ve relay'yi tamamen atlatır. Discord'u başlattığında uygulama, sistem
proxy'sinin **yalnızca güncelleme için** açılmasını teklif eder:

- pencerede *OK* (hiçbir şey yapma) ve *Start with system proxy* düğmeleri vardır;
- başladıktan sonra bu oturum, **güncelleyici 20 saniye sessiz kaldıktan sonra** sistem
  proxy'sini yeniden kapatır;
- sistem proxy'sini zaten kendin açtıysan bu teklif hiç görünmez ve hiçbir şeye dokunulmaz.

## Uygulamanın kendini güncellemesi

Üst çubuktaki **Check for update** düğmesi GitHub'dan son sürümü sorar:

| Etiket | Anlamı |
| --- | --- |
| Check for update | henüz kontrol edilmedi veya kontrol başarısız oldu |
| Latest version | bu yapı en yeni sürüm |
| Update to vX.Y.Z | daha yeni bir sürüm hazır — tıklamak yükleyiciyi indirir ve uygulamayı yeniden başlatır |

Açılışta uygulama arka planda sessizce de kontrol eder; oradaki bir başarısızlık hiçbir
şeyi değiştirmez.

## Kurulum

`Discord.Proxy_<version>_x64-setup.exe` dosyasını
[Releases](https://github.com/peditx/discord-proxy/releases) sayfasından indirip çalıştır.
Yükleyici NSIS'tir; uygulama içinden kurulan güncellemeler aynı dosyayı kullanır.

## Ayarlar

Ayarlar ve kayıtlı proxy'ler şurada saklanır:

```
%APPDATA%\com.peditx.discordproxy\store.json
```

## Derleme

Uygulama **yalnızca GitHub Actions** üzerinde derlenir — bkz.
[.github/workflows/build.yml](.github/workflows/build.yml). `main`'e her push NSIS
yükleyicisini Artifact olarak üretir; bir etiket (`v1.2.3`) eklemek aynı dosyayı Release'e
de ekler.

Yerel derleme (Windows, isteğe bağlı):

```bash
cargo tauri build
```

## Geliştirme

```bash
cargo tauri dev
```

Frontend derleme adımı yoktur — [src/](src/) içindeki ham HTML/CSS/JS olduğu gibi yüklenir
ve Rust arka uygulamasıyla `window.__TAURI__.core.invoke` üzerinden konuşur.

### Proje yapısı

```
src/                     arayüz (derleme adımı yok)
src-tauri/src/
  lib.rs                 durum + Tauri komutları
  relay.rs               yerel relay (düz HTTP ve CONNECT)
  dial.rs                upstream bağlantısı: HTTP / SOCKS5 / SOCKS4
  sys.rs                 Windows kaydı, Discord'u bulma/başlatma
  store.rs               modeller + JSON kalıcılığı
.github/workflows/
  build.yml              Windows derlemesi ve dağıtımı
docs/
  screenshot.png         yukarıdaki ekran görüntüsü
```

## Krediler

Tasarım: **PeDitX** · [peditx.ir](https://peditx.ir) · 2026
