# SalamanderRS Web v0.9.1 — konfigurovatelné kořeny v bezpečném režimu

**Co se změnilo proti v0.8:** Zápis v omezeném režimu už není vázaný na
`/tmp`. Kořeny mohou ležet například pod `/home`, `/AI/data/karel` a
`/AI/models/karel` nebo mohou být bind-mounty `/data` a `/models` uvnitř
kontejneru. Landlock se při povolení zápisů zapíná stejně jako dřív
**před vytvořením vláken serveru**. Když není dostupný, server se zápisy
odmítne spustit. `--unrestricted` zůstává oddělený režim pro místního
administrátora bez Landlocku a s HTTP Basic autentizací.

Toto je stále **vývojový prototyp**, nikoliv bezpečný veřejný nebo
víceuživatelský souborový server. V omezeném režimu zatím není HTTP
přihlašování, server proto naslouchá pouze na `127.0.0.1`. Nezveřejňujte
jej přes Tailscale ani proxy. Než se připojí skutečná data, musí být
provedena autorizace přes AI Platform a audit zbývajících operací.

## Režimy

| Spuštění | Kořeny | Zápis | Landlock | Ověření HTTP |
| --- | --- | --- | --- | --- |
| Bez přepínače | Nakonfigurované DATA/MODELS, standardně `/data` a `/models` | ne | ne; soubory omezuje aplikace | žádné; localhost |
| `SALAMANDER_ENABLE_WRITES=1` | **Libovolné existující adresáře kromě `/`** | ano, pod ne-root UID | **povinný**; při chybě se proces nespustí | žádné; localhost, zatím jen pro testování |
| `--unrestricted` | Standardně `/` a `/` | ano | **vypnutý** | povinné HTTP Basic `admin` |

V omezeném režimu nelze nastavit některý kořen přímo na `/` (ani při
pouhém čtení); k administraci celého hostitele slouží výhradně explicitní
`--unrestricted`. Zapisovatelný omezený režim se nespustí jako UID 0.
Všechny kořeny se před spuštěním kanonizují a musí skutečně existovat.
Ukládání souborů vyžaduje `SALAMANDER_ENABLE_WRITES=1`; samotné nastavení
cest **zápis nezapne**.

### Bezpečnostní test mimo `/tmp`

Pod přihlášeným uživatelem `administrator` (nikoliv root):

```bash
mkdir -p "$HOME/salamander-sandbox-check/data" "$HOME/salamander-sandbox-check/models"
SALAMANDER_DATA_ROOT="$HOME/salamander-sandbox-check/data" \
SALAMANDER_MODELS_ROOT="$HOME/salamander-sandbox-check/models" \
SALAMANDER_ENABLE_WRITES=1 \
./target/release/salamander-rs-web --verify-sandbox
```

Test ověří zápis **uvnitř obou kořenů** a blokování čtení/zápisu **mimo
obě cesty**. Před aktivací Landlocku vybere existující čitelný vnější
soubor a zapisovatelný adresář pro nezávislé kontrolní pokusy.
Očekávaný výsledek:
`Landlock verification succeeded: outside reads and writes denied.`

Služba může po úspěšném testu běžet pod stejnou konfigurací bez
`--verify-sandbox`. Je nutné připojení přes lokální SSH tunel; **na
produkčních uživatelských datech se zatím netestuje**.

### Příprava pro AI Platform

Na hostiteli budou skutečné uživatelské stromy například:

```text
/AI/data/karel
/AI/models/karel
```

V budoucím izolovaném kontejneru musí být dostupné jako bind-mounty:

```text
Host: /AI/data/karel   -> kontejner: /data    (rw)
Host: /AI/models/karel -> kontejner: /models  (rw)
```

Aplikace pak uvnitř kontejneru uvidí tyto kořeny:

```text
SALAMANDER_DATA_ROOT=/data
SALAMANDER_MODELS_ROOT=/models
SALAMANDER_ENABLE_WRITES=1
```

Kontejner poběží pod číselným UID:GID přiděleným platformou, například
`20001:20001` pro uživatele `karel`, nikoliv jako root. Nepřipojovat
`/AI` ani Docker socket; runtime musí mít jen potřebné knihovny,
read-only základní image a nutné pracovní adresáře. Landlock je pouze
**další vrstva** vedle izolace mount namespace, práv UID/GID a autentizace
přes uživatelský konektor. Docker/seccomp musí umožnit Landlock ABI v3
(Linux 6.2+); pokud sandbox nelze aktivovat, **neobcházet chybu**.

Než tento postup opravdu zavedeme, zbývá dokončit autentizaci a
zabezpečení F2/F5/F6/F7/F8/ZIP proti závodům se symlinky a přejmenováním.
Kvóty jsou stále nezapojené a UI nezobrazuje autoritativní zbytek kvóty.
Samotná změna kořenů není souhlas s produkčním nasazením.

### Lokální správce serveru (pouze pro experimenty)

`--unrestricted` výslovně vypne Landlock, vyžaduje heslo
`SALAMANDER_ADMIN_PASSWORD` a dovolí také nastavit kořeny na `/`.
Server je stále dostupný pouze přes `127.0.0.1`; používejte SSH tunel.
Spouštění s rootovými oprávněními zůstává vysoce rizikové, protože
souborové jádro stále obsahuje cesty náchylné k souběžným změnám.
**Nejde o produkční správce rootovských souborů.**

### F4 textový editor (od v0.8)

F4 otevře textový UTF-8 soubor do 1 MiB. Uložení přes Ctrl+S používá
vedlejší soubor a přejmenování; kontroluje změny, hardlinky, ACL a xattr.
Není určený pro citlivé konfigurační soubory nebo souběžně měněná data.

## Sestavení

Z kořene workspace s `Cargo.toml`:

```bash
cargo test -p salamander-core -p salamander-web
cargo build --release -p salamander-web
```

Pokud už máte v pracovním stromu `Cargo.lock`, ponechte ho a použijte
`cargo build --release --locked -p salamander-web`. Archiv vydání
`Cargo.lock` neobsahuje; při prvním sestavení ho Cargo vytvoří. Pro
opakovatelná vydání musí být výsledný lockfile verzovaný.

---

**Níže je archivní dokumentace starších verzí.** Údaj o nutnosti `/tmp`
platil do v0.8 a ve v0.9 už **neplatí**.

## Starší změny a podrobná dokumentace v0.7/v0.6

# SalamanderRS Web v0.7 - bezpečnostni testovaci verze

**v0.7 je pokusne zabezpecena navazujici verze v0.6.** Dvojpanelove UI,
kopirovani, presuny, ZIP, upload i konflikty zustavaji zachovane.

V zapisovem testovacim rezimu se nově uplatni Linux **Landlock**:
proces smi otevirat a menit soubory pouze pod nakonfigurovanymi dvema
koreny; pokud Landlock ABI >=3 (Linux 6.2+) neni dostupny nebo jej
napriklad blokuje seccomp, aplikace **odmitne start se zapisem**.
Landlock se zapina pred vytvorenim vlaken Tokio.

U ZIP se kontroluji nebezpecne cesty, symlinky a hardlinky; preflight
prochazi archiv prubezne bez ukladani neomezeneho indexu do pameti.
Pri rozbalovani se kontroluje take *skutecne* mnozstvi dekomprimovanych
dat a pocet polozek, nikoliv jen velikost deklarovana v archivu.

**Zatim stale zadna autentizace ani podpora produkcniho zapisu.**
Oba koreny pro zapis musi lezet pod `/tmp`; HTTP zustava jen na
`127.0.0.1`. Landlock neni izolace UID ani plnohodnotna ochrana proti
metadatovym operacim pri soubehu procesu. Tento build NESMI byt nasazen
na skutecna uzivatelska data ani napojen na uzivatelsky Tailscale.

Detaily: [README_SECURITY_WEB.md](README_SECURITY_WEB.md).

## Sestaveni a testovani v0.7

```bash
cd ~/SalamanderRS-0.6.0-alpha.1-Web-v0.7-security/SalamanderRS-Rust-0.6.0-alpha.1
cargo test -p salamander-core -p salamander-web
cargo build --release -p salamander-web
```

Overeni Landlock v nove kratke instanci (NEzapina HTTP server):

```bash
mkdir -p /tmp/salamander-test/data /tmp/salamander-test/models
SALAMANDER_DATA_ROOT=/tmp/salamander-test/data \
SALAMANDER_MODELS_ROOT=/tmp/salamander-test/models \
SALAMANDER_ENABLE_WRITES=1 \
./target/release/salamander-rs-web --verify-sandbox
```

Ocekavana posledni radka:
`Landlock verification succeeded: outside reads and writes denied.`

Pak spust bez argumentu `--verify-sandbox` stejne jako v0.6.

---

# SalamanderRS Web v0.6 — konflikty při přenosu souborů

Samostatný dvojpanelový webový správce v Rustu (Axum). HTML/CSS/JavaScript
jsou zabudované přímo v binárce pomocí `include_str!`. Funkce používají
původní `salamander-core`; grafická GTK verze a znaková TUI verze si
zachovávají stejné chování. Pouze v GTK zdroji je úprava `.clone()` nutná
kvůli rozšířené variantě `ConflictAction::Rename` ve společném jádře.

**Toto je stále TESTOVACÍ PROTOTYP bez autentizace. Nepoužívejte ho se
skutečnými uživatelskými daty ani přes veřejnou síť / Tailscale.**

## Nové ve v0.6

- **F5 / F6 při kolizi** vyvolají vlastní webový dialog. Zobrazuje
  zdrojovou a cílovou cestu i velikost běžných souborů (je-li dostupná).
  Uživatel vybírá **Přepsat / Přeskočit / Přejmenovat cíl / Zrušit operaci**.
- **Použít pro všechny** funguje pro přepsání nebo přeskočení dalších
  kolizí včetně souborů uvnitř slučovaných adresářů. Přejmenování platí
  vždy pouze pro jeden konflikt; nové jméno smí být jen jeden název položky,
  nikoliv cesta.
- **Přejmenování při konfliktu** neodstraňuje stávající cílový soubor.
  Kopírovaná či přesouvaná položka dostane nové jméno ve stejném cílovém
  adresáři. Pokud je obsazené i nové jméno, zobrazí se další konflikt.
- **Nahrávání z PC**: opakované nahrání souboru se stejným názvem vyvolá
  stejný dialog. Volby jsou přeskočit, explicitně přepsat, nahrát pod
  novým jménem nebo zrušit; „použít pro všechny“ funguje pro sérii souborů.
- **Atomické zveřejnění uploadu:** nejdřív se celý soubor streamuje do
  `.partial` a synchronizuje. Teprve potom se cíl nahradí jedním `renameat`
  (jen při výslovném přepsání), nebo zveřejní bez přepisu přes `renameat2`
  s `RENAME_NOREPLACE`. Není možné tímto způsobem přepsat adresář nebo
  symbolický odkaz. Přenos při chybě běžně smaže neúplný `.partial`.
- **Počítání přeskočených položek** provádí přímo souborové jádro, včetně
  voleb „přeskočit všechny“.
- **Obnova dialogu po refreshi prohlížeče**: běžící F5/F6 nebo ZIP job
  si karta ukládá do `sessionStorage` a po obnovení znovu načte jeho stav.
  Při nepřítomnosti odpovědi na konflikt po 10 minutách server operaci zruší,
  aby nezůstal trvale zablokovaný zápis. Částečně provedené operace se
  tím samy nevracejí.

## Dosavadní funkce

- Dva panely; v obou nezávisle DATA/MODELS, navigace podadresáři,
  označování více souborů pomocí Ctrl+klik, Shift+klik nebo mezerníku.
- F2 přejmenování, F5 kopírování, F6 přesouvání mezi panely,
  F7 nový adresář, F8 trvalé mazání s potvrzením.
- **ZIP vytvořit / ZIP rozbalit** do druhého panelu, bez přepisování
  existujících cílů. ZIP se záměrně **neprochází jako adresář**.
  ZIP extrakce je testovací: max. 20 000 položek a 20 GiB deklarovaných
  rozbalených dat, bez šifrovaných ZIPů a symbolických odkazů.
- Upload více souborů ze zařízení a download jednoho souboru do zařízení,
  obojí streamované (bez načtení celého modelu do paměti).
- Kopírování, přesun a ZIP běží na pozadí, nejvýše jedna zápisová operace
  najednou. Soubory se přenášejí postupně.
- Volné místo se zjišťuje pro **aktuální filesystem** každého panelu.
  **Kvóty nejsou ještě napojené**, takže zobrazené dostupné místo je
  pouze horní mez. Cílové pravidlo AI Platform: zobrazit
  `min(volné místo na filesystemu, zbývající vynucená kvóta)`.

## Bezpečnost a omezení

- HTTP poslouchá **pouze na `127.0.0.1`**, bez autentizace.
- Zápisy jsou výchozím stavem zakázané. `SALAMANDER_ENABLE_WRITES=1`
  je povolí jen tehdy, když **oba** kanonické kořeny leží v podadresářích
  `/tmp`, nikoliv přímo v `/tmp`.
- Upload/download používají Linux `openat2` a otevřené deskriptory kořenů;
  cesta nesmí vést přes symlink ani `..`. Vyžadují Linux 5.6+.
- **F2/F5/F6/F7/F8 a ZIP stále používají v jádře cestová jména.**
  Nejsou zcela odolné vůči paralelní výměně adresářů nebo symlinků jiným
  procesem. Samotný kontejner bez root přístupu, vazba na konkrétní UID/GID,
  descriptor-relative operace a autentizace musí být dodělané před
  nasazením do AI Platform.
- F5 přepsání používá dočasné soubory a obnovu při běžné chybě, ale
  nenahrazuje transakční filesystem. **Při pádu procesu/hostitele v kritické
  části operace mohou vzniknout částečné výsledky.** F6 při přesunu přes
  hranici filesystemů musí kopírovat a pak mazat původní zdroj.
- Nejsou hotové SFTP, obnova přerušených uploadů, HTTP Range při downloadu,
  bezpečný editor, omezování zdrojů ZIP bomb, integrace kvót ani autorizace
  uživatele přes Gateway API. SFTP je v plánu; nejde o funkci v0.6.
- Při chybě ZIP extrakce mohou zůstat již rozbalené soubory. Výchozí
  limit jednoho uploadu je 256 GiB (`SALAMANDER_MAX_UPLOAD_BYTES`).
- ZIP používá libarchive společného jádra. Po kompilaci ověřte `ldd`;
  může obsahovat dynamickou vazbu na `libarchive.so.13`.

## Sestavení na `ailab`

Z adresáře projektu obsahujícího kořenový `Cargo.toml`:

```bash
cargo test -p salamander-core -p salamander-web
cargo build --release -p salamander-web
ls -lh target/release/salamander-rs-web
ldd target/release/salamander-rs-web
```

Spuštění proti **testovacím** adresářům:

```bash
mkdir -p /tmp/salamander-test/data /tmp/salamander-test/models
SALAMANDER_DATA_ROOT=/tmp/salamander-test/data \
SALAMANDER_MODELS_ROOT=/tmp/salamander-test/models \
SALAMANDER_ENABLE_WRITES=1 \
SALAMANDER_PORT=8090 \
./target/release/salamander-rs-web
```

Z Windows lze otevřít SSH tunel:

```powershell
ssh -N -L 8090:127.0.0.1:8090 administrator@ailab
```

V prohlížeči pak otevřete `http://127.0.0.1:8090`.

## Doporučený ověřovací scénář

V jiném SSH okně připravte obsah obou stromů:

```bash
printf 'původní DATA\n' > /tmp/salamander-test/data/kolize.txt
printf 'starý cílový MODELS\n' > /tmp/salamander-test/models/kolize.txt
```

1. Vlevo DATA, vpravo MODELS. Označte `kolize.txt` a stiskněte F5.
   Vyzkoušejte „Přeskočit“ (cíl zůstává beze změny), pak F5 znovu
   s „Přejmenovat cíl“ (v cíli vznikne nový soubor a původní zůstane).
2. Znovu F5 a zvolte „Přepsat“; ověřte, že nový obsah nahradil starý.
3. Vyzkoušejte F6 s přejmenováním cíle; po dokončení se původní zdroj
   odstraní a obě cílové položky zůstanou zachované.
4. Vytvořte dva konfliktní soubory a zkuste „Použít pro všechny“ při
   kopírování složky, včetně souborů hluboko v podadresářích.
5. Z Windows nahrajte stejný soubor dvakrát a ověřte Přeskočit, Přepsat,
   Přejmenovat a aplikaci rozhodnutí na více souborů.
6. Ověřte obyčejné kopírování, přesun, ZIP a upload/download bez kolizí,
   aby nová verze nezměnila staré funkce.

**Validace v tomto prostředí:** zdrojové úpravy, patch a syntaxi JavaScriptu
jsme ověřili, Rustový toolchain zde ale není nainstalovaný a není dostupná
síť pro stažení. `cargo test` i provozní test v Rustu musí proběhnout na
`ailab` před další integrací.


## v0.9.1 — vytvoření prázdného souboru

V aktivním panelu stiskněte **Shift+F4** nebo klikněte na tlačítko
**Nový soubor**. Zadejte název a potvrďte. V aktuálním adresáři
zvoleného stromu (`DATA` nebo `MODELS`) vznikne **prázdný soubor
(0 bajtů)**. Existující položka se nikdy nepřepíše; kolize vrací HTTP 409.
Nově vytvořený textový soubor pak můžete otevřít klávesou F4.

Backend: `POST /api/file/create` s JSON `{"root":"data","path":"", "name":"novy.txt"}`
a hlavičkou `X-Salamander-Request: create-file`. Volání funguje jen při
povolených zápisech; používá `openat2` pro přístup do cílového adresáře
vzhledem k deskriptoru kořene a `openat(O_CREAT|O_EXCL|O_NOFOLLOW)`
pro bezpečné vytvoření souboru bez přepsání.

v0.9.1 zároveň opravuje chybu překladu E0034 v editoru (`Read::by_ref`).
