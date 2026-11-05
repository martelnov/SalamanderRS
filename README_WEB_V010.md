# SalamanderRS Web v0.10 – F3, metadata obrázků, obnovitelné přenosy

Vychází z v0.9.1. GTK ani TUI se nemění. Webová aplikace zůstává **jedinou
binárkou** obsahující HTML/CSS/JS (`include_str!`), bez Node.js/GTK při běhu.

## Funkce

- **Shift+F4** vytváří prázdný soubor v aktivním panelu (převzato z v0.9.1).
- **F3** otevře čtecí prohlížeč UTF-8 (první 2 MiB) nebo PNG/JPEG/WebP/GIF.
  Obrázky se streamují, nikoli načítají celé do RAM serveru. Náhled omezen na
  256 MiB souboru; zoom 1:1 a přechod mezi obrázky šipkami.
- Metadata: PNG `tEXt`, `iTXt`, `zTXt`, `eXIf`; JPEG EXIF, XMP, komentáře a
  APP13; WebP EXIF/XMP. GIF se zobrazuje bez specializovaného parseru metadat.
  Podporovaná generační pole `parameters` (Automatic1111/Forge), `workflow`
  a `prompt` (ComfyUI) jsou zobrazená vedle obrázku. Podporováno kopírování
  promptu, seedu a workflow, export ComfyUI workflow JSON a export rozpoznaných
  **původních metadat** do JSON včetně Base64 zdrojových bajtů. Neznámé
  parametry se neodhadují. Limit 2 MiB na položku / 8 MiB rozpoznaných dat.
- **Upload**: soubor se po výběru posílá po 4MiB blocích. Serverový checkpoint
  (`.salamander-resume-<id>.json`) a skrytá `.part` vznikají *ve stejném
  cílovém adresáři*, aby závěrečný rename zůstal atomický i při mountpointech.
  Checkpoint se potvrdí až po synchronizaci dat, přežije pád programu.
  Po výpadku znovu vyber **stejný místní soubor**. Otisk prvního a posledního
  128 KiB spolu s velikostí a mtime brání běžné záměně souborů, **není to však
  plný SHA-256 celého souboru**. Průběžný upload vyžaduje bezpečný kontext
  (`http://127.0.0.1` nebo HTTPS) kvůli Web Crypto API.
- **Download**: HTTP `Range`, `206`, `416`, `ETag`, `If-Range`. V prohlížečích
  s File System Access API (Edge/Chrome na localhost/HTTPS) průběžné ukládání
  na disk a checkpoint nejvýše každých 256 MiB. Identita vzdáleného souboru
  a přidělené místní oprávnění jsou uložené v IndexedDB. Po výpadku klikni
  znovu na Stáhnout. Ostatní prohlížeče použijí běžné stahování spravované
  prohlížečem; navázání tam závisí na jeho možnostech.
- Dvoukořenový bezpečný režim s Landlock z v0.9 zůstává. `/tmp` není povinné.
  Rozpracované přenosy jsou skryté i při zapnutém zobrazování skrytých souborů.

## Sestavení

Na Ubuntu jako běžný uživatel s nainstalovaným rustup:

```sh
cargo test -p salamander-core -p salamander-web
cargo build --release -p salamander-web
```

Přibyla knihovna `flate2` (dekomprese PNG zTXt/iTXt). První sestavení obnoví
`Cargo.lock`: uložit jej do repozitáře a další sestavení provádět s `--locked`.

## Spuštění v testovacím prostředí

```sh
mkdir -p /tmp/salamander-test/{data,models}
SALAMANDER_DATA_ROOT=/tmp/salamander-test/data \
SALAMANDER_MODELS_ROOT=/tmp/salamander-test/models \
SALAMANDER_ENABLE_WRITES=1 SALAMANDER_PORT=8090 \
./target/release/salamander-rs-web
```

Server stále naslouchá pouze na `127.0.0.1:8090`; ze vzdáleného Windows
počítače použij SSH tunel `ssh -N -L 8090:127.0.0.1:8090 administrator@ailab`.
Testovat pouze s testovacími daty. `--unrestricted` není potřeba.

Automatický HTTP smoke test: `bash tools/smoke_web_v010.sh`
(při běhu musí být port 28090 volný a sestavená binárka k dispozici).

## Omezení důležitá pro produkci

1. Bezpečný režim stále **nemá HTTP autentizaci**. Některé starší operace
   v `salamander-core` používají cesty náchylné k souběžným změnám. Nezavádět
   k reálným uživatelským datům AI Platform, dokud se neopraví a nespojí
   s ověřením identity konektoru.
2. Rozpracované uploady spotřebovávají diskovou kvótu. Existuje endpoint
   `POST /api/resumable/cancel`, ale automatické TTL/úklid ještě není součástí
   správce. Staré checkpointy je potřeba spravovat; nesmazat `.part`, pokud
   existuje platný checkpoint.
3. Při přepsání se původní soubor nahradí až po dokončení nahrávání.
   V případě **nové kolize přesně při závěrečném rename** se server zastaví
   s HTTP 409 a ponechá rozpracovaný přenos. Toto je konzervativní ochrana,
   nikoliv vyřešení kolize bez opakování.
4. Integrace kvót z AI Platform není implementovaná; samostatný správce zatím
   ukazuje pouze volné místo na filesystemu a „kvóta nezjištěna“.
5. Metadata jsou dekódovaná jen pro uvedené formáty a rozpoznané typy položek.
   Export obsahuje přesné zdrojové bajty rozpoznaných položek, nikoli úplný
   binární dump všech chunků či fotografických profilů.
6. Obnovitelný download neslibuje pokračování od posledního bajtu po *pádu*
   prohlížeče: soubor je potvrzován po 256MiB blocích (poslední nedokončený
   blok se opakuje). File System Access API může být v konkrétním prohlížeči
   nedostupné nebo vyžadovat opětovné oprávnění.
7. Přenosy sice kontrolují velikosti a verzi souboru, ale v0.10 **neprovádí
   plný SHA-256 přenesených dat**. Pro kritické modely ověř součet samostatně.

## Ruční ověření

- Shift+F4 `novy.txt` → F4 editace → F3 náhled textu.
- PNG generovaný v ComfyUI s `workflow`, `prompt`; PNG z A1111 s `parameters`;
  JPEG a WebP s EXIF/XMP. Zkus kopírování polí a export workflow JSON.
- Velký upload: přeruš SSH tunel uprostřed, obnov spojení a vyber stejný
  soubor. Zkontroluj jeho konečnou velikost a `sha256sum` obou kopií.
- Download v Edge/Chrome: přeruš spojení, obnov jej a klikni na Stáhnout.
  Ověř konečný `sha256sum`.
- Změň zdrojový soubor před navázáním downloadu: systém musí odmítnout
  navázání na starou verzi.
- Ověř, že F5/F6/F8 a ZIP fungují jako předtím.
