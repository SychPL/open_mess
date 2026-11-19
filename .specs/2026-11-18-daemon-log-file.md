# open_mess - plik logu demona

Data: 2026-11-18
Status: po R10 (limit rund z AGENTS.md): GLM APPROVE od R4 do R10, Codex REJECT w R10 (2x P2 + P3: dwa wdrożone, jeden w rejestrze rebutali); czeka na decyzję właściciela
Powiązane: `.specs/2026-10-26-open-mess-design.md` (8.4 "Logi nie zawierają tokenów, ciasteczek ani treści wiadomości"), `.specs/2026-10-29-live-captions.md` (krok 5: skąd `OPEN_MESS_LOG`), `.specs/2026-11-04-captions-archive.md` (zdarzenia `open_mess::captions`).

## 1. Cel

Demon działa cały dzień z Harmonogramu zadań i nikt nie czyta jego stderr. 2026-11-18 około 10:40 konto Teams właściciela przeszło w `offline` i nie dało się ustalić dlaczego: demon nie zostawia żadnego śladu. Ten spec daje demonowi domyślny plik logu z ostrzeżeniami, błędami i przydatnymi zdarzeniami `info` własnych celów open_mess, z limitem rozmiaru i rotacją, tak żeby następną awarię dało się zdiagnozować z pliku.

Plik nigdy nie zawiera sekretów ani treści (8.4): reguła dla miejsc wywołań (4.6) plus oczyszczanie przy zapisie (4.5) jako druga linia obrony.

Poza zakresem: nowe zdarzenia w `accounts.rs` (przywracanie kont - równoległa sesja `account-restore-retry`) i w `recording/captions.rs` (fałszywe `failed` - równoległa sesja `captions-false-failed`); ich wywołania `tracing` trafią do tego pliku same. Poza zakresem są też pozostałe komentarze `ponytail: ... reported once a log sink exists` poza `daemon.rs` (4.8) - osobna zmiana. Przeglądarka logu w oknie, wysyłanie logu gdziekolwiek, log poza Windows (D1).

## 2. Kontekst w kodzie (sprawdzony)

- `crates/open-mess/src/daemon.rs:45-56`: `tracing_subscriber` jest instalowany tylko przy ustawionym `OPEN_MESS_LOG` (poziom `error`...`trace`), z jedną warstwą `fmt` na stderr i globalnym filtrem `log_targets` (`daemon.rs:187`): `Targets::new().with_target("open_mess", level)`. `Targets` dopasowuje cel po prefiksie tekstu, więc obejmuje `open_mess` i `open_mess_host`; zależności są wyłączone, bo np. rmcp na `debug` wypisuje całe wyniki narzędzi (komentarz przy `log_targets`, recenzja Codexa 0cff6c1).
- Inicjalizacja jest dziś przed blokadą instancji (`acquire_lock`, `daemon.rs:86`): drugi `om daemon` też instaluje subskrybenta, zanim odbije się od blokady. Blokada jest w folderze danych (`instance.rs`), więc dwa demony z różnymi `OPEN_MESS_DATA_DIR` działają obok siebie (`docs/protocols/teams.md:1851`) i dzielą `%LOCALAPPDATA%\open_mess`.
- `Paths::local` (`update/state.rs:21`, `Paths::current` w `:26`) to `%LOCALAPPDATA%\open_mess` - programy, aktualizacje, nagrania, modele; nie wędruje z profilem. `daemon::update()` daje `Some((Paths, _))` tylko na Windows; poza Windows (serwer obok Paperclipa, spec `2026-11-03-paperclip-plugin`) jest `None` i nie ma tam `Paths::local`.
- `start()` (`daemon.rs:89`) zawodzi z przyczyn maszyny i konfiguracji: baza stanu (rusqlite), Menedżer poświadczeń, port zajęty, `OPEN_MESS_LISTEN`, środowisko uruchomieniowe; moduł, który się nie ładuje, nie zatrzymuje startu (`modules.rs`: trafia na listę nieprawidłowych). Przyczyna idzie dziś do `last-start-error.txt`, a następny udany start ten plik usuwa (`tests/daemon.rs:81`).
- `tracing-subscriber` 0.3.23 jest już w `Cargo.lock` z cechami `fmt` i `std` (`fmt` włącza `registry`, więc są `Layer`, `Filtered`, `with_filter`). Warstwa `fmt` przy błędzie zapisu woła `eprintln!` (`fmt_layer.rs`, `log_internal_errors`, domyślnie włączone), a `eprintln!` panikuje, gdy stderr nie przyjmuje zapisu. `chrono` 0.4 z `clock` jest bezpośrednią zależnością `open-mess` (`chrono::Local` już jest w `daemon.rs`).
- Host przekazuje logi modułów (`host.log`, `crates/open-mess-host/src/bindings.rs:144`) do `tracing` z celem `open_mess_host::bindings`, polem `module` i tekstem modułu przyciętym do `MAX_LOG_MESSAGE` = 2048 bajtów (`limits.rs:71`). Poziomy 0-4 to error...trace.
- `crates/open-mess/tests/daemon.rs` uruchamia prawdziwy `om daemon` z `OPEN_MESS_DATA_DIR` w katalogu tymczasowym, ale bez `LOCALAPPDATA`, więc demon testowy liczy `Paths::local` z prawdziwego `%LOCALAPPDATA%` właściciela; `tests/update_probation.rs:266` ustawia `LOCALAPPDATA` na katalog tymczasowy.
- Komentarze `ponytail` o raportowaniu "once a log sink exists" (i podobne o braku logu) są m.in. w `daemon.rs:265` (wątek kolejki transkrypcji), `daemon.rs:329` (nieudane przywrócenie kont przy starcie), `daemon.rs:433` (wątki rejestratora), a poza `daemon.rs` w `accounts.rs`, `recording/captions.rs`, `approval.rs`, `pending.rs`, `recording/queue.rs`, `recording/recorder.rs`, `triggers/spawn.rs`, `ui/window.rs`; lista nie jest pełna (GLM R2) - osobna zmiana przejdzie je wszystkie wyszukiwaniem.

### 2.1 Przegląd istniejących wywołań `tracing` w `crates/` (co wypisują)

| Miejsce | Poziom, cel | Co niesie | Ocena |
|---|---|---|---|
| `recording/captions.rs:708, 714, 748, 800, 1013` | info, `open_mess::captions` | stały tekst; `outcome`/`reason` to `Debug` enumów (`Outcome`, `Reason` - same nazwy wariantów), `cues` to liczba | czyste |
| `recording/captions.rs:1141` | warn, `open_mess::captions` | stały tekst | czyste |
| `open-mess-host/src/actor.rs:369` | warn | `module` (nazwa z manifestu), `crashes` (liczba), `{error}` = `HostError` z `run_on_instance`: tylko `Load`, `Timeout` albo `Crashed(root_cause)` pułapki wasmtime (kod pułapki, czasem tekst błędu importu hosta); `Module(..)` tu nie trafia (błąd modułu jest wewnątrz wyniku eksportu) | czyste; `Load` może nieść ścieżkę pliku modułu (profil - 4.5) |
| `open-mess-host/src/bindings.rs:101, 116` | warn | `Debug` `StorageError`; `rusqlite` dołącza do błędu otwarcia pełną ścieżkę `teams.sqlite3` (Codex R1) | ścieżka profilu z nazwą użytkownika - 4.5 zamienia ją na `%USERPROFILE%` |
| `open-mess-host/src/bindings.rs:148-152` | error...trace | tekst modułu (`host.log`), do 2048 B | niezaufany tekst modułu - 4.5 i 4.6 |
| `open-mess-host/src/bindings.rs:160, 166, 179` | warn | stały tekst i limity | czyste |
| `open-mess-host/src/net.rs:262` | debug | metoda, host, status, bajty | nie trafia do pliku (debug) |
| `open-mess-host/src/sign_in.rs:115, 422, 430` | debug/warn | stały tekst i limity | czyste |

Logi modułu Teams (`modules/teams/src`, `host::log`): stałe teksty, statusy HTTP, kody końca Teams (`shown(code)`), nazwy powodów (`reason.name()`), liczby, nazwy nieznanych linii SDP i tekst błędu sieci hosta (`describe`, `net.rs:67` odcina z błędu reqwest adres przez `without_url`). Dwie luki (Codex R1), naprawiane tutaj (4.8):

- `sdp.rs:28` `name()` bierze z nieznanej linii wszystko do pierwszego `:` lub spacji, więc linia `p=+12025550123` (telefon) albo `o=<użytkownik> ...` trafia do błędu `incompatible`, a `live.rs:877` go loguje;
- `rtc.rs:482-485` (`accept`) zamienia błąd parsera SDP stosu WebRTC (`rtc-sdp`) i `set_remote_description` na tekst przez `network(e)`, a te błędy cytują linie odpowiedzi (nazwa użytkownika z `o=`, poświadczenia ICE); `live.rs:888` loguje go przez `describe`. Pozostałe `network(e)` w `rtc.rs:448-453` dotyczą lokalnej oferty, bez danych serwera.

## 3. Decyzje

- **D1 - gdzie**: `<Paths::local>\logs\open_mess.log`, czyli `%LOCALAPPDATA%\open_mess\logs\open_mess.log`, i jeden plik poprzedni `open_mess.old.log` (oba otwierają się w Notatniku dwuklikiem). Tylko na Windows. Poza Windows bez pliku: serwer działa w kontenerze, którego środowisko i tak przechowuje i rotuje stderr; diagnostyka tam to dalej `OPEN_MESS_LOG`.
- **D2 - co**: tylko własne cele open_mess (prefiks `open_mess`, czyli też `open_mess_host` i logi modułów przez `host.log`) od poziomu `info` w górę. Zależności (rmcp, wasmtime, reqwest, hyper, rustls, tauri) nigdy, na żadnym poziomie: ich tekstów nikt tu nie przejrzał pod kątem 8.4. Poziom pliku jest stały, nie do ustawienia.
- **D3 - `OPEN_MESS_LOG`** działa jak dziś (stderr, poziom ze zmiennej, te same cele) i nie zmienia pliku. Obie warstwy mają własne filtry (`with_filter`), więc `OPEN_MESS_LOG=trace` nie wpuszcza `debug` do pliku, a jego brak nie wyłącza pliku.
- **D4 - rozmiar**: najwyżej 4 MiB na plik, dwa pliki, więc do 8 MiB plus jeden wiersz. Szacunek: zdarzenia rozmowy z napisami to kilkanaście wierszy po ~150 B, a ostrzeżenie modułu przy nieudanym odpytywaniu przychodzi raz na cykl odpytywania; 4 MiB to wiele dni.
- **D5 - bez nowej zależności**: własna warstwa `tracing_subscriber::Layer` z własnym zapisem do pliku zamiast `tracing-appender`. `tracing-appender` rotuje tylko według czasu (nie ma limitu rozmiaru, a limit jest wymaganiem), to nowa paczka do `cargo deny`, a w trybie nieblokującym własny wątek. Własna warstwa nie używa też formatowania `fmt`, więc nie ma ścieżki `eprintln!` przy błędzie zapisu.

## 4. Działanie

### 4.1 Start

- Nowy moduł `crates/open-mess/src/logfile.rs`: `LogFile` (folder i limit; plik otwierany na każdy wiersz, 4.3), `FileLayer` (`Mutex<LogFile>` i folder profilu), implementujący `Layer<S>` z samym `on_event`, `targets(level)` (przeniesione z `daemon.rs` `log_targets`, używane przez obie warstwy) i `layer(dir, max, home)` - warstwa z filtrem `info`, jak ją instaluje demon i testy.
- `daemon::run`: instalacja subskrybenta to pierwszy krok `run`, przed folderem danych (`paths::data_dir`), decyzją o aktualizacji i blokadą - każde z nich może zawieść (folder Roaming przekierowany na niedostępny udział, brak prawa zapisu do `daemon.lock` - Codex R3, R4). Folder logu to `Paths::local_dir()` (`%LOCALAPPDATA%\open_mess`; wydzielone z `Paths::current`, które dotąd liczyło go razem z folderem danych) plus `logs`, tylko na Windows. Warstwy: stderr jak dziś (przy `OPEN_MESS_LOG`, z `log_internal_errors(false)`, żeby błąd stderr nie panikował) oraz plik; jako `Option<Layer>` (w `tracing-subscriber` `Option<L>` jest warstwą), każda z własnym `Targets`. Drugi demon z tym samym folderem danych (`AlreadyRunning`) nie loguje nic, a plik jest otwierany dopiero przy wierszu (4.3), więc go nie dotyka ani nie rotuje; ścieżki aktualizacji (`Start::Exit`, `LaunchHelperAndExit`) też nie logują.
- Zawiódł folder danych: `error`, cel `open_mess::daemon`, "the daemon did not start: <rodzaj>" (bez folderu danych nie ma `last-start-error.txt`). Zawiodło `update()` (brak `LOCALAPPDATA`), blokada z innego powodu niż `AlreadyRunning` albo `start()`: to samo zdarzenie, gdy jest gdzie je zapisać, a pełny tekst idzie do `last-start-error.txt` (dla `update()` i blokady to nowość: dotąd trafiały tylko na stderr, a CLI pokazywało dawną przyczynę z pliku - Codex R5, P3). Rodzaj to nazwa wariantu `Error` (pierwsze litery i cyfry jego `Debug`), dla `Io` z `ErrorKind`, dla `PortInUse` z portem - nigdy tekst błędu: ten cytuje plik sekretów (`serde_json` przy uszkodzonym `secrets.json`) albo wartość ustawienia (Codex R3). Udany start: `info` "the daemon started" z polem `version` (`CARGO_PKG_VERSION`). Na końcu `run` (po zamknięciu sesji modułów, przed zwrotem) `info` "the daemon stopped". W pliku widać każdy restart, także ten z Harmonogramu, nieudany start z rodzajem, który przetrwa usunięcie `last-start-error.txt` przez następny udany start, i koniec bez własnego zamknięcia (start bez wcześniejszego stop): awarię, ale też zabicie procesu przez `scripts/install.ps1` albo Menedżer zadań i utratę zasilania - log tego nie rozróżnia (Codex R6, P3).
- Po skanie modułów (`Modules::scan` w `start()`) każdy moduł, który nie działa: `warn`, "a module does not run: <stan>" z polem `module` (nazwa z manifestu) - stan to tekst `ModuleState` ("awaiting approval (file changed)", "disabled", "stopped after repeated crashes"). Plik `Invalid` daje tylko "a module file is not a valid module", bez nazwy i powodu: jego wpis nosi nazwę pliku, którą nadał użytkownik (`Teams - Jan Kowalski.wasm`), a powód to tekst ładowarki (Codex R10). `PendingReplacement` nie daje wiersza: moduł o tej nazwie to inny plik, który ma własny wiersz, jeśli nie działa (Codex R10, P3). Konta takiego modułu zostają `offline`; najczęstsza przyczyna po instalacji to plik modułu czekający na ponowną zgodę (Codex R9).
- Zasobnik i okno nie wystartowały (`ui::run` zwraca błąd, API działa dalej): `warn`, "the tray and the window did not start: <rodzaj>" - obok dotychczasowego tekstu na stderr (Codex R6).
- Wyjście przy aktualizacji w toku (`Start::Exit`, `LaunchHelperAndExit`) nie loguje nic, jak dziś; jego powód zapisuje `last-start-error.txt`.

### 4.2 Format wiersza

Jedno zdarzenie to jeden wiersz UTF-8 zakończony `\n`:

```
2026-11-18 10:40:12.345 +01:00 [4812] WARN  open_mess_host::bindings: polling an account for new messages failed module=teams
```

Czas lokalny z przesunięciem (`chrono::Local`, `%Y-%m-%d %H:%M:%S%.3f %:z`; właściciel podaje godziny lokalne; gdy Windows nie poda strefy, chrono 0.4.45 robi `unwrap()` w `offset/local/mod.rs:193`, więc `Local::now` idzie przez `catch_unwind`, a po panice ten i każdy następny wiersz ma czas UTC z `+00:00`, bez ponownej próby, więc komunikat paniki trafia na stderr raz - Codex R3, recenzja efacd4e), identyfikator procesu w nawiasach (odróżnia dwa demony z osobnymi folderami danych, które dzielą plik, i kolejne uruchomienia), poziom wyrównany do 5 znaków, cel, komunikat, potem pola ` nazwa=wartość` w kolejności zdarzenia (`&str` bez cudzysłowów, reszta przez `Debug`). Spanów nie ma w kodzie, więc nie są wypisywane.

### 4.3 Rotacja i limit

- Gdy ostatni bajt pliku nie jest `\n` (wiersz urwany przez utratę zasilania w trakcie zapisu), nowy wiersz zaczyna się od `\n`, więc nie dokleja się do urwanego (Codex R9, P3); stąd otwarcie także do odczytu.
- Plik jest otwierany po nazwie (`create` + `read` + `append`) dla każdego wiersza i zamykany po zapisie; żaden uchwyt nie zostaje otwarty między zdarzeniami. Uchwyt trzymany dłużej pisałby dalej do pliku, który drugi demon z osobnym folderem danych (wspólny `%LOCALAPPDATA%`) już przemianował, i rotowałby potem cudzy plik, wypychając pełną historię (Codex R2). Koszt to otwarcie pliku na zdarzenie; zdarzenia pliku są rzadkie (D2).
- Rozmiar to długość pliku z metadanych świeżego uchwytu (nie licznik własnych zapisów). Metadane niedostępne - rozmiar traktowany jak przekroczony.
- Gdy rozmiar plus wiersz przekroczyłby limit (a plik nie jest pusty), uchwyt jest zamykany, `open_mess.log` przemianowany na `open_mess.old.log` (zastępując go) i otwierany nowy. Gdy przemianowanie się nie uda (np. inny program trzyma `open_mess.old.log` bez `FILE_SHARE_DELETE`), bieżący plik jest obcinany do zera (`File::create`) i otwierany jak zawsze z `append`: limit wygrywa z historią, a zapis od pozycji zero nigdy nie nadpisuje wiersza, który drugi demon dopisał w międzyczasie (Codex R3, P3). Po rotacji rozmiar jest sprawdzany jeszcze raz na nowym uchwycie: plik, którego nie dało się ani przemianować, ani obciąć (np. uprawnienia NTFS pozwalające tylko dopisywać), nie przyjmuje już wierszy - wiersz przepada zamiast przekroczyć limit (Codex R4). Tej ścieżki nie da się przenośnie odtworzyć w teście (dopisywanie bez prawa obcięcia wymaga ACL), więc test jej nie ma; warunek to ta sama funkcja `full` co przed rotacją.
- W procesie zapis i rotacja idą pod jednym `Mutex`. Dwa procesy, które w tej samej chwili uznają plik za pełny, mogą zrotować dwa razy (drugi wypycha świeży plik pierwszego); okno to kilka wywołań systemowych, a dwa demony na jednym profilu to układ testowy - akceptowane.
- Wiersz ma najwyżej ~4,2 KiB (4.5), więc żaden plik nie przekracza limitu o więcej niż jeden wiersz.

### 4.4 Błędy zapisu

- Nie da się utworzyć folderu, otworzyć pliku albo zapisać wiersza: zdarzenie przepada, a następne próbuje od nowa (`create_dir_all` + otwarcie, jak każde zdarzenie - 4.3).
- Warstwa nigdy nie panikuje, nie zwraca błędu, nie pisze na stdout ani stderr. Zatruty `Mutex` jest przejmowany (`PoisonError::into_inner`), nie przerywa zapisu. Formatowanie przez `write!` do `String` (wynik ignorowany), nie przez `to_string()`, które panikuje na błędzie `Display`.
- Demon startuje i działa tak samo bez pliku: żadna ścieżka startu nie zależy od logu.

### 4.5 Oczyszczanie przy zapisie (druga linia obrony)

Zdarzenie z pustym polem `module` nie jest zapisywane wcale: host nadaje to pole każdemu swojemu zdarzeniu o module (`bindings.rs`, `sign_in.rs`, `actor.rs`), a puste jest tylko w wywołaniu `manifest` modułu, który nie ma jeszcze nazwy - także takiego, który czeka na zatwierdzenie. Bez tej reguły ostrzeżenia hosta z tego wywołania (odmowa magazynu w pętli, pułapka `manifest`) mogłyby zalać plik i wypchnąć rotacją historię działających kont (Codex R7); źródłowa blokada `host.log` (4.8) nie obejmuje ostrzeżeń samego hosta. Jedno miejsce w sinku obejmuje też przyszłe ostrzeżenia hosta.

Reszta dotyczy komunikatu i wartości pól (czas, proces, poziom, cel i nazwy pól są stałe z kodu), w tej kolejności:

1. **Foldery profilu**: każde wystąpienie wartości `USERPROFILE`, `APPDATA` i `LOCALAPPDATA` zamieniane na `%USERPROFILE%`, `%APPDATA%`, `%LOCALAPPDATA%`, najdłuższa wartość pierwsza (`APPDATA` leży zwykle w profilu, a przekierowany polityką domeny `APPDATA` na udziale `\\serwer\profile\<imię>\...` - poza nim, GLM R9). Każda z wartości (np. `C:\Users\Łukasz Kowalski`; bez względu na wielkość liter, także poza ASCII - `Ł` i `ł` jak w NTFS, porównanie znak po znaku przez `to_lowercase`, Codex R3) jest zamieniana - dla udziału także w postaci dosłownej ścieżki `\\?\UNC\serwer\...` (`C:\...` zawiera się w `\\?\C:\...`; recenzja efacd4e) - w trzech postaciach: tak, jak jest, z ukośnikami `/` i dokładnie tak, jak zapisuje ją `Debug` napisu (`format!("{folder:?}")` bez cudzysłowów: podwojone `\\`, `\u{a0}` dla twardej spacji i każda inna ucieczka; `bindings.rs:101` loguje `{e:?}` z `StorageError::Failed(String)` - Codex R2, R8). Nazwa folderu profilu to często imię i nazwisko, a ścieżki pod nim niosą błędy `rusqlite` (`teams.sqlite3`), ładowania modułu i plików. Wartość krótsza niż 4 znaki jest pomijana.
2. **Jeden wiersz**: znaki sterujące (kategoria Cc, w tym `\r`, `\n`, `\t`, ESC, NEL), znaki kierunku i zerowej szerokości (U+200B-U+200F, U+202A-U+202E, U+2066-U+2069, U+FEFF) oraz U+2028/U+2029 zamieniane na spację. Tekst modułu ani serwera nie podrobi wiersza logu ani sekwencji terminala.
3. **Zaciemnianie słów**: "słowo" to maksymalny ciąg liter, cyfr i znaków `-_.:@/+%~?&#` (bez `=`, więc w `nazwa=wartość` zostaje nazwa pola). Słowo jest zamieniane na `<redacted>`, gdy spełnia którykolwiek warunek: (a) zawiera `@`; (b) ma co najmniej 9 cyfr ASCII; (c) ma co najmniej 24 znaki, w tym jakąkolwiek cyfrę; (d) ma co najmniej 32 znaki; (e) ma kształt MRI Teams: 1-3 cyfry, `:` i litera (`8:live.com:jkowalski`, `28:bot` - GLM R7; godzina `10:40` ma po dwukropku cyfrę i zostaje). To łapie kształty sekretów i identyfikatorów z ruchu Teams: GUID-y (36 znaków: tenant, użytkownik, obiekt - także bez cyfr, przez (d)), JWT (Bearer, skypetoken), tokeny odświeżania, MRI i wątki (`19:...@thread.v2`, `8:orgid:<guid>`), adresy e-mail, numery telefonów, znaczniki czasu w milisekundach, adresy URL ze ścieżką lub zapytaniem, długie adresy IPv6. Zostają kody (także sub-kody Teams do 8 cyfr), liczby, statusy HTTP, nazwy enumów, krótkie słowa, polskie litery.
4. **Długość**: komunikat z polami najwyżej 4096 bajtów (na granicy znaku); dłuższy kończy się `...`.

To nie zastępuje reguły 4.6: zwykłego tekstu (treść wiadomości, napisy, imiona) nie da się rozpoznać po kształcie.

### 4.6 Reguła treści dla miejsc wywołań

Zdarzenie niesie: stały tekst, stany, nazwy wariantów enumów, kody (HTTP, kody Teams, `ErrorKind`), liczby, rozmiary, czasy trwania, nazwę modułu, wersję. Tekst błędu systemu i konfiguracji (`std::io::Error` z pliku, wątku, gniazda; błędy Windows z kodem; błędy bazy stanu) jest dozwolony - ścieżki pod profilem wyrównuje 4.5.

Nigdy: tokeny, ciasteczka, klucze, hasła, poświadczenia ICE; identyfikatory tenantów, użytkowników, kont, rozmów, wątków, wiadomości, połączeń; treść wiadomości i napisów; imiona, nazwy wyświetlane, adresy e-mail, numery telefonów; adresy URL ze ścieżką lub zapytaniem; treść odpowiedzi serwera i jej fragmenty (linie SDP, pola JSON). Nigdy też `Display`/`Debug` błędu, który może nieść obcy tekst: `HostError::Module`, `ModuleError` (tekst modułu, często z serwera), `Error::Account`, `io::Error::other(...)` z takim tekstem, błędy reqwest z adresem, błędy parserów cytujące wejście, w tym błąd startu demona (`Error::SecretsFile` cytuje `secrets.json`, `BadConfig` wartość ustawienia). Zamiast nich - rodzaj albo wynik (jak `join_outcome` w `captions.rs`) albo stały tekst kroku.

Logi modułów (`host.log`) podlegają tej samej regule po stronie modułu, łącznie z tekstem błędów hosta, które moduł loguje (dlatego 4.8 poprawia `rtc.rs`); host przepuszcza je przez 4.5. Regułę dopisuję do `AGENTS.md` (sekcja "Non-negotiables", obok punktu o sekretach), żeby obowiązywała każdą następną zmianę.

### 4.7 Testy nie dotykają folderów właściciela

`tests/daemon.rs` ustawia `LOCALAPPDATA` na katalog tymczasowy testu (jak `update_probation.rs`) i usuwa `OPEN_MESS_LOG`. Inne testy nie uruchamiają `om daemon` (`server.rs` jest tylko dla Unix i nie ma tam pliku).

### 4.8 Zmiany w miejscach wywołań

- `daemon.rs`: start, nieudany start i stop demona (4.1);
- `daemon.rs:265`: kolejka transkrypcji, której wątek nie wystartował - `warn`, cel `open_mess::daemon`, "the transcription queue did not start: <io::Error>"; komentarz `ponytail` usunięty;
- `daemon.rs:433`: powiadomienia albo wątki rejestratora, które nie wystartowały - `warn`, "the recorder did not start: <io::Error>"; komentarz `ponytail` usunięty;
- `modules/teams/src/sdp.rs` `name()`: tylko typ linii (`p=`, `o=`) i nazwa atrybutu linii `a=` - same litery, cyfry i `-` od początku, najwyżej 32 znaki (`a=ice-ufrag=AbCd` to `a=ice-ufrag`, Codex R3); linia bez typu to `(not a line)`. Nigdy wartość;
- `crates/open-mess-host/src/bindings.rs` `log`: wywołanie `manifest` modułu, który nie ma jeszcze nazwy (`runtime.rs`: "its imports reach nothing"), nie loguje nic - jak jego magazyn i zdarzenia. Skan uruchamia `manifest` każdego pliku `.wasm` w folderze modułów przed sprawdzeniem zgody, więc bez tego moduł czekający na zatwierdzenie mógłby zalać plik i wypchnąć rotacją historię działających kont (Codex R6). `modules/test-echo` loguje w `manifest` jeden wiersz, a test w `tests/events.rs` sprawdza, że nie dociera;
- `crates/open-mess-host/src/rtc.rs` `accept`: błąd parsera odpowiedzi to stały tekst "the answer is not valid SDP", a odrzucenie przez `set_remote_description` - "the answer was refused"; tekst stosu WebRTC, który cytuje linie odpowiedzi, nie wychodzi z hosta.

`daemon.rs:329` (nieudane przywrócenie kont) zostaje sesji `account-restore-retry`, która zmienia tę ścieżkę; notatka w folderze notatek grupy.

## 5. Kroki

1. `logfile.rs`: `LogFile` (otwarcie, rotacja według rozmiaru pliku, błędy), `clean` i zamiana profilu (4.5), `FileLayer` (`on_event`, wizytor pól, format 4.2), `targets`, `layer`; testy jednostkowe.
2. `daemon.rs`: instalacja na początku `run` z dwiema warstwami (`update/state.rs`: `Paths::local_dir`), zdarzenia 4.1, dwa `warn` z 4.8; `log_internal_errors(false)` na stderr; test `log_targets` przeniesiony na `logfile::targets`.
3. `sdp.rs` `name()` i `rtc.rs` `accept` (4.8) z testami.
4. `tests/daemon.rs`: `LOCALAPPDATA` w katalogu testu i test pliku logu.
5. `AGENTS.md` (reguła 4.6), `CONTRIBUTING.md` (wiersz `logfile.rs` w tabeli).
6. `cargo xtask ci`, commit, recenzje commita.

## 6. Dotknięte pliki

- `crates/open-mess/src/logfile.rs` (nowy), `crates/open-mess/src/lib.rs` (`pub mod logfile`),
- `crates/open-mess/src/daemon.rs`, `crates/open-mess/src/update/state.rs` (`Paths::local_dir`),
- `crates/open-mess/tests/daemon.rs`,
- `crates/open-mess-host/src/rtc.rs`, `crates/open-mess-host/tests/rtc.rs`,
- `crates/open-mess-host/src/bindings.rs`, `crates/open-mess-host/tests/events.rs`, `modules/test-echo/src/lib.rs`,
- `modules/teams/src/sdp.rs`,
- `AGENTS.md`, `CONTRIBUTING.md`.

Bez nowych zależności i cech w `Cargo.toml`.

## 7. Kryteria akceptacji

1. Demon na Windows bez `OPEN_MESS_LOG` tworzy `%LOCALAPPDATA%\open_mess\logs\open_mess.log`; nieudany start (port zajęty) zapisuje "the daemon did not start: PortInUse(N)" (rodzaj, nie tekst błędu, który zostaje w `last-start-error.txt`), a następny udany "the daemon started version=X"; drugi demon z tym samym folderem danych, odrzucony przez blokadę, nie zapisuje nic; plik `bogus.wasm`, który nie jest modułem, daje przy udanym starcie "a module file is not a valid module" (test `tests/daemon.rs`, z `LOCALAPPDATA` w katalogu testu).
2. Do pliku trafiają `info`, `warn` i `error` celów `open_mess*`; `debug` tych celów i zdarzenia innych celów (np. `rmcp`) nie trafiają (test jednostkowy warstwy z subskrybentem lokalnym); filtr `targets` nie przepuszcza `rmcp` ani `hyper` na żadnym poziomie (test w `daemon.rs`).
3. Rotacja: przy limicie N bajtów po wielu wierszach istnieją dokładnie `open_mess.log` i `open_mess.old.log`, każdy nie większy niż N, z najnowszymi wierszami w kolejności (test z małym limitem); plik większy niż limit z poprzedniego uruchomienia rotuje przy pierwszym wierszu.
4. Nieudane przemianowanie zostawia bieżący plik obcięty, nie rosnący (test: `open_mess.old.log` jest folderem). Plik kończący się urwanym wierszem dostaje `\n` przed następnym (test).
5. Dwa `LogFile` piszące na zmianę do jednego folderu dzielą jedną historię: `open_mess.log` i `open_mess.old.log` mają dokładnie ostatnie wiersze obu w kolejności zapisu, żaden ponad limit, i żaden piszący nie wypycha pełnego pliku drugiego (test).
6. Folder logu, którego nie da się utworzyć (ścieżka pod plikiem), nie powoduje paniki ani błędu, także przez warstwę; po usunięciu przeszkody następny wiersz trafia do pliku (test).
7. Oczyszczanie: `\n`, `\r`, `\t`, ESC, U+202E i U+2028 zamienione na spację; GUID (z cyframi i bez), JWT, `19:x@thread.v2`, `8:orgid:<guid>`, adres e-mail, URL ze ścieżką, token odświeżania, IPv6, `+12025550123` i 13-cyfrowy znacznik czasu zamienione na `<redacted>`, a w `token=<jwt>` zostaje `token=`; `HTTP 403`, `code 4097, sub-code 5012`, `Transient(ModuleFailed)`, `module=teams`, `version=0.2.0` i polskie litery bez zmian; komunikat przycięty do 4096 bajtów na granicy znaku (testy `clean`). Folder profilu w dowolnej wielkości liter, z `/` i w postaci `Debug` (`\\`, `\u{a0}`) zapisany jako `%USERPROFILE%`, `APPDATA` przekierowany na udział jako `%APPDATA%`, a nazwa użytkownika z profilu nie występuje w pliku (testy warstwy).
8. Wiersz ma format 4.2: czas lokalny, który parsuje się formatem z 4.2, identyfikator procesu, poziom, cel, komunikat, pola (testy).
9. `sdp.rs`: odpowiedź z liniami `p=+12025550123`, `Jan Kowalski` i `a=ice-ufrag=AbCd` daje błąd "sdp: unexpected lines p= (not a line) a=ice-ufrag" (test modułu).
10. `rtc.rs`: odpowiedź z uszkodzoną linią `o=JanKowalski 1` jest odrzucana jako `network` z tekstem "the answer is not valid SDP", bez cytatu linii (test `tests/rtc.rs`; GLM R5).
11. Wiersz `host.log`, który `test-echo` wysyła w `manifest`, nie dociera do logu (test `tests/events.rs`); zdarzenia z pustym `module` (ostrzeżenie magazynu, pułapka) nie trafiają do pliku, a te same z `module=teams` trafiają (test warstwy). `8:live.com:jkowalski` i `28:bot` zaciemnione, `10:40` i `line 3:` nie (testy `clean`).
12. `cargo xtask ci` zielone; `cargo deny` bez zmian (brak nowych paczek).

## 8. Threat model

- **Zaufane**: kod rdzenia i hosta (stałe teksty, nazwy pól, cele), właściciel czytający plik na swoim komputerze.
- **Niezaufane**: tekst modułu (`host.log`, do 2048 B), tekst pułapki wasmtime i błędów importów hosta, tekst błędów sieci i WebRTC, wartości pól pochodzące z zewnątrz, ścieżki pod profilem. Przechodzą przez 4.5: foldery profilu jako `%USERPROFILE%`, `%APPDATA%`, `%LOCALAPPDATA%`, jeden wiersz, zaciemnianie kształtów sekretów, limit długości.
- **Główne ryzyko**: plik logu jest właśnie tym, co właściciel wkleja do zgłoszenia albo wysyła do diagnozy. Dlatego reguła 4.6 (źródło) i 4.5 (sink) razem; sink nie rozpozna zwykłego tekstu, więc odpowiedzialność za treść zostaje przy miejscach wywołań i przy recenzji. Folder danych ustawiony ręcznie przez `OPEN_MESS_DATA_DIR` poza tymi trzema folderami (inny dysk, folder z imieniem) nie jest wyrównywany - to wybór właściciela, nie polityka domeny.
- **Moduł niezatwierdzony**: w folderze modułów, ale bez zgody właściciela - jego `manifest` nie loguje nic (4.8), ostrzeżenia hosta z tego wywołania nie trafiają do pliku (4.5, puste `module`), a innych wywołań nie dostaje.
- **Złośliwy moduł** (zatwierdzony): może pisać do logu dowolny tekst w granicach 4.5, ale nie poza plik, nie ponad limit rozmiaru i nie podrobi wierszy rdzenia; dostępu do danych właściciela przez log nie zyskuje (ma już własny magazyn i sieć z manifestu). Zalew logów: rotacja trzyma dysk w 8 MiB; utrata starszej historii przy zalewie jest akceptowana.
- **Dwa demony** z osobnymi folderami danych (testy, `docs/protocols/teams.md:1851`): wspólny plik, wiersze rozróżnia identyfikator procesu, limit trzyma 4.3.
- **Dysk**: plik w `%LOCALAPPDATA%` właściciela, bez szyfrowania, z uprawnieniami profilu - jak nagrania i stan. Nigdy w Roaming.
- **stderr (`OPEN_MESS_LOG`)**: włączany ręcznie do diagnozy (na serwerze trafia do logu kontenera) i nieoczyszczany przez 4.5 - chroni go tylko reguła 4.6, jak dziś; dlatego nowe zdarzenia (start, blokada) niosą rodzaj błędu, nie tekst.
- **Dostępność**: błąd pliku nie zatrzymuje demona ani nie powoduje paniki (4.4); warstwa stderr dostaje `log_internal_errors(false)`; czas lokalny przez `catch_unwind` (4.2).
- **Granica twardnienia**: bez szyfrowania pliku, bez podpisów wierszy, bez ochrony przed innym procesem tego samego użytkownika, który zmienia plik.

## 9. Rejestr rebutali

- **Codex R3, P2 - stary moduł Teams z dawnym `name()` loguje `i=Budzet`**: nowy demon nie poprawi tekstu, który stary moduł już złożył, a zwykłego słowa sink nie rozpozna (4.5, ostatni akapit). Moduły jadą z demonem: wydanie niesie `teams.wasm` obok `om.exe` (`xtask` `module_release_name`), `scripts/install.ps1` kopiuje moduły razem z programem, a samoaktualizacja umieszcza moduły nowej wersji (`update/apply.rs`). Stary moduł przy nowym demonie to tylko ręczny układ deweloperski (`cargo run` z dawno zatwierdzonym modułem w folderze danych).
- **Codex R4, P2 - błąd `serde_json` uszkodzonego `secrets.json` cytuje wartość, a `om daemon` wypisuje pełny błąd startu na stderr (`cli.rs:116`) i do `last-start-error.txt`**: to zachowanie sprzed tego specu (`file_store.rs:60` dołącza `{err}` do `Error::SecretsFile`), którego ten spec nie zmienia ani nie poszerza: do pliku logu i do warstwy `OPEN_MESS_LOG` trafia tylko rodzaj (`SecretsFile`, 4.1). Naprawa leży w `file_store.rs` (sam rodzaj i pozycja błędu parsera zamiast jego tekstu) - osobna zmiana, zgłoszona właścicielowi jako następny krok.
- **Codex R5, P2 - nazwa nieznanego atrybutu SDP (`a=contact-Alicja`) może nieść imię**: nazwy atrybutów to słownik protokołu, który ustala usługa Microsoftu, nie dane osób; dane siedzą w wartościach, a tych `name()` nie oddaje nigdy (4.8, po R1 i R3). Nazwy nieznanych linii są potrzebne do dostosowania parsera do nowej odpowiedzi Teams - tak został poprawiony `sdp.rs` po "Live results" 19 (`a=ssrc-group`, spec napisów, krok 5). Serwer, który celowo wkłada imię w nazwę atrybutu, to złośliwa usługa Teams, poza tym modelem zagrożeń (8: ryzyko to przypadkowy wyciek przy wklejaniu logu).
- **Codex R10, P2 - przy `OPEN_MESS_LOG=warn` błąd `rusqlite` ze ścieżką bazy idzie na stderr bez zamiany folderów**: warstwa stderr i to ostrzeżenie (`bindings.rs:101`) są sprzed tego specu i ten spec ich treści nie zmienia; stderr to diagnostyka włączana ręcznie, a jej brak oczyszczania jest zapisany w 8. Plik logu, który działa domyślnie i który właściciel wkleja do zgłoszeń, zamienia te foldery (4.5). Ostrzeżenia hosta z tekstem błędu `rusqlite` można zastąpić rodzajem w osobnej zmianie razem z `file_store.rs` (rebutal R4).
- **Codex R5, P2 - synchroniczny zapis na zawieszonym udziale SMB blokuje demona**: `%LOCALAPPDATA%` jest lokalny z definicji - przekierowanie folderów Windows obejmuje tylko foldery wędrujące, nie `AppData\Local`. W tym samym `%LOCALAPPDATA%\open_mess` demon już synchronicznie pisze nagrania i trzyma `bin`, a folder danych (SQLite) pisze synchronicznie na każdym wywołaniu; zawieszony system plików tam zatrzymuje demona niezależnie od logu. Złącze (junction) `logs` na udział sieciowy to wybór właściciela.

## Changelog rund

- R1: Codex REJECT (5x P2), GLM APPROVE (2x P3); wszystko wdrożone. Nazwy nieznanych linii SDP bez wartości (`p=` z telefonem, `o=` z użytkownikiem) w `sdp.rs` i reguła (b) 9 cyfr w 4.5 (Codex); stałe teksty błędów `accept` w `rtc.rs`, bo parser SDP cytuje linie odpowiedzi (Codex); folder profilu jako `%USERPROFILE%`, bo `rusqlite` dołącza ścieżkę `teams.sqlite3` (Codex); rozmiar z metadanych pliku przed każdym zapisem i identyfikator procesu w wierszu, bo dwa demony z osobnymi folderami danych dzielą plik (Codex); "the daemon started" dopiero po udanym `start()`, a nieudany start z przyczyną, która przetrwa usunięcie `last-start-error.txt` (Codex, GLM); reguła zaciemniania zapisana jako alternatywa warunków z liczbami, próg 32 zamiast 48 (GLM).
- R2: Codex REJECT (2x P2), GLM APPROVE (1x P3); wszystko wdrożone. Folder profilu zamieniany także w postaci `Debug` (`\\`) i z `/`, bo `bindings.rs:101` loguje `{e:?}` (Codex); plik otwierany po nazwie na każdy wiersz zamiast trzymanego uchwytu, bo uchwyt drugiego demona po cudzej rotacji pisał do starego pliku i rotował świeży plik na miejsce pełnej historii (Codex); lista komentarzy `ponytail` oznaczona jako niepełna, z `accounts.rs` i `captions.rs` (GLM).
- R3: Codex REJECT (7x P2, 1x P3); GLM bez werdyktu (wznowiona sesja oddała 17 znaków; runda powtórzona jako R4). Wdrożone: `Local::now` przez `catch_unwind` z UTC zamiast paniki chrono (Codex); błąd startu i blokady w logu jako rodzaj, nie tekst, bo cytuje `secrets.json` albo wartość ustawienia, także na stderr (Codex, dwa znaleziska); nazwa atrybutu SDP tylko z liter, cyfr i `-` (Codex); instalacja logu przed blokadą, a błąd blokady (poza `AlreadyRunning`) w logu i w `last-start-error.txt` (Codex); folder profilu bez względu na wielkość liter poza ASCII (Codex); obcięcie przy nieudanym przemianowaniu, potem `append` (Codex, P3). Odrzucone: stary moduł z dawnym `name()` - rejestr rebutali.
- R4: Codex REJECT (3x P2), GLM APPROVE (2x P3). Wdrożone: log instalowany jako pierwszy krok `run`, przed folderem danych, z `Paths::local_dir` (Codex); po rotacji ponowne sprawdzenie rozmiaru, plik nie do przemianowania ani obcięcia nie przyjmuje wierszy (Codex); krok 2 i kryterium 1 zgodne z 4.1 (GLM). Odrzucone: tekst `serde_json` z `secrets.json` na stderr i w `last-start-error.txt` - sprzed specu, rejestr rebutali.
- R5: Codex REJECT (2x P2, 1x P3), GLM APPROVE (1x P3). Wdrożone: błąd `update()` też w logu i w `last-start-error.txt` (Codex, P3); test stałego tekstu `accept` w `tests/rtc.rs` i kryterium 10 (GLM). Odrzucone: nazwa atrybutu SDP jako imię i zapis na zawieszonym udziale SMB (Codex) - rejestr rebutali.
- R6: Codex REJECT (2x P2, 1x P3), GLM APPROVE (model zapasowy `glm-5.3-flash`: główny dwa razy bez werdyktu). Wdrożone: `host.log` w `manifest` modułu bez nazwy nie dociera nigdzie, z testem (Codex); `warn` przy nieudanym starcie zasobnika i okna (Codex); start bez stop opisany jako każdy koniec bez zamknięcia, nie tylko awaria (Codex, P3).
- R7: Codex REJECT (1x P2), GLM APPROVE (1x P3); wszystko wdrożone. Zdarzenie z pustym `module` (ostrzeżenia hosta z wywołania `manifest`, także modułu bez zgody) nie trafia do pliku, z testem (Codex); reguła (e) dla krótkich MRI Teams (GLM).
- R8: Codex REJECT (1x P2), GLM APPROVE; wdrożone. Postać `Debug` folderu profilu brana z samego `Debug` (`format!("{home:?}")`), więc obejmuje każdą ucieczkę, także `\u{a0}` twardej spacji, nie tylko `\\`; test z twardą spacją (Codex).
- R9: Codex REJECT (1x P2, 1x P3), GLM APPROVE (1x P3); wszystko wdrożone. `warn` dla każdego modułu, który po skanie nie działa, z jego stanem (`Invalid` tylko jako rodzaj), z testem procesu (Codex); `\n` przed wierszem po urwanym wierszu, z testem (Codex, P3); `APPDATA` i `LOCALAPPDATA` zamieniane jak `USERPROFILE`, bo przekierowany polityką Roaming leży poza profilem (GLM, P3).
- R10: Codex REJECT (2x P2, 1x P3), GLM APPROVE (siódmy raz z rzędu, R4-R10). Wdrożone: plik `Invalid` w logu bez nazwy pliku (Codex); bez wiersza dla `PendingReplacement` (Codex, P3). Odrzucone: tekst `rusqlite` na stderr przy `OPEN_MESS_LOG` - sprzed specu, rejestr rebutali. Limit 10 rund z AGENTS.md wyczerpany: bez R11, decyzja o specu należy do właściciela.
- Recenzja commita efacd4e (Codex REJECT: P2 + 2x P3, GLM APPROVE): udział także w postaci `\\?\UNC\...` (Codex, P2); po panice chrono czas UTC bez ponownej próby, komunikat paniki raz (Codex, P3). Bez zmiany: urwany w środku znak UTF-8 zostaje w pliku przed `\n` (Codex, P3) - Notatnik i edytory czytają taki plik, a naprawa wymagałaby obcinania bajtów, które mógł dopisać drugi demon.
