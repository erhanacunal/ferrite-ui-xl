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
- [x] **1c Referans ölçümleri (cihaz, 2026-11-11):** `scripts/perf_baseline.py --fw X.bin` örneği FLASH.BIN olarak kaydedip firmware'i uart_boot `/boot` ile RAM'den çalıştırır (kayıtlı firmware değişmez; uart_boot'a HTTP bağlantısı autoboot'u iptal eder, bekleme ona dayanmaz). `perf/ferrite-ui/` (ferrite-ui 895a98c firmware + kendi örnekleri), `perf/baseline/` (XL), `perf/dl/` (XL + `/api/dl?on=1`); özet `python scripts/perf_compare.py`.
  - **VM farkı çözüldü (bisect):** %3.5'i derleyici: ferrite-ui'de rustup dizin override'ı nightly 1.98 (2026-06-03), XL stable 1.99.1 ile derleniyor. Kalan %2.8 `1fb7233` (FLEX_AUTO): `flex::flush`/`flex_note` `Vm::step`'e satır içi açılıyordu; artık yalnız "bekleyen var mı" kontrolü inline. bench mod 0 (Minstr/s): ferrite-ui 10.77; XL nightly 10.74 → düzeltmeyle 10.81; XL stable 10.19 → 10.47. Toolchain `rust-toolchain.toml` ile nightly-2026-06-04'e sabitlendi (ferrite-ui ile aynı derleyici); firmware ölçülen 10.81 build'iyle bayt bayt aynı, golden 13/13 iki modda. `perf/baseline` ve `perf/dl` sabitlenmiş derleyiciyle yeniden ölçüldü: bench 10.8 = 10.8 Minstr/s, diğer örnekler gürültü içinde (`perf_compare.py`).
  - Diğer örnekler ±%3 içinde; çoğu render GPU compositor'dan geçtiği için `draw_widget` neredeyse hiç çağrılmıyor. DL yalnız tablette ölçülebildi: widget çizimi 7.4 → 9.6 ms/s (+%30), en uzun geçiş 8.9 → 11.6 ms.
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

- [x] **2a** (2026-11-11): `ferrite-core/src/canvas.rs` (`Canvas` + `Pen`, GAT `Pen<'a>`; `LcdImpl` doğrudan yönlendirir). `render.rs` çizim yardımcıları, `draw_widget_body`, `draw_field`, `draw_label_text`, `draw_bg_image`, `draw_widget_image`, `draw_polygon_shape`, `font`, `image` (`PixelCursor` dahil) ve `keyboard` `&impl Canvas` alır; `B::HAS_ALPHA` → `has_alpha()`. Kare/katman/present/portrait `LcdImpl`'de kaldı. 198 test, golden 13/13 piksel aynı.
  - VM çizim opcode'ları (`fillRect`, `drawText`…) hâlâ `ctx.lcd`: 2d'de `on_paint` ile birlikte Recorder'a bağlanacak. Kaydırma çubuğu ve restore yolları pass seviyesinde, widget listesine girmez.
- [x] **2b** (2026-11-11): `ferrite-core/src/dl.rs`: `List` (çağrılar sırayla; piksel/kapsama/nokta dilimleri ortak arenalarda, ardışık `write_pixel`'ler birleşir) + `Recorder: Canvas` (durum sorguları hedeften, scissor'u kendisi izler; kalem `PenBegin/To/Finish/End` olarak, grafikte dolgu ile iç içe) + `List::replay`. `render::draw_body`: `dl::ENABLED` açıkken widget havuzdaki kaydediciye çizilir ve hemen replay edilir. Açma: sim `FERRITE_SIM_DL=1`, exynos `POST /api/dl?on=1`, `golden.py --dl`. Test `shift_tests::replayed_display_lists_match_direct_drawing` (her widget türü, tam ve parçalı clip; mutasyonlarla doğrulandı). Golden 13/13 her iki modda aynı hash; sim'de maliyet ölçülemeyecek kadar küçük. Liste şimdilik mutlak koordinatlarda; 2c'de taşınma için ofset.
- [x] **2c** (2026-11-11): `dl::Cache` (BTreeMap, LRU, 32 MB / liste başına 4 MB / 4096 liste). Anahtar `(id, rev)` değil, çizimin okuduğu her şeyin FNV hash'i (`render::dl_key`): widget (bookkeeping bayrakları hariç) + ext (son çizilen rect'ler hariç), kutu, canvas, miras alınan arka planlar (`effective_bg`, `resolved_text_bg`), metin içeriği, font/görüntü kimliği, grafik dizisi, input imleci. Aynı görünen widget'lar listeyi paylaşır; kaçan bir girdi bayat liste değil, yalnız ıska. İlk görülen görünüm doğrudan çizilir (`seen`), ikincisinde tüm ekrana karşı kaydedilir, sonra her clip rect'ine `List::replay_clipped` ile oynatılır (scissor'lar kesilir; scissor'a bakmayan piksel akışları ve gradient dolguları replay'de kesilir). `dl::VERIFY` (sim `FERRITE_SIM_DL_VERIFY=1`, `golden.py --dl` her zaman): her isabette yeniden kaydedip karşılaştırır, farkta panic. Sayaçlar: sim dump sonunda stderr, exynos `/api/status` `dl`. Testler `cached_display_lists_replay_into_any_clip`, `clipped_replay_cuts_pixel_streams` (mutasyonlarla doğrulandı; anahtardan metin çıkarılınca VERIFY tablet saatinde yakaladı). Golden 13/13 iki modda. Sim isabetleri: scroll 26.9 K / 10 K doğrudan, layers 5.7 K / 300. Test backend'i artık piksel akışlarını yazıyor: ekranın solundan başlayan gradient kutu hatasını gösterdi (`ad1e6b3`, ferrite-ui'de de var).
  - Tek çekirdekte kazanç sınırlı: isabet yalnız çizim öncesi işi (metin yerleşimi, görüntü çözme, geometri) atlar, pikseller yine çizilir. Asıl getiri 3. adım (listeleri render çekirdeği oynatır).
  - Cihaz (`perf/dl2c`): `draw_widget` kullanan tek örnek tablet ana ekranı; draws 8.2 (kapalı) / 8.6 (2b) / 8.8 ms/s (2c). Anahtar hash'i + kırpılmış replay, atlanan işten pahalı. Diğer örnekler GPU compositor'dan geçiyor (draws 0), farklar ölçüm gürültüsü.
- [x] **2d** (2026-11-11): VM çizim opcode'ları (`fillRect`, `rect`, `line`, `circle`, `fillCircle`, `drawImage`, `drawText`, `roundedRect`, `fillRoundedRect`, `arc`) tek bir satır dışı `Vm::draw_op`'ta, `canvas!` ile hedef seçilir: `ObjPool::render.painting` açıkken `paint_rec` (Recorder), yoksa ekran. `Canvas::draw_arc` + `Op::ThinArc` eklendi. `dl::ENABLED` açıkken `Vm::run_paint` dispatcher'ı widget başına bir kez çalıştırır (`record_paint`), listeyi hemen olduğu yere replay eder (piksel aynı: ekran ya da exynos capture sprite'ı, çünkü Recorder durumunu hedeften alır) ve widget'ın son paint listesi olarak `render.paints`'te tutar (2e dump, 3. adım). Sayaç `dl::PAINTS` (sim stderr, `/api/status` `dl.paints`). Test `recorded_vm_drawing_matches_direct_drawing` (mutasyonla doğrulandı); golden 13/13 iki modda, paint örneği 147 paint listesi.
  - Cihaz (`perf/2d`, `perf/dl2d`): bench mod 0 10.8 → 11.3 Minstr/s (çizim opcode'ları `exec_one`'dan çıkınca döngü küçüldü). Tablet ana ekranı draws: kapalı 8.3, açık 6.8 / 7.0 ms/s (2c tekrarı 8.8 / 8.0; koşular arası ~1 ms/s oynuyor).
  - Henüz yapılmayan: paint listesini capture yerine widget çiziminde clip'e göre replay etmek (capture semantiği tüm backend'lerde). Sim'de bu, `paint` örneğinin restore'larda silinen çizimini düzeltir ama golden hash'lerini değiştirir; 3. adımla birlikte ele alınacak.
- [x] **2e** (2026-11-11): `render::dl_dump(ctx, id, out)`: id yoksa DFS sırasıyla widget başına bir satır (id, tür, kutu, son çizimdeki görünüm anahtarı + cache durumu: `cached N ops`, `seen` = bir kez doğrudan, `evicted`, `not drawn`; katman dışında görünüm değiştiyse `look changed`; `on_paint` listesi), id ile o satır + listeler op op (`List::write_text`: alanlar + dilim verisinin başı). Anahtar dump'ta yeniden hesaplanmaz: katmandaki widget'lar katman koordinatlarında anahtarlanır, bu yüzden `draw_body` her widget'ın son anahtarını `render.drawn_keys`'te tutar. exynos `GET /api/dl[?id=N]` (CPU0 geçişler arasında yazar, `net::dl_service`); sim `--dl-dump` (her `--dump-at` karesi için `dl_MS.txt`) ve etkileşimli çalışmada sağ tık = imlecin altındaki en üstteki widget'ın listeleri stderr'e (`render::widget_at`). Kartta denendi (tablet: pager listesi, hatalı id → 400); sağ tık elle denenmedi.

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

- [x] **Hazırlık** (2026-11-11): `ferrite-core/src/snapshot.rs`. C sınıfı prop id'ye göre değil, (widget, prop) çiftine göre her render pass'te belirlenir: çalışan bir tween'in prop'u ve her kaydırma kabının `scroll_y`'si (`layer_alpha` okuması VM'deki slot değerini verir, A'dır). `runtime::snapshot_fill` render fazının sonunda bunları `render.snapshot`'a yazar (`Vm::snapshot_anims` + DFS'te `FLAG_CLIP_CHILDREN`); `OP_W_GET` `Snapshot::read`'den geçer (boş snapshot = tek `is_empty` kontrolü): C okumalarını sayar (`snapshot::READS`), `snapshot::STALE` açıkken snapshot'tan cevaplar (`STALE_DIFF` = canlıdan farklı cevaplar). Sim: sayım hep açık (dump sonunda stderr), `FERRITE_SIM_STALE_READS=1` / `golden.py --stale`; exynos `POST /api/snapshot?track=&stale=`, `/api/status` `snapshot`. Test `reads_of_animated_props_come_from_the_render_snapshot_when_stale` (mutasyonla doğrulandı).
  - Sonuç: hiçbir golden senaryosu C değeri okumuyor (sayaç 0), `--stale` ile 13/13 aynı; örnek `.fl`'lerde `get(scroll_y)` ya da animasyonlu prop okuması yok (kendi durumlarını tutuyorlar). 1 frame eski okuma mevcut uygulamaları bozmaz. Kartta bench 11.4 Minstr/s (değişmedi).

## Adım 3 (taslak): Render ayrı çekirdekte

Bulgular (2026-11-11):
- Tablet ana ekranında ağaç küçük: nesne arena'sı ~20 KB (106 widget, 145 işaretçi).
- GPU compositor (varsayılan) ekranın çoğunu `draw_widget`'tan geçirmeden çizer; display list'ler yalnız
  CPU pass'lerinde (katmanlar, compositor'un reddettiği alanlar) oluşur (2e dump'ı: tablette yalnız pager'ın
  listesi vardı).

Options:
- **A, tree copy:** the VM core copies the used arena and live pointer table to the render core each frame
  (~20 KB, tens of microseconds). The render core runs `render_dirty`, `compose`, and layer code against its
  copy, moving the compositor, layers, and captures together. Render writes (dirty clearing, `FLAG_RENDERED`,
  `SEC_PREV`, `SEC_DRAWN`, layer state, and `render.*`) must live in render-owned side tables; dirty/moved/changed
  data arrives from the VM as messages.
- **B, display lists:** the VM core keeps the render walk (DFS, occlusion, clipping, restoration), while the
  render core only rasterizes lists. The compositor must also consume lists (GPU tessellation or rasterization
  to sprites), and the VM core continues paying for the tree walk.

**Initial implementation: A** (user, 2026-11-11). Copying was considered cheap, and this let the compositor and layer paths move intact while keeping render work off the VM core. Display lists remain on the render core (CPU passes and `on_paint` lists); `on_paint` is recorded on the VM core and sent as a list (2d). This was the first implementation, not the target data-transfer design.

### Render data between the two cores (redesign)

Target: the VM CPU is the sole owner of the widget tree; the render CPU reads a renderer-specific immutable snapshot and never accesses `ObjPool`. The VM produces a compact, contiguous `RenderSnapshot` with widget properties, depth-first paint order, required text/graph data, and dirty regions. Allocator state, ownership tables, and unrelated VM strings/arrays are excluded.

Snapshot buffers cross a single-producer/single-consumer ownership channel. A published buffer stays immutable until rendering finishes and returns it to the VM. The VM prepares the other buffer while rendering runs. If rendering falls behind, intermediate unconsumed states are replaced with the newest snapshot instead of accumulating in a queue. Render-owned records (previous rects, cache, layers, and dirty tracking) stay on the render CPU. Resources use render-side handles with defined lifetimes instead of VM pointers.

Migrate incrementally: first separate render reads from `ObjPool` through the `RenderSnapshot` read API and keep mutable render state render-owned; next replace `sync_view` with snapshot production; finally add snapshot-buffer ownership to the `render_start`/`render_poll` handoff. Keep the tree-copy implementation as a reference until simulator golden output confirms equivalent behavior. Snapshot construction adds work on the VM CPU; the goal is to stop transferring unrelated allocator/object data and let the VM prepare the next scene while rendering continues.

| | Work | Verification |
|---|---|---|
| 3a | Move render-owned widget state out of the tree: `FLAG_RENDERED`, `FLAG_PAINTED`, `SEC_PREV`, `SEC_DRAWN` become pointer-indexed `RenderRec` entries in `RenderState`. Dirty bits stay in the tree and arrive as VM-to-render messages. | Same output in single-core tests and both golden modes |
| 3a+ | Remove buffered rendering; keep dirty rendering with one `FLAG_DIRTY` bit and one previous rect per widget, reducing transferred state. | Tests and golden output match in both modes |
| 3b ✓ | Two contexts on one core (`view.rs`, `Ctx::view`, `render::with_view`; `RenderMsg`): the render context keeps render-owned state and a packed `RenderSnapshot`. At sync, capture reachable widget records plus referenced text and graph samples into reusable buffers; do not copy the VM arena or pointer table. Transfer dirty/changed data and clear it on the VM; record `will_move` using old absolute rects before updating the scene. Render accessors read the scene facade, never the VM pool. | Golden output matches; measure capture cost |
| 3c | Move the render pool and render phase to a second core (Exynos CPU2, including LCD/G2D/GPU ownership and interrupts). Route touch input (`FullInput`: press feedback, scrollbar, slider) through messages instead of direct `render_dirty` calls. The VM publishes and continues without waiting. | Device A/B performance; golden output (sim remains single-core) |
| 3c-4 | Prepare the packed scene on CPU0 before polling the in-flight render. If CPU2 is still busy, keep the VM-owned buffer private and continue VM work; once the render context returns, publish the scene by swapping buffer ownership. Reuse the returned buffer for the next capture. Render messages and dirty-state transfer remain at the context handoff. | Core and Exynos cross-target checks; measure VM capture time and render overlap |

3c substeps:
- **3c-1 ✓** A render-side `Ctx` (`Ctx::render_ctx`) owns the render pool and reaches the same LCD/flash hardware; fonts, images, and filesystem state are copied. Single-core behavior matches golden output on sim and device; performance is within measurement noise.
- **3c-2 ✓** Ownership handshake: IDLE lets the VM sync; BUSY means rendering is running. `render_dirty` submits work or leaves it for the next pass when busy. Render-side follow-up work (`flush_dirty`, layer placement) stays there. VM direct drawing (`on_paint`, drawing builtins, keyboard overlay, error screens) waits until render is IDLE. Single-core mode remains synchronous.
- **3c-3 ✓ (partial)** Exynos CPU2 (`rcore.rs`) runs render jobs while CPU0 retains presentation (flush and G2D interrupt); each core cleans its own L1 at handoff. Passes with `on_paint` still render on CPU0. Device measurements: tablet longest pass 11.3 → 1.2 ms; scrolling CPU0 render 685 → 62 ms/s (presentation 57.9 → 53.4/s with pacing enabled). Presentation difference was measurement noise over 10-second windows (CPU2 52.7/s, CPU0 52.65/s); a GPU frame driven from CPU2 costs about 0.3 ms more (`show` cache cleans). Remaining: move G2D interrupt/GPU ownership to CPU2 and A/B on device.

### Continuation checkpoint (2026-11-11)

- Implemented `RenderSnapshot` scene capture with packed widget/text/graph data; the render-side pool no longer copies or reads the VM arena or pointer table. Snapshot buffers alternate ownership between CPU0 and the render context.
- `render_dirty` captures on CPU0 before polling the in-flight render. When the render context is still away, CPU0 keeps the latest scene private and continues VM work. Once the context returns, the scene is handed over with the render messages and dirty state. The reusable buffer returns to CPU0 for the next capture.
- Core mock and Exynos ARM cross-target `cargo check` both passed. No tests were run in this continuation.
- The tablet monitor stress test was active for the latest five-window capture (`python ..\\ferrite-ui\\tools\\ferrite_cli.py -p tcp://192.168.88.120 perf --interval 2 --count 5`). The five windows were stable: about 2,034–2,036 passes per 2 s, `render` 6.3–6.8 us/pass, animations 13.4–13.6 us/pass, 116–117 presents per window, and G2D/GPU drawing about 1.94–2.13 ms/present.
- `draw_widget calls: 0` does not mean the stress test had no dirty widgets: Exynos uses the GPU compositor for widget rendering, outside that CPU widget-draw counter. G2D operation and present counts confirm active rendering. The CLI's `render` phase is CPU0-side time and does not isolate `RenderSnapshot::capture`; `render walk` is not a reliable measure of CPU2 work when the cores overlap.
- Added profiler sub-phases `scene capture` and `scene handoff` (CPU0 microseconds per window/pass and longest pass). The standard perf response remains within its 255-byte payload limit; the longest-pass breakdown omits audio, animation, miscellaneous tasks, and frame-end time to make room.
- The first instrumented five-window run stalled presentation after its first sample. Code review found the runtime and CPU2 renderer both update profiler sub-phase counters; those counters were unsynchronized. Changed the cycle and event counters to atomics, rebuilt, and repeated the run. The screen remained live in both modes; the stalled batch is retained separately.
- Matched device A/B (five 2-second windows per mode, tablet Monitor stress, 1000 MHz): CPU2 on averaged 2,031 passes, 12.4 ms CPU0 render time, 7.50 ms scene capture, 3.65 ms handoff, 116 presents, and 216.5 ms G2D/GPU time per window. CPU2 off averaged 1,792 passes, 256.2 ms CPU0 render time, 4.29 ms scene capture, 1.95 ms handoff, 116 presents, and 225.1 ms G2D/GPU time per window. The monitor held 57–58 FPS in both modes and screenshots showed the same stress scene. Raw windows: `perf/3c4-scene/rcore-on.json`, `perf/3c4-scene/rcore-off.json`; stalled first attempt: `perf/3c4-scene/rcore-on-stalled.json`.
- Re-enabling CPU2 after the off-mode windows first caused a render stall in the RAM-only build. A later run of the saved firmware also stopped changing the display during Monitor stress, while touch input still worked; no panic line appeared on UART. This points to a present-path stall rather than a halted render core.
- `lcd::flush` previously returned immediately whenever a G2D present was in `COPY`. If a completion interrupt was missed, no code polled the band or timed out unless a later CPU drawing operation fenced overlapping rows. It now polls G2D completion and falls back to a CPU copy after 250 ms. The fallback disables G2D drawing/present for the rest of the boot and switches the display buffer.
- Exynos release `cargo check` passed after the present-path change; saved firmware was not modified.
- RAM-only release image initially ran Monitor stress for 129 s, with the present counter advancing 42 → 1,708 → 4,004 (about 57 fps). It froze again after about 6 minutes. During the freeze, `/api/status` still responded and `passes` advanced 417,528 → 431,210, but compositor frames stayed at 20,240 and presents at 18,488; `/api/screen` timed out. This indicates `render_away` remained true while CPU2 was waiting on the present path.
- Root cause in the handoff: `frame_end` skips `lcd::flush()` while CPU2 owns the render context, while CPU2's `wait_rows()` intentionally leaves the CPU0-owned G2D present to CPU0's interrupt. The timeout added above therefore could not run in this state. `frame_end` now calls `lcd::service_present()` while rendering is away, so CPU0 polls or times out the G2D job and CPU2 can continue.
- The follow-up RAM-only image was sent through `uart_boot` HTTP (CRC OK, 0.4 s; not saved). With Monitor stress active, status at uptime 242 s showed 12,077 presents; at uptime 501 s it showed 26,637 presents and 14,560 CPU2 jobs since the prior sample. The user confirmed the display stayed live beyond the previous ~6-minute failure window.
- No tests were run.

| 3d ✓ (kısmi) | VM'in ekrana doğrudan çizimi (`on_paint` dışı `fillRect`…) ve `on_paint` listeleri render çekirdeğine liste olarak; C okumaları (`snapshot.rs`) seqlock'lu snapshot'tan | `paint` örneği, `--stale` |

### Riskler

- 2a büyük ama mekanik (~150 çağrı noktası). `font`/`image` akışları en zor kısım.
  Büyük görseller `Pixels` değil, `Image{res_id}` ile kaydedilmeli.
- Widget başına liste için bellek tavanı ve LRU baştan olmalı.
- AA kenarlarında piksel eşitliği aynı clip sırasına bağlı. Golden testler 2b'nin kabul kriteri.
