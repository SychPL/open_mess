# open_mess - samoaktualizacja z GitHub Releases

Data: 2026-10-27
Status: do review (Codex, GLM)
Powiązane: `.specs/2026-10-26-open-mess-design.md` (8.1, 8.3, 8.8, 15), `scripts/install.ps1`.

## 1. Cel

open_mess sam sprawdza, pobiera i - po zgodzie użytkownika - instaluje nowe wersje z wydań GitHub repozytorium `SychPL/open_mess`, a każde wydanie jest podpisane kluczem właściciela. Dziś aktualizacja to ręczne `scripts/install.ps1`; po tej zmianie użytkownik klika "Zainstaluj" w powiadomieniu albo w oknie.

## 2. Decyzje właściciela

- Źródło: GitHub, repozytorium `SychPL/open_mess` (publiczne), wydania (Releases) z podpisanymi plikami.
- Demon działa jako zadanie Harmonogramu `open_mess` przy logowaniu, z `%LOCALAPPDATA%\open_mess\bin\om.exe` (`scripts/install.ps1`).

## 3. Kontekst w repo

- `crates/open-mess`: demon (`daemon.rs`), okno (`ui/`), powiadomienia (`approval.rs`), stan w SQLite (`store.rs`), lokalne API.
- `crates/open-mess-host`: moduły WASM; `modules.rs` w `crates/open-mess` wiąże zgodę na moduł z SHA-256 pliku i manifestu (spec 8.3).
- `scripts/install.ps1`: zatrzymuje demona, przenosi działający `om.exe` na `om.old.exe`, kopiuje nowy plik i moduły, rejestruje i uruchamia zadanie `open_mess`.
- Wersja: `[workspace.package] version` w `Cargo.toml` (dziś `0.0.0`).

## 4. Wydanie (po stronie właściciela)

`cargo xtask release <wersja>` (semver, np. `0.1.0`):
1. wymaga czystego drzewa na `main` zgodnego z `origin/main` i zielonego `cargo xtask ci`;
2. ustawia wersję w `Cargo.toml`, commituje `Release <wersja>` i taguje `v<wersja>`;
3. buduje `om.exe` (release) i moduły platform (`open_mess_<nazwa>.wasm` -> `<nazwa>.wasm`, bez modułów testowych);
4. tworzy `manifest.json`: `{ "version": "<wersja>", "files": [ { "name": "om.exe", "kind": "exe", "size": n, "sha256": "..." }, { "name": "teams.wasm", "kind": "module", "size": n, "sha256": "..." } ] }`;
5. podpisuje `manifest.json` formatem minisign (Ed25519) kluczem prywatnym z pliku wskazanego przez `OPEN_MESS_RELEASE_KEY` (domyślnie `%USERPROFILE%\.open_mess\release.key`, chroniony hasłem, nigdy w repo); wynik `manifest.json.minisig`;
6. pushuje commit i tag, tworzy wydanie GitHub (`gh release create v<wersja>`) z plikami: `om.exe`, `<moduły>.wasm`, `manifest.json`, `manifest.json.minisig`.

Klucz publiczny leży w repo (`keys/release.pub`) i jest wkompilowany w `om` (stała). `cargo xtask release-keygen` tworzy parę kluczy raz.

## 5. Sprawdzanie i pobieranie (demon)

- Demon sprawdza 5 minut po starcie i potem co 24 godziny: `GET https://api.github.com/repos/SychPL/open_mess/releases/latest` (bez tokenu, z `User-Agent: open_mess/<wersja>`, limit czasu 30 s). Pomija wydania `draft` i `prerelease`.
- Wydanie jest kandydatem tylko, gdy jego wersja (z tagu `v<semver>`) jest **wyższa** od wersji działającego `om` - nigdy niższa ani równa (brak downgrade).
- Najpierw pobiera `manifest.json` i `manifest.json.minisig` (limit 64 KiB każdy) i weryfikuje podpis kluczem wbudowanym. Brak podpisu, zły podpis albo `version` w manifeście różna od tagu = wydanie odrzucone i zapisane w stanie aktualizacji jako błąd.
- Potem pobiera wyłącznie pliki z manifestu, każdy jako zasób tego samego wydania: nazwy tylko z białej listy (`om.exe` albo `<nazwa>.wasm`, gdzie `<nazwa>` to `[a-z0-9-]{1,32}`), bez separatorów ścieżek; rozmiar i SHA-256 muszą zgadzać się z manifestem; łączny limit 256 MiB.
- Połączenia tylko HTTPS do `api.github.com`, `github.com` i przekierowań na `objects.githubusercontent.com` / `release-assets.githubusercontent.com`; z systemowym proxy i magazynem certyfikatów, jak moduły (spec 8.3).
- Pliki trafiają do `%LOCALAPPDATA%\open_mess\updates\<wersja>\`; częściowe pobrania są usuwane; nic z tego katalogu nie jest uruchamiane przed pełną weryfikacją.

## 6. Zgoda użytkownika

- Domyślnie aktualizacja **nie** instaluje się sama. Po pobraniu i weryfikacji: powiadomienie Windows "open_mess <wersja> jest gotowy - Zainstaluj / Później" oraz w oknie sekcja Demon i logi -> Aktualizacje: bieżąca wersja, znaleziona wersja, "Sprawdź teraz", "Zainstaluj", ostatni wynik.
- Kliknięcie "Zainstaluj" (w powiadomieniu albo w oknie) zapisuje w SQLite zgodę: wersja, SHA-256 podpisanego manifestu, czas; zgoda jest ważna 10 minut i jednorazowa. Bez takiej zgody `apply-update` odmawia (sekcja 7), więc uruchomienie pomocnika z CLI przez agenta niczego nie instaluje (recenzja R1).
- Opcja w oknie "Instaluj aktualizacje automatycznie" (domyślnie wyłączona): wtedy demon sam zapisuje zgodę po pobraniu, ale tylko gdy w `pending.rs` nie ma akcji w stanie `waiting` (czeka na zatwierdzenie) ani `approved` (zatwierdzona, wysyłana), żadne wywołanie narzędzia nie trwa i żadne uruchomienie z wyzwalacza nie działa (recenzja R1).
- Żadne narzędzie MCP ani polecenie CLI nie zapisuje zgody ani nie zmienia tej opcji (jak zatwierdzenia, spec 8.6). `status` pokazuje tylko stan aktualizacji.

## 7. Instalacja

Zadania Harmonogramu (rejestruje je `scripts/install.ps1` i - przy pierwszym starcie po tej zmianie - demon, jeśli brakuje drugiego):
- `open_mess` - jak dziś: demon przy logowaniu;
- `open_mess-update` - bez wyzwalacza, akcja `conhost.exe --headless %LOCALAPPDATA%\open_mess\bin\om.update.exe apply-update --data-dir <folder danych demona>`. Osobne zadanie ma własny obiekt zadania (job), więc pomocnik nie należy do instancji zadania demona i `open_mess` da się po aktualizacji uruchomić ponownie (recenzja R1: proces potomny demona dziedziczyłby job zadania `open_mess`, a jego `MultipleInstances IgnoreNew` zignorowałby ponowny start).

Kroki:
1. Demon (po zgodzie z sekcji 6) kopiuje zweryfikowany `om.exe` z katalogu aktualizacji do `bin\om.update.exe`, sprawdza jego SHA-256, zapisuje w `<folder danych>\update-pending.json` wersję i katalog aktualizacji, uruchamia zadanie `open_mess-update` i kończy pracę.
2. `apply-update` (z `--data-dir`, więc działa też przy `OPEN_MESS_DATA_DIR` - recenzja R1):
   - czeka do 60 s, aż demon zwolni blokadę instancji (`daemon.lock`) i zakończy proces;
   - sprawdza w SQLite ważną, niewykorzystaną zgodę dla tej wersji i tego manifestu i oznacza ją jako wykorzystaną; brak zgody = koniec bez zmian;
   - weryfikuje ponownie podpis manifestu i sumy wszystkich plików w katalogu aktualizacji;
   - robi kopię zapasową: `state.sqlite3` (demon jest zatrzymany, więc kopia jest spójna), obecnych plików modułów i `om.exe` (kopia do `om.old.exe`; `om.exe` nie znika ani na chwilę);
   - kopiuje nowy `om.exe` do `bin\om.new.exe` i podmienia nim `om.exe` jednym `rename` z zastąpieniem (atomowe w obrębie folderu), więc zanik zasilania zostawia albo starą, albo nową wersję, nigdy brak pliku (recenzja R1);
   - kopiuje nowe moduły do folderu modułów i zapisuje `<katalog aktualizacji>\installed.json` (wersja wydania, nazwy i SHA-256 zainstalowanych modułów);
   - uruchamia zadanie `open_mess`.
3. Jeśli nowy demon nie odpowie na lokalne API w 60 s, `apply-update` zatrzymuje go (procesy `om.exe daemon` z folderu `bin`), przywraca `om.old.exe` jako `om.exe`, poprzednie pliki modułów i kopię `state.sqlite3` (cofa migracje schematu i wszystko, co zapisał nieudany demon, w tym automatyczne zatwierdzenia modułów z punktu 4 - recenzja R1), i uruchamia `open_mess` ponownie. Wynik (zainstalowano / przywrócono, z przyczyną) trafia do `<folder danych>\last-update.txt` i do okna.
4. Moduły z podpisanego wydania (decyzja projektowa, zmiana wobec spec 8.3): demon przy skanie zatwierdza automatycznie plik modułu, gdy (a) jego SHA-256 jest w `installed.json` ostatniej zainstalowanej wersji, a ten plik powstał z manifestu, którego podpis był zweryfikowany, oraz (b) manifest modułu z pominięciem pola `version` (nazwa, platforma, hosty, hosty logowania, tożsamość WAM, dodatkowe narzędzia) jest identyczny z zatwierdzonym. Zmiana czegokolwiek poza `version` wymaga zgody w oknie. Plik dodany ręcznie (nie ma go w `installed.json`) zawsze wymaga zgody (recenzja R1).
5. Po udanej instalacji demon usuwa katalogi aktualizacji starszych wersji i `om.update.exe`; zostaje tylko katalog bieżącej wersji do czasu następnej (recenzja R1, P3).

## 8. Threat model

- Zaufany: klucz prywatny wydań (tylko u właściciela, poza repo), klucz publiczny wkompilowany w `om`, użytkownik klikający w oknie / powiadomieniu.
- Niezaufane: GitHub jako transport i hosting (konto, repozytorium albo CDN mogą zostać przejęte), zawartość wydań, nazwy i rozmiary plików, sieć.
- Obrona: obowiązkowy podpis manifestu, sumy SHA-256 każdego pliku, biała lista nazw, limity rozmiaru, brak downgrade, weryfikacja przed uruchomieniem i drugi raz w `apply-update`, jednorazowa zgoda w SQLite zapisywana tylko przez okno / powiadomienie, wycofanie przy nieudanym starcie razem z kopią bazy.
- Poza zakresem (zamrożone): kradzież klucza prywatnego właściciela; złośliwy kod działający już na koncie Windows użytkownika (spec 15).
- Publiczne repo: każdy widzi wydania i klucz publiczny; bez klucza prywatnego nie podpisze wydania.

## 9. Dotknięte pliki

- `crates/open-mess/src/update/` (nowy): sprawdzanie, pobieranie, weryfikacja (minisign), stan i zgoda, uruchamianie `open_mess-update`;
- `crates/open-mess/src/cli.rs`: polecenie `apply-update` (ukryte w pomocy);
- `crates/open-mess/src/store.rs`: migracja z tabelą zgód i stanu aktualizacji;
- `crates/open-mess/src/modules.rs`: automatyczne zatwierdzanie z punktu 7.4;
- `crates/open-mess/src/ui/` i `ui/app.js`: sekcja Aktualizacje, opcja automatycznej instalacji; `approval.rs`: powiadomienie o gotowej wersji;
- `xtask/src/main.rs`: `release`, `release-keygen`; `keys/release.pub` (nowy);
- `scripts/install.ps1`: drugie zadanie `open_mess-update`;
- `CONTRIBUTING.md`: proces wydania.

## 10. Poza zakresem

Aktualizacje różnicowe, kanał beta, stopniowe wdrażanie, Linux/macOS, rotacja klucza wydań (do osobnego specu, gdy będzie potrzebna), aktualizacje bez zadania Harmonogramu.

## 11. Kryteria akceptacji

- Wydanie z dobrym podpisem i wyższą wersją: pobrane, zweryfikowane, po kliknięciu "Zainstaluj" demon wraca w nowej wersji, konto online bez ponownej zgody na moduł Teams przebudowany w tym wydaniu z manifestem różnym tylko w `version`.
- Zły podpis, brak podpisu, zła suma, niedozwolona nazwa pliku, za duży plik, niższa albo równa wersja: nic nie jest instalowane, okno pokazuje przyczynę.
- Nowy demon nie startuje albo nie odpowiada (także po migracji schematu): przywrócona poprzednia wersja, moduły i baza, poprzedni demon działa, okno pokazuje "przywrócono".
- `om.update.exe apply-update` uruchomione bez zgody z okna nie zmienia niczego.
- Żadne narzędzie MCP/CLI nie instaluje aktualizacji.

## 12. Rejestr rebutali

Brak (przed pierwszą rundą).

## Changelog rund

- R1 (Codex REJECT, GLM REJECT): kopia `state.sqlite3` i przywracanie jej przy wycofaniu (oba P1); jednorazowa zgoda w SQLite sprawdzana przez `apply-update` (Codex P2); automatyczna instalacja tylko bez akcji w toku i uruchomień wyzwalaczy (Codex P2); osobne zadanie `open_mess-update` zamiast procesu potomnego demona (GLM P1); zatrzymanie wiszącego nowego demona przed przywracaniem (GLM P2); atomowa podmiana `om.exe` (Codex P2); `--data-dir` dla `apply-update` (Codex P2); dokładna reguła automatycznego zatwierdzania modułów z `installed.json` i manifestem bez `version` (Codex P2, GLM P2); sprzątanie katalogów aktualizacji i lista dotkniętych plików (GLM P3).
