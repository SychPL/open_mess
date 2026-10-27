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
- Opcja w oknie "Instaluj aktualizacje automatycznie" (domyślnie wyłączona): wtedy instalacja rusza po pobraniu, gdy nie ma akcji oczekujących na zatwierdzenie i żadne wywołanie narzędzia nie trwa.
- Żadne narzędzie MCP ani polecenie CLI nie uruchamia instalacji ani nie zmienia tej opcji (jak zatwierdzenia, spec 8.6). `status` pokazuje tylko stan aktualizacji.

## 7. Instalacja

1. Demon kopiuje zweryfikowany `om.exe` z katalogu aktualizacji do `%LOCALAPPDATA%\open_mess\bin\om.update.exe`, sprawdza jego SHA-256 jeszcze raz i uruchamia go bezpośrednio (bez powłoki): `om.update.exe apply-update --from <katalog> --wait-pid <pid demona>`, po czym kończy pracę.
2. `apply-update` czeka (do 60 s) na zakończenie procesu demona, weryfikuje ponownie podpis manifestu i sumy wszystkich plików w katalogu, przenosi `om.exe` na `om.old.exe` (poprzedni `om.old.exe` usuwa), kopiuje nowy `om.exe`, zachowuje kopie obecnych plików modułów, kopiuje nowe moduły do `%APPDATA%\open_mess\modules\`, uruchamia zadanie `open_mess`.
3. Jeśli nowy demon nie odpowie na lokalne API w 60 s, `apply-update` przywraca `om.old.exe` i poprzednie moduły i uruchamia zadanie ponownie. Wynik (zainstalowano / przywrócono, z przyczyną) trafia do `%APPDATA%\open_mess\last-update.txt` i do okna.
4. Moduły z podpisanego wydania (decyzja projektowa, zmiana wobec spec 8.3): nowy plik modułu, którego manifest (nazwa, hosty, hosty logowania, tożsamość WAM, dodatkowe narzędzia) jest identyczny z zatwierdzonym, jest zatwierdzany automatycznie, bo jego pochodzenie potwierdza podpis wydania; zmieniony manifest nadal wymaga zgody w oknie. Moduł dodany ręcznie do folderu (nie z wydania) zawsze wymaga zgody.

## 8. Threat model

- Zaufany: klucz prywatny wydań (tylko u właściciela, poza repo), klucz publiczny wkompilowany w `om`, użytkownik klikający w oknie / powiadomieniu.
- Niezaufane: GitHub jako transport i hosting (konto, repozytorium albo CDN mogą zostać przejęte), zawartość wydań, nazwy i rozmiary plików, sieć.
- Obrona: obowiązkowy podpis manifestu, sumy SHA-256 każdego pliku, biała lista nazw, limity rozmiaru, brak downgrade, weryfikacja przed uruchomieniem i drugi raz w `apply-update`, wycofanie przy nieudanym starcie.
- Poza zakresem (zamrożone): kradzież klucza prywatnego właściciela; złośliwy kod działający już na koncie Windows użytkownika (spec 15).
- Publiczne repo: każdy widzi wydania i klucz publiczny; bez klucza prywatnego nie podpisze wydania.

## 9. Poza zakresem

Aktualizacje różnicowe, kanał beta, stopniowe wdrażanie, Linux/macOS, rotacja klucza wydań (do osobnego specu, gdy będzie potrzebna), aktualizacje bez zadania Harmonogramu.

## 10. Kryteria akceptacji

- Wydanie z dobrym podpisem i wyższą wersją: pobrane, zweryfikowane, po kliknięciu "Zainstaluj" demon wraca w nowej wersji, konto online bez ponownej zgody na niezmieniony moduł Teams.
- Zły podpis, brak podpisu, zła suma, niedozwolona nazwa pliku, za duży plik, niższa albo równa wersja: nic nie jest instalowane, okno pokazuje przyczynę.
- Nowy demon nie startuje: przywrócona poprzednia wersja, okno pokazuje "przywrócono".
- Żadne narzędzie MCP/CLI nie instaluje aktualizacji.

## 11. Rejestr rebutali

Brak (przed pierwszą rundą).

## Changelog rund

Brak (przed pierwszą rundą).
