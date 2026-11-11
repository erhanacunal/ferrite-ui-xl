# ferrite-ui-xl — Plan (Adım 1 ve 2)

ferrite-ui'nin güçlü cihazlar (tablet/telefon sınıfı; ilk hedef Exynos 4412 eTab4) için
ayrılmış sürümü. Amaç: ferrite-ui'nin GD32 kısıtlarıyla yapılamayan mimari değişiklikler
(render'ı ayrı çekirdeğe taşımak, display list, native layout).

Varsayım: XL bare-metal + Embassy üzerinde kalır.

## Sınırlar

- **Tek kaynak:** `.fl` derleyicisi (`tools/`), `.fl` kütüphanesi (`lib/`) ve Designer
  ferrite-ui'de kalır. XL bunları kopyalamaz. Şimdilik yan yana checkout
  (`../ferrite-ui`, `FERRITE_UI` ile değiştirilebilir) kullanılır. Gerekirse `tools` + `lib`
  ayrı bir repoya çıkarılıp iki repoya da submodule olarak eklenir.
- **Uyumluluk:** `.fl` kaynak dili ve Designer projeleri uyumlu kalır. Bytecode farkları
  cihaz yetenekleriyle (`caps.*`, `superops`/`multi_program` örüntüsü) açılır.
- **Okuma modeli:** VM render'ı asla beklemez, render da VM'i beklemez. Render'dan VM'e
  iletişim sadece salt okunur snapshot ve olay kuyruğu ile olur.

## Adım 1: Taşıma (referans noktası)

- [x] **1a Kopya:** `ferrite-core`, `bsp/exynos4412`, `bsp/sim`, `examples/exynos4412`
  ferrite-ui `9829d15`'ten (`git archive`). HAL submodule'ü `vendor/exynos_4412_rs`,
  ferrite-ui ile aynı commit'te (`11087c3`). Örneklerin `include_dirs`'i
  `../ferrite-ui/lib`'e, `sim.bat` `%FERRITE_UI%\tools`'a bakıyor.
  Doğrulandı: core testleri (exynos feature seti) 195 geçti, sim (`tablet`) ve exynos release
  derleniyor, `bench`/`tablet` image'ları ferrite-ui araçlarıyla derleniyor.
  `tablet/assets/` git-ignored ve üretiliyor (`make_assets.py`).
- [x] **1b Feature'ları sabitleme:** `wide_obj`, `pool_large`, `wide_code`, `fast_vm`,
  `superops`, `multi_program`, `render_cache`, `layers`, `perf`, `audio` artık feature değil,
  kapalı dalları silindi (u16 `ObjPtr`/`Off`, sparse var tablosu, katmansız stub'lar).
  `epaper`/`sdcard`/`sdboot` modülleriyle birlikte kaldırıldı. `AA_UNDERLAY`/`Underlay`
  kaldırıldı. Sim her zaman 1280×800 ve readback'li. `CLAUDE.md` başına XL farkları eklendi.
  - Ertelendi (2a'ya): `HAS_ALPHA=false` yolu (mock backend testleri ona dayanıyor),
    `send_command`/`send_data` + `OP_FPGA_*`, GD32 statik heap'i (`heap.rs`, ölü kod),
    CLAUDE.md'nin tamamen yeniden yazılması.
- [x] **1c Referans ölçümleri (sim):** sim'de deterministik mod (`--dump DIR --dump-at MS,..
  [--step-ms N]`: sanal saat, sabit RTC, sadece scripted dokunma, BMP dump).
  `scripts/golden.py` `golden/scenarios.json`'daki 13 senaryoyu (9 örnek) çalıştırıp frame
  hash'lerini `golden/hashes.json` ile karşılaştırıyor (`--update` ile kaydeder; farklarda
  `target/golden/<senaryo>/*_diff.png`).
- [ ] **1c Referans ölçümleri (cihaz):** `scripts/perf_baseline.py` (kartta XL firmware +
  WiFi gerekli) örnek başına `ferrite_cli.py perf` çıktısını `perf/baseline/`'a yazar.
- [x] **1d Native flex** (aşağıya bakın). Seviye 1 ve 2 tamam.

Kabul kriteri: testler geçiyor, sim örnekleri çalışıyor, tablet cihazda açılıyor,
perf değerleri ferrite-ui ile aynı (±%2).

### 1d Native flex

Bugün `lib/flex.fl` içindeki `flexLayout(box)` her `get`/`set`/`target` için birer opcode
çalıştırıyor. Çocuk başına yaklaşık 20 VM komutu, iç içe container'lar için de recursive
.fl çağrıları gerekiyor. Ayrıca çağrılması unutulursa layout bayat kalıyor.

Layout **VM çekirdeğinde** kalır, render tarafına taşınmaz. Bu sayede pozisyon ve
boyutlar VM'e ait prop'lar (A sınıfı) olarak kalır ve senkron okumalar beklemeden doğru
sonuç verir. Layout'un maliyeti raster'a göre küçük, ayrı bir çekirdeğe ihtiyacı yok.

- [x] **Seviye 1, açık çağrı (aynı semantik):** `flexLayout(box)` cihazda
  `caps.native_flex` varsa `FLEX_LAYOUT` (0xDB) opcode'u olarak derlenir
  (ferrite-ui `tools`, cihaz profili `exynos4412-xl`; firmware `CAP_NATIVE_FLEX`,
  `ferrite-core/src/vm_flex.rs`). Okumalar, yazmalar (`w_set`), target ve aritmetik
  .fl ile bire bir aynı. Test: `runtime::flex_tests`, `testdata/flex/flex_test.fl`
  programını iki şekilde derleyip (`gen.py`) tüm widget'ları karşılaştırıyor.
  Bu programda VM komut sayısı 274.850 → 160.631.
- [x] **Seviye 2, otomatik layout (opt-in: `FLEX_AUTO` = 128, flex mode bit 7):** bit'i
  olmayan container'lar bugünkü gibi davranır (flex çocuklarını elle konumlandıran kod
  bozulmasın diye). lib/flex.fl bit'i yok sayar, eski cihazlarda `flexLayout()` gerekir.
  Bekleyen container'lar `ObjPool::flex_pending`'te (widget flag'lerinde boş bit yok).
  Test: `flex_auto.fl` (FLEX_AUTO, çağrı yok) ile `flex_manual.fl` (her okumadan önce
  `flexLayout`) aynı gözlemleri ve aynı ağacı vermeli; okumasız yazmadan sonra render
  geçişi layout'u güncelliyor.
  Tasarım: layout'u etkileyen prop'lar değiştiğinde container
  "layout dirty" işaretlenir: `size`, `visible`, `flex*`, `border`/`padding`, çocuk
  eklenmesi/çıkarılması/taşınması, büyümeyen çocuğun ana eksen boyutu.
  - Layout VM burst'ünün sonunda (render/commit öncesi) native olarak çalışır.
  - Kirli bir container'ın çocuğundan `loc`/`size` okunursa, okumadan önce o alt ağacın
    layout'u hesaplanır (tarayıcılardaki forced layout gibi).
  - `flexLayout()` çağrısı no-op ya da "hemen hesapla" anlamına gelir. Mevcut kod
    değişmeden çalışır.

Seviye 1 ferrite-ui'nin exynos4412 hedefine de aynı cap ile eklenebilir. Seviye 2 XL'e özel.

## Adım 2: Display list (tek çekirdek, davranış aynı)

Renderer piksel yazmak yerine widget başına komut listesi üretir, liste ayrıca rasterize
edilir. Kesim noktası `LcdImpl` API'si (`render.rs`/`vm.rs` buradan ~60 metot kullanıyor;
`lcd.backend()` çağrıları neredeyse yalnızca testlerde).

```rust
pub enum DrawCmd {
    PushClip(Rect), PopClip, Translate(i32, i32),
    FillRect{..}, BlendRect{..}, FillGradientRect{..},
    FillRRect{..}, StrokeRRect{..}, FillGradientRRect{..},
    Line{..}, LineThick{..}, Polyline{pts: Span, ..}, Wedge{..}, Polygon{pts: Span, ..},
    Circle{..}, FillCircle{..}, Ellipse{..}, Oval{..}, Arc{..},
    Glyphs{font, text: Span, color, x, y, ..},   // metin kopyalanır
    Image{res_id, src: Rect, dst: Rect, alpha},  // kaynak id ile
    Pixels{w, h, data: Span},
    LayerTarget(..), LayerClear(..), LayerCompose(..),
}
pub struct DisplayList { cmds: Vec<DrawCmd>, arena: Vec<u8> }
```

- Liste widget'ın kendi koordinatlarında tutulur, replay sırasında `Translate` uygulanır.
- Liste pool'a referans tutmaz. 3. adımda başka çekirdeğe gönderilebilmesi buna bağlı.
- Renk tipi `Color` newtype olur (şimdilik içi RGB565).

| | İş | Doğrulama |
|---|---|---|
| 2a | `Canvas` trait'i; `LcdImpl: Canvas`; `draw_widget_body`, `font`, `image` ve VM çizim opcode'ları `&impl Canvas` alır | Davranış aynı, tüm testler geçer |
| 2b | `Recorder: Canvas` + `replay`; her widget kaydedilir ve hemen replay edilir (cache yok) | Sim'de hemen çizim ile replay piksel bazında aynı (testler + golden) |
| 2c | Widget başına cache, anahtar `(id, rev)`. `rev` her `W_SET`/`update_ext`'te artar. Bellek tavanı + LRU | perf: hit oranı, kayıt/replay maliyeti |
| 2d | `on_paint` Recorder üzerinden; exynos ARGB capture listeden üretilen bir optimizasyon olur | `paint` örneği, `captured_on_paint_*` |
| 2e | `GET /api/dl?id=` dump, sim'de liste görüntüleyici | elle kontrol |

Çalışma anında açma/kapama: `dl::ENABLED` + `POST /api/dl?on=0|1`. `ExynosRenderer::draw`,
`compose` ve sprite cache'inin sözleşmesi değişmez, sadece imzaları uyarlanır.

### Read-only okuma modeline hazırlık

`W_GET` tek bir `PropRead` arayüzünden geçer:

| Sınıf | Örnek | 3. adımda kaynak |
|---|---|---|
| A: VM'e ait | `PROP_*`'ların neredeyse hepsi, flex sonuçları dahil | VM store, beklemesiz |
| B: VM'de türetilen | mutlak rect, görünürlük zinciri, metin ölçümü | VM çekirdeğinde hesaplanır |
| C: Render'a ait | render tarafına taşınacak animasyon değerleri, ivmeli scroll, layer/present durumu | `RenderSnapshot` (seqlock), salt okunur, en fazla 1 frame eski |

`RenderSnapshot` tek çekirdekte bile render pass sonunda doldurulur. C okumaları bir bayrakla
buradan yapılır, böylece "1 frame eski" semantiği sim'de önceden denenebilir.
İlk iş: property animation ve `scroll_y` sahipliğini netleştirip C listesini kesinleştirmek.

### Riskler

- 2a büyük ama mekanik (~150 çağrı noktası). `font`/`image` akışları en zor kısım.
  Büyük görseller `Pixels` değil, `Image{res_id}` ile kaydedilmeli.
- Widget başına liste için bellek tavanı ve LRU baştan olmalı.
- AA kenarlarında piksel eşitliği aynı clip sırasına bağlı. Golden testler 2b'nin kabul kriteri.
