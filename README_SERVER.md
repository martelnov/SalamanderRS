# Salamander RS 0.6.0-alpha.1 — Linux servery + desktop v Rustu

Toto je **vývojová verze zdrojových kódů** vycházející z původního
Salamander RS 0.5.1. Architektura je rozdělená na čtyři cargo balíčky:

| Crate / program | Účel | Potřebuje grafiku? |
|---|---|---|
| `salamander-core` | PŮVODNÍ souborové operace, archivace, model, ACL, SFTP, SSH askpass, hledání, trezor | ne |
| `salamander-rs` | PŮVODNÍ grafické GTK4 rozhraní | ano (GTK4) |
| `salamander-tui` → `salamander-rs-tui` | Dvoupanelové TUI s Ratatui/Crossterm | **ne** |
| `salamander-launcher` → `salamander` | Automatický výběr GUI / TUI | **ne** |

`gtk`, `glib`, X11, Wayland ani `ncurses` nejsou závislostí TUI. Funkce
souborového systému a SFTP nejsou implementované dvakrát — TUI volá
`salamander-core`, tj. stejné funkce, které používá GTK.

## Rychlý start na serveru (Ubuntu 24.04+/Debian 13)

Požadavky: Rust **1.85+**, `libarchive.so.13`, `libsodium.so.23`, OpenSSH.
Na Ubuntu:

```bash
sudo apt update
sudo apt install build-essential pkg-config libarchive13t64 libsodium23 openssh-client
# Rust přes rustup.rs, nebo kompatibilní distro balíčky:
# curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
./scripts/build-server.sh
./target/release/salamander --tui
```

Pro nainstalování programu na server:

```bash
sudo ./scripts/install-server.sh
ssh -t administrator@server salamander
```

**Výsledkem jsou dvě nativní kompilované binárky ELF, nikoliv Python.**
Binárka `salamander` je miniaturní spouštěč, na serveru automaticky vybere
`/usr/local/bin/salamander-rs-tui`. Jestliže na počítači běží grafika a v
stejném adresáři je i původní `salamander-rs`, automaticky použije GTK.
Ruční volba: `salamander --gui`, `salamander --tui`.

Ke spuštění již **Rust, Cargo ani kompilátor nemusí být nainstalované**.
Stačí zkopírovat obě ELF binárky na počítač se stejnou architekturou a
kompatibilními runtime knihovnami. Pokud cílový server nemá
`libarchive.so.13` a `libsodium.so.23`, musí se nejprve nainstalovat.
Kompletně statický ELF pro všechny distribuce toto vydání **nenabízí**.

## Desktopové sestavení

```bash
sudo apt install build-essential pkg-config libgtk-4-dev libarchive13t64 libsodium23 openssh-client
./scripts/build-desktop.sh
./target/release/salamander
```

### Klávesy v TUI

| Klávesa | Akce |
|---|---|
| Tab | aktivní panel |
| ↑/↓, PgUp/PgDn, Home/End | pohyb |
| Enter, Backspace | otevřít adresář / zpět |
| Insert, mezerník | označit soubor |
| F1 | nápověda |
| F2 / Ctrl+F2 | přejmenovat / změnit unixová práva (chmod) |
| F3 | prohlížeč TEXT/HEX (H) |
| F4 | místní editor `$EDITOR` |
| F5 / F6 | kopírovat / přesunout |
| F7 / F8 | vytvořit adresář / trvale smazat s potvrzením |
| Alt+F7 | rekurzivně vyhledat lokální název a volitelně obsah souboru |
| F9 | lokální shell, nebo vzdálený SSH shell |
| F10, Ctrl+Q | skončit |
| Ctrl+L | změnit adresář |
| Ctrl+K | připojit uložený SFTP server nebo vytvořit nový (N) |
| Ctrl+H | skryté soubory |
| Ctrl+R | obnovit |
| / | rychlé nalezení názvu |

Ctrl+F2 mění práva lokálních souborů i na SFTP (bez následování symlinků),
pomocí sdíleného engine oprávnění.

Kopírování a přesun **mezi místními panely** používá originální bezpečný
engine (konflikty, kopírování adresářů, metadata, příznaky). F5 z
otevřeného archivu rozbalí označené položky do lokálního protějšího panelu.
F3 z archivu funguje. Základní SFTP panel funguje s uloženými servery v
`~/.config/salamander-rs/servers` (stejná databáze jako GTK). Přímo v TUI
lze stisknout **Ctrl+K → N** a vytvořit nový profil zadáním hostitele,
uživatele, portu, volitelného klíče a dalších přepínačů SSH; hesla se do
profilu neukládají. Podporuje interaktivní ověřování hesel a host key přes
SSH_ASKPASS. TUI používá
stejné SSH/SFTP jádro jako GUI. Při ověřování otisku klíče je nutné jej
skutečně zkontrolovat; potvrzení je vyžadováno explicitně.

**Bezpečnost:** F8 znamená _nevratné smazání_, uživatel musí stisknout
`A` (Enter samo o sobě mazání nepotvrdí). Při kopírování konfliktů
lze přepsat nebo přeskočit každý soubor; Esc přeruší operaci.

### Co zatím TUI neumí (GTK původní funkce zachovává)

- editaci/mazání uvnitř archivů a vytváření archivů v TUI,
- přenos **přímo** SFTP→SFTP a bezpečný F6 přesun přes SFTP (F5 kopírování ano),
- grafické dialogy ACL/xattr a trezoru hesel (vytváření nových SFTP profilů
  již funguje; úpravy existujících profilů zatím nikoliv),
- zápis F4 upravených vzdálených/archivních souborů,
- některé pokročilé volby GTK (sloupce, výběr maskou, drag-and-drop apod.).

Na serveru tedy jde o **první terminálové rozhraní sdílející celý existující
backend**, nikoliv klon všech GTK funkcí do textového prostředí.

## Stav sestavení

Zdrojové kódy jsou připraveny k překladu na Linuxu. V prostředí, v němž
byl vytvořen tento balíček, nebyly nainstalované `cargo` ani `rustc`
a nebylo dostupné externí stahování. **Z tohoto důvodu nelze tvrdit,
že ZIP byl lokálně kompilován a otestován.** Spuštěné shell/syntax kontroly
nenahrazují `cargo check`, `cargo test` a interaktivní integrační testy.

Součástí je `.github/workflows/build.yml`, který na Ubuntu 24.04 při
spuštění CI sestaví TUI a GUI, spustí testy a uloží přeložené ELF binárky
jako GitHub Actions artefakty. Před produkčním nasazením prověřit zejména
F5/F6/F8, SFTP host key, přerušování kopírování a chování ve tmux/SSH.
