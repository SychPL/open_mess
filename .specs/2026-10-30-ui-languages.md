# open_mess - okno, powiadomienia i zasobnik po polsku i po angielsku

Data: 2026-10-30
Status: zatwierdzony (Codex, GLM, runda 4)
Powiązane: `.specs/2026-10-26-open-mess-design.md` (okno konfiguracji), `.specs/2026-10-27-self-update.md` (`last-update.txt`), `crates/open-mess/ui/`, `crates/open-mess/src/ui/`, `crates/open-mess/src/approval.rs`.

## 1. Cel

Wszystko, co open_mess pokazuje człowiekowi, jest w jednym języku, polskim albo angielskim. Dotyczy to okna konfiguracji, powiadomień Windows, okien dialogowych i menu w zasobniku. Dziś te teksty są wymieszane: okno i powiadomienie o aktualizacji są po polsku, powiadomienia o zatwierdzaniu i okna dialogowe po angielsku, a okno nie ma wersji angielskiej.

## 2. Decyzje właściciela (2026-10-30)

- Okno ze wszystkimi tekstami, powiadomienia i menu w zasobniku mają wersję polską i angielską, wybieraną według języka Windows.
- Opisy narzędzi MCP, CLI i logi zostają po angielsku: czytają je agenci i skrypty.

Decyzje projektowe (nie właściciela, do sprawdzenia w review):
- **Wybór ręczny w oknie** ("Automatycznie (język Windows)", "Polski", "English"). Bez niego właściciel na polskim Windows nigdy nie zobaczy wersji angielskiej.
- **Brak plików katalogu i kluczy.** Każdy tekst ma obie wersje obok siebie w kodzie, więc brak tłumaczenia nie skompiluje się w Ruście i jest widoczny w review w JS.

## 3. Kontekst w repo

- **Okno** to osadzony folder `crates/open-mess/ui/` (`index.html`, `app.js`, `app.css`) w Tauri 2 z `custom-protocol`.
  - Wszystkie etykiety i zdania okna są literałami w `app.js` (około 190) i `index.html` (około 17, `<html lang="pl">`). `app.css` nie ma tekstów.
  - Tekst od innych ludzi i modułów trafia na stronę tylko przez `el()` (`textContent`), nigdy przez `innerHTML`.
  - JS mapuje identyfikatory z Rusta na tekst: `ACCOUNT_STATES`, `OUTCOMES`, `UPDATE_PHASES`, `KINDS`, `CONDITIONS`, `WRITE_OPTIONS`.
  - `confirm()` i `alert()` mają tekst z `app.js`, a przyciski z WebView2.
- **Komendy okna** to `crates/open-mess/src/ui/commands.rs`, wszystkie `Result<_, String>`.
  - Błędy idą przez `text(err)`, czyli `Display` typu błędu. JS pokazuje je jako `"Błąd: " + err` (`app.js:52`).
  - Po polsku są tu:
    - stałe `MODULE_CHANGED`, `UPDATE_IN_PROGRESS`, `TARGET_CUT`, `AUTONOMOUS_RULE`;
    - `describe()` (stan modułu);
    - lista zmian manifestu (`list_changes`, "nowy host", "nowe narzędzie", ...);
    - komunikaty "nie ma modułu/konta/reguły ...";
    - tytuł i filtry okna wyboru programu (`pick_program`, rfd).
  - `AUTONOMOUS_RULE` cytuje etykiety okna "samodzielnie", "z potwierdzeniem" i nazwę sekcji "Uprawnienia agentów".
- **Pozostałe polskie teksty, które trafiają do okna:**
  - `crates/open-mess/src/ui/sign_in.rs`: stałe `NOT_FROM_WINDOW`, `BUSY`, `GONE`, `NO_WINDOW`;
  - `sign_in_page.rs`: komunikaty i tytuł okna logowania `"Logowanie - {host}"`;
  - `crates/open-mess/src/accounts.rs`: `OTHER_ACCOUNT` i komunikaty `Error::Account(format!(..))` w `sign_in`, które działa tylko z okna;
  - `crates/open-mess/src/update/service.rs`: "instalacja już trwa", "trwa sprawdzanie aktualizacji, ...", "gotowa jest inna wersja ...", powody w `installable()`.
- **Zasobnik** (`crates/open-mess/src/ui/window.rs:307-312`):
  - pozycje "Otwórz ustawienia" i "Zakończ", menu budowane raz;
  - identyfikatory "open" i "quit" są porównywane, etykiety nie;
  - tytuł okna "open_mess - ustawienia" w `window.rs:350` i w `<title>`.
- **Powiadomienia** (`crates/open-mess/src/approval.rs`):
  - Po angielsku jest to, co buduje `Prompt::new(&Preview)`:
    - tytuł "open_mess: approve {what}";
    - "In reply to", "Before/After", "With ...", "Open it to read all of it before you decide.".
  - Też po angielsku: przyciski "Approve/Reject/Show all", toast "open_mess: {count} actions wait for your approval" (count >= 2) i teksty okien rfd ("Yes approves it, No rejects it").
  - Po polsku: toast aktualizacji "open_mess {version} jest gotowy" z przyciskami "Zainstaluj" i "Później".
  - Nazwy AUMID ("open_mess", "open_mess - aktualizacje") są zapisywane w rejestrze przy każdym `register()`.
- **Wspólne typy błędów** mają `Display` po angielsku i trafiają też do CLI i MCP:
  - `Error` w `crates/open-mess/src/lib.rs`;
  - `UpdateError` w `crates/open-mess/src/update/mod.rs`;
  - `HostError`, `ModuleError` i `HttpError` w `open-mess-host`.
  - Wyjątki po polsku: `Error::UpdateInProgress` ("aktualizacja w toku, spróbuj za chwilę", zwracany z `pending.rs:304`) i `UpdateError::HelperNotStarted` ("pomocnik nie wystartował").
- **`last-update.txt`:**
  - `Display` wyniku `Outcome` (`update/apply.rs:80-84`: "zainstalowano", "przywrócono: ...", "przerwano: ...", "wycofanie nieudane: ...", "wycofanie odłożone: ...", "; kopia zapasowa: ...") jest zapisywany do tego pliku;
  - zapisuje go pomocnik aktualizacji albo nowy demon (`update/probation.rs:123`);
  - okno pokazuje go dosłownie jako "Ostatni wynik", więc język jest zamrożony w chwili zapisu;
  - powody w środku są zwykle po angielsku ("przerwano: the daemon did not stop"); `NO_CONSENT` w `apply.rs:39` ("brak ważnej zgody") jest po polsku.
- **Ustawienia** trzyma tabela `config (key, value)` w `crates/open-mess/src/store.rs` (`get_config`, `set_config`). Wzór ustawienia z okna: `set_auto_install`.
- **Wykrywanie języka:** dziś nie ma żadnego.
  - `windows` 0.62.2 w `crates/open-mess/Cargo.toml` ma tylko `UI_Notifications`.
  - `Windows.System.UserProfile.GlobalizationPreferences::Languages()` (feature `System_UserProfile`, zwraca `IVectorView<HSTRING>`, więc też `Foundation_Collections`) jest bezpiecznym wywołaniem WinRT, jak istniejący kod toastów: bez `unsafe`.
- **Testy porównujące polskie teksty:**
  - `tests/ui.rs` (96, 116, 119, 205, 291, 433);
  - `tests/sign_in.rs` (347, 377, 393, 402, 419);
  - `tests/update_service.rs` (342, 343, 424, 439, 503);
  - `tests/update_probation.rs` (378, 481);
  - `tests/update_apply.rs` (260, 263, 269, 308, 382, 441, 467);
  - `tests/update_quiesce.rs` (396).
  - Angielski tekst porównuje test jednostkowy `src/approval.rs:472`.
  - Nie ma testów JS ani `package.json`.

## 4. Wybór języka

1. **Typ i ustawienie.**
   - Nowy moduł `crates/open-mess/src/i18n.rs`: `pub enum Lang { Pl, En }`.
   - Ustawienie `language` w tabeli `config` przyjmuje wartości `auto` (także brak klucza), `pl` i `en`. Każda inna wartość znaczy `auto`.
2. **Tryb `auto`.**
   - Pierwszy wpis `GlobalizationPreferences::Languages()` decyduje: znacznik BCP 47, którego pierwszy podznacznik to `pl` (bez względu na wielkość liter), daje `Pl`.
   - Każdy inny znacznik, pusta lista albo błąd wywołania daje `En`. Poza Windows (bramka CI na ubuntu) `auto` daje `En`.
   - Logika wyboru to czysta funkcja `pub fn from_tag(first: Option<&str>) -> Lang`, testowana tabelą. Znacznik jest dowolnym napisem z systemu: porównywany jest tylko prefiks do pierwszego `-`, bez indeksowania.
3. **Bieżący język procesu.**
   - `i18n::current() -> Lang` i `i18n::set(Lang)`, na `AtomicU8`, bez blokad. Domyślnie `En`, dopóki demon nie ustali języka.
   - Demon ustala język po otwarciu bazy, przy każdym otwarciu okna (zmiana języka Windows bez restartu) i po zmianie ustawienia.
   - Teksty czytają `current()` w chwili budowy, więc powiadomienie po zmianie ustawienia jest już w nowym języku.
   - Okno przy otwarciu dostaje tytuł w bieżącym języku (`window.rs:350`).
   - **Testy.** Stan jest globalny dla procesu, a testy w jednym pliku biegną równolegle. Dlatego:
     - testy jednostkowe w `src/` nigdy nie wołają `i18n::set`, tylko funkcje czyste z parametrem `Lang`;
     - w jednym pliku testów integracyjnych wszystkie demony mają ten sam język. Pomocnicy testów, którzy budują stan demona (`tests/common/mod.rs` `daemon()` i lokalne, na przykład `unapproved()` w `tests/ui.rs`), zapisują `language` w bazie i wołają `apply_language` (4.5), więc każde ustalenie języka w tym procesie daje ten sam wynik;
     - istniejące pliki, które porównują polskie teksty, używają `pl`;
     - przypadki angielskie są w nowym pliku `crates/open-mess/tests/ui_en.rs` z `en`.
4. **Komendy okna.**
   - `language() -> { setting: "auto"|"pl"|"en", lang: "pl"|"en" }`.
   - `set_language(setting)`:
     - zła wartość daje błąd "nieznany język" / "unknown language";
     - po poprawnej wartości: zapis, ustalenie języka (pkt 5) i zwrot `lang`.
   - `set_language` przechodzi przez tę samą blokadę zapisów w trakcie aktualizacji co inne zapisy z okna (`changes_allowed` w `commands.rs:35-44`, `not_held` w `update/service.rs:278`). W trakcie sprawdzania nowej wersji zwraca więc błąd "aktualizacja w toku" / "update in progress", zamiast zapisać wartość, którą wycofanie by cofnęło.
   - Rejestracja komend tak samo jak `set_auto_install`: handler w `ui/window.rs`, lista w `build.rs` i `capabilities/main.json`. Pilnuje tego test `window_commands_are_registered` (`tests/ui.rs:519`).
   - To ustawienie okna, nie narzędzie: nie zmienia niczego, co widzą agenci (AGENTS.md, "One tool registry"), tak jak automatyczna instalacja.
5. **Jedno miejsce ustalania języka.** Start demona, otwarcie okna i `set_language` wołają jedną funkcję `apply_language`. Czyta ona ustawienie, ustala `Lang` i robi `i18n::set`. Gdy język różni się od poprzedniego (każdy krok tylko wtedy, gdy dana rzecz istnieje: w testach nie ma zasobnika ani okna):
   - przebudowuje menu zasobnika (`TrayIcon::set_menu`);
   - ustawia tytuł otwartego okna (`WebviewWindow::set_title`, bo `document.title` nie zmienia natywnego paska) i otwartego okna logowania (`"Logowanie - {host}"` / `"Sign in - {host}"`, `sign_in_page.rs:160`);
   - wysyła do otwartego okna zdarzenie `language-changed` tym samym mechanizmem co dzisiejsze `pending-changed` (`window.eval`, `window.rs:141`); `app.js` robi na nie `location.reload()`, więc także okno już otwarte, które ktoś tylko przywołuje z zasobnika, przechodzi na nowy język;
   - zapisuje na nowo nazwy AUMID w rejestrze (to samo co `register()`), tylko w procesie, który sam zarejestrował AUMID przy starcie. Pomocnicy testów tego nie robią, więc testy nie piszą do rejestru.
   - Zmiana języka Windows przy `auto` wykryta przy otwarciu okna zmienia więc też menu i nazwy powiadomień, nie tylko okno.

## 5. Teksty po stronie Rusta

1. **Wzór.**
   - `i18n::pick(pl: &'static str, en: &'static str) -> &'static str` według `current()`.
   - Teksty z danymi: `match i18n::current() { Lang::Pl => format!(..), Lang::En => format!(..) }`.
   - Funkcje czyste, które się testuje (`Prompt::new`, `describe`, lista zmian, `localize_result`), dostają `Lang` parametrem, a wołający podaje `current()`.
   - Polskie stałe (`MODULE_CHANGED`, `UPDATE_IN_PROGRESS`, `TARGET_CUT`, `AUTONOMOUS_RULE`, stałe z `sign_in.rs`, `OTHER_ACCOUNT`) stają się funkcjami `fn module_changed() -> &'static str` i podobnymi.
2. **Zakres.**
   - Każdy literał z polską treścią w kodzie produkcyjnym `crates/open-mess/src` (dziś około 48, `grep` po polskich literach) dostaje obie wersje. Wyjątek: teksty czytane przez agentów, CLI albo moduły, które przechodzą na angielski (pkt 5 i 7).
   - To obejmuje też miejsca spoza inwentarza z sekcji 3, na przykład:
     - `commands.rs:594` ("stronę logowania można tylko anulować"), `:609` ("moduł podał nieprawidłowy adres strony logowania"), `:735` ("ten demon nie aktualizuje się sam");
     - `update/service.rs:413` ("nie ma pobranej wersji do zainstalowania").
   - Główne miejsca:
     - komendy okna i ich stałe, `describe`, lista zmian manifestu;
     - okno wyboru programu (tytuł i filtry);
     - `sign_in.rs`, `sign_in_page.rs` z tytułem okna logowania, `accounts.rs` (`sign_in`), komunikaty `update/service.rs` i `installable()`;
     - zasobnik i tytuł okna;
     - `approval.rs`: oba rodzaje toastów, przyciski, teksty rfd i nazwy AUMID.
   - Lista zmian manifestu ma osobny szablon dla każdego języka (polskie "nowy host" / "nowe narzędzie" zależą od rodzaju rzeczownika).
   - **Powody braku automatycznej instalacji.** `installable()` zwraca dziś gotowy polski tekst zapamiętany w `Setup.installable` przy starcie (`update/service.rs:240`, `:501-510`). Po zmianie zwraca typ powodu z danymi, na przykład `enum NotInstallable { OtherExe { running, task }, OtherDataDir { data, task }, UnknownTaskDataDir }`, gdzie `UnknownTaskDataDir` to dzisiejszy powód z `service.rs:523-525`, gdy nie wiadomo, jaki folder danych dostaje zadanie. Wariant istnieje tylko dla powodu, który kod dziś naprawdę produkuje. `UpdateService::view` i błąd `start_install` (`service.rs:404`) zamieniają go na tekst w bieżącym języku przy każdym odczycie, więc zmiana języka bez restartu zmienia też ten tekst.
3. **Toast kilku akcji.** Polska liczba mnoga przez `i18n::plural_pl(n, one, few, many)`:
   - 1 to forma "one";
   - liczby kończące się na 2-4, ale nie na 12-14, to forma "few";
   - reszta to forma "many".
   - Przykład: "open_mess: 2 akcje czekają na zatwierdzenie", "open_mess: 5 akcji czeka na zatwierdzenie". Angielski bez zmian ("actions", count >= 2).
4. **Okna rfd.**
   - Przyciski `YesNo` podpisuje Windows w swoim języku. Tekst nazywa przyciski w bieżącym języku: "Tak"/"Nie" albo "Yes"/"No".
   - Przy wyborze ręcznym innym niż język Windows te nazwy mogą się różnić od podpisów na przyciskach. To znane ograniczenie (sekcja 10).
5. **Wspólne typy błędów zostają po angielsku w obu językach.** Ich `Display` czytają też CLI i MCP.
   - `Error::UpdateInProgress` dostaje angielski `Display` "update in progress, try again in a moment".
   - `UpdateError::HelperNotStarted` dostaje "the update helper did not start".
   - Komendy okna, które już dziś zamieniają konkretny wariant na własny tekst (`UPDATE_IN_PROGRESS`), robią to dalej, w bieżącym języku.
   - Prefiks błędu w oknie zależy od języka: "Błąd: " albo "Error: ".
6. **Komunikaty z `accounts.rs`** powstają w `Accounts::sign_in` i w wołanym z niego `Accounts::add` (`accounts.rs:113`, "konto {} jest usuwane", bez polskich liter, więc wymienione wprost). Oba woła wyłącznie komenda okna `sign_in_start`, więc tłumaczy się je przy tworzeniu (`current()`). Ten sam tekst "moduł {module} nie działa" w `sign_in_page.rs` używa tej samej funkcji.
7. **Odmowy logowania dla modułu.** `NOT_FROM_WINDOW` i `NO_WINDOW` z `sign_in.rs` idą też do modułu jako `SignInReply::Refused` (`sign_in.rs:130`, `:139`), a stamtąd do odpowiedzi narzędzia, którą może czytać agent (`tests/sign_in.rs:429`).
   - Ta droga dostaje stały angielski tekst: "sign-in can only start from the configuration window", "the configuration window is not open".
   - Komenda okna zwraca tekst w bieżącym języku.
   - Tekst czytany przez agenta nie zależy więc od ustawienia okna (sekcja 2).

## 6. `last-update.txt`

1. **Zapis po angielsku.**
   - `Display` dla `Outcome` zapisuje stałą angielską postać: `installed`, `rolled back: {why}`, `aborted: {why}`, `rollback failed: {why}`, `rollback deferred: {why}`. Ten sam tekst trafia do logów.
   - Znacznik kopii zapasowej, który `apply.rs:347` dokleja dziś do `why` ("; kopia zapasowa: {path}"), ma postać `; backup: {path}`.
   - `NO_CONSENT` brzmi "no valid consent".
   - Każdy zapis do `last-update.txt` idzie przez `Outcome::to_string()`. Dotyczy to też zapisu demona w `update/service.rs:476` (`record_result(&format!("przerwano: {err}"))`), który zmienia się na `Outcome::Aborted(err.to_string())`. Żadne miejsce nie składa już tekstu wyniku samo.
2. **Tłumaczenie przy odczycie.** `UpdateView.last_result` przechodzi przez czystą funkcję `localize_result(raw: &str, lang: Lang) -> String`:
   - znany prefiks na początku zamienia na prefiks w danym języku ("zainstalowano", "przywrócono: ", "przerwano: ", "wycofanie nieudane: ", "wycofanie odłożone: ");
   - ostatnie wystąpienie `; backup: ` zamienia na "; kopia zapasowa: ";
   - resztę, czyli powód, zostawia bez zmian;
   - tekst bez znanego prefiksu (plik z wersji sprzed zmiany, po polsku) zwraca dosłownie.
3. Pomocnik i nowy demon nie muszą więc znać języka okna.

## 7. Okno

1. **Start okna.**
   - `app.js` przed pierwszym rysowaniem woła `language` i zapamiętuje `lang`.
   - Ustawia `document.documentElement.lang` i `document.title`.
   - Gdy `lang` to `en`, zamienia teksty elementów z `index.html` (pkt 3).
   - Jeśli `language` zawiedzie, okno działa po polsku jak dziś i pokazuje błąd.
2. **Teksty w JS.**
   - `tr(pl, en)` zwraca tekst w bieżącym języku.
   - `plural(n, forms)`: dla `pl` formy `{ one, few, many }` wybierane przez `Intl.PluralRules("pl")`, dla `en` formy `{ one, other }`.
   - Każdy literał widoczny dla człowieka w `app.js`, także w `confirm()` i `alert()`, staje się wywołaniem `tr(...)` albo `plural(...)`. Mapy identyfikatorów (`ACCOUNT_STATES`, `OUTCOMES`, ...) mają obie wersje.
   - Napisy złożone z polskiej gramatyki ("raz(y)", "kont połączonych", "Najwyżej pobudek na godzinę") dostają poprawne formy przez `plural`.
   - Zdanie z `TARGET_CUT` i "(pisze)/(czyta)", które są też w Ruście, mają w obu miejscach te same dwa języki.
3. **`index.html`.**
   - Elementy z tekstem zostają z polskim tekstem i dostają `data-en="..."`; `aria-label` dostaje `data-en-aria-label="..."`.
   - Zamiana działa przez `textContent` i `setAttribute`. Treść `data-en` pochodzi tylko z naszego pliku.
   - Wyjątek: przycisk "Do zatwierdzenia" z licznikiem (`<span id="waiting-count">`) ma tekst w osobnym `<span data-en="...">`, żeby zamiana nie skasowała licznika.
4. **Wybór języka.**
   - W sekcji "Demon i logi" / "Daemon and logs" jest pole wyboru: "Automatycznie (język Windows)" / "Automatic (Windows language)", "Polski", "English". Nazwy języków są zawsze we własnym języku.
   - Zmiana woła `set_language`, a potem `location.reload()`.
5. **Bezpieczeństwo strony bez zmian.** Dane (nazwy, tytuły, wiadomości, manifesty) nadal trafiają na stronę tylko przez `el()` / `textContent`. Tłumaczenie nie wprowadza `innerHTML`.

## 8. Threat model

- **Zaufane:**
  - kod i pliki okna z naszego repo, w tym treść `data-en` i teksty `tr()`;
  - lista języków z Windows (system użytkownika), ale traktowana jako dowolny napis (sekcja 4.2);
  - wartość `language` z lokalnej bazy: zła wartość znaczy `auto`, nic się nie psuje.
- **Niezaufane, bez zmian względem dziś:** nazwy, tytuły rozmów, treść wiadomości, manifesty i błędy modułów.
  - Trafiają do szablonów obu języków tylko jako dane: w oknie przez `textContent`, w toastach z dzisiejszym przycinaniem (`approval.rs`), nigdy jako szablon ani klucz tłumaczenia.
  - Tekst od modułu nie jest tłumaczony.
- **Bez nowych zależności, bez `unsafe`, bez sieci.** Feature `System_UserProfile` (i `Foundation_Collections`) crate'a `windows`, który już jest w drzewie.
- **Głębokość:** dwa języki, bez języków pisanych od prawej, bez formatowania dat i liczb według kraju (dziś okno pokazuje je tak samo w obu językach), bez tłumaczenia `Display` wspólnych błędów.

## 9. Dotknięte pliki

- **Nowy:** `crates/open-mess/src/i18n.rs`: `Lang`, `from_tag`, `current`/`set`, `pick`, `plural_pl`, ustalenie języka z ustawienia i Windows, testy.
- **Okno i zasobnik:**
  - `crates/open-mess/src/lib.rs`: moduł `i18n`, angielski `Display` dla `UpdateInProgress`;
  - `crates/open-mess/Cargo.toml`: feature `System_UserProfile` i `Foundation_Collections` crate'a `windows`;
  - `crates/open-mess/src/ui/commands.rs`, `ui/mod.rs` (komendy `language` i `set_language`), `ui/window.rs` (zasobnik, tytuł, przebudowa menu, ustalenie języka przy otwarciu), `ui/sign_in.rs`, `ui/sign_in_page.rs`;
  - `crates/open-mess/build.rs` i `crates/open-mess/capabilities/main.json` (nowe komendy);
  - `crates/open-mess/tests/common/mod.rs` (język w pomocniku);
  - `crates/open-mess/ui/index.html` i `crates/open-mess/ui/app.js`.
- **Konta i powiadomienia:** `crates/open-mess/src/accounts.rs` i `crates/open-mess/src/approval.rs`.
- **Aktualizacje:**
  - `crates/open-mess/src/update/mod.rs` (`HelperNotStarted`), `update/apply.rs` (`Outcome`, `NO_CONSENT`), `update/service.rs` (komunikaty i `localize_result` w `UpdateView`);
  - `crates/open-mess/src/daemon.rs` albo miejsce otwarcia bazy: ustalenie języka na starcie.
- **Testy:** `crates/open-mess/tests/ui.rs`, `sign_in.rs`, `update_service.rs`, `update_probation.rs`, `update_apply.rs`, `update_quiesce.rs` i nowe przypadki w `i18n.rs` i `approval.rs`.

## 10. Kryteria akceptacji

1. Na Windows z językiem `pl-PL` i ustawieniem `auto` w jednym języku polskim są:
   - okno, tytuł okna i menu zasobnika;
   - okno logowania;
   - toasty zatwierdzania (jednej i kilku akcji), toast aktualizacji, okna rfd i okno wyboru programu.
   - Jedyny wyjątek to `Display` wspólnych błędów po prefiksie "Błąd: " i powody w "Ostatni wynik", które zostają po angielsku.
2. Na Windows z językiem `en-US` (albo `de-DE`) wszystko z punktu 1 jest po angielsku, bez polskich słów.
3. Przełączenie w oknie na "English" przy polskim Windows przeładowuje okno po angielsku. Bez restartu po angielsku są też menu zasobnika i następne toasty. "Automatycznie" wraca do języka Windows.
4. CLI, narzędzia MCP i logi nie zmieniają tekstów poza tekstami, które dziś są po polsku, a od teraz będą po angielsku:
   - błędy `UpdateInProgress` i `HelperNotStarted`;
   - odmowy logowania przekazywane modułowi (5.7).
4a. Zmiana języka w oknie w trakcie sprawdzania nowej wersji zwraca błąd "aktualizacja w toku" i nie zmienia ustawienia. Po zmianie języka natywny pasek tytułu otwartego okna jest w nowym języku.
5. `last-update.txt` zapisany przez nową wersję jest po angielsku. Okno pokazuje go w bieżącym języku. Plik po polsku ze starszej wersji pokazuje dosłownie.
6. **Testy jednostkowe:**
   - `from_tag`: `pl`, `pl-PL`, `PL-pl`, `en-US`, `de-DE`, pusty napis, `None`;
   - `plural_pl`: 1, 2, 4, 5, 11, 12, 14, 21, 22, 25, 102, 112;
   - `localize_result`: każdy prefiks w obu językach, z przyrostkiem kopii zapasowej, tekst nieznany i stary polski dosłownie;
   - `Prompt::new` (wysłanie, edycja, reakcja) w obu językach;
   - `describe` i lista zmian manifestu w obu językach;
   - `set_language` ze złą wartością zwraca błąd i nie zmienia ustawienia.
7. **Testy, które dziś porównują polskie teksty,** dostają `language = pl` w bazie przed startem demona (4.3) i porównują z funkcjami zamiast stałych.
   - Nowy plik `tests/ui_en.rs` (`language = en`) sprawdza po angielsku te same stany co polskie przypadki z `tests/ui.rs`, `tests/sign_in.rs` i `tests/update_service.rs`: opis modułu, błędy logowania, aktualizację w toku, powód braku automatycznej instalacji i ostatni wynik.
   - Testy plików `last-update.txt` sprawdzają postać angielską, także zapis z `update/service.rs:476` i znacznik kopii zapasowej.
   - Test odmowy logowania dla modułu (`tests/sign_in.rs:429`) sprawdza tekst angielski przy `language = pl`.
8. **Test plików** (Rust, czyta pliki z repo):
   - każdy element `index.html` z własnym tekstem ma `data-en`, a każdy `aria-label` ma `data-en-aria-label`;
   - każda linia `app.js` z polską literą (ąćęłńóśźż, także wielkie) zawiera `tr(` albo jest formą `plural` (`one:`, `few:`, `many:`);
   - w każdym pliku `crates/open-mess/src/**/*.rs`, przed pierwszym `#[cfg(test)]`, każda linia kodu z polską literą w literale zawiera `pick(`, `Lang::Pl` albo `plural_pl(`, albo któraś z 4 poprzednich linii je zawiera (rustfmt łamie długie wywołania na linie kontynuacji). Polskie prefiksy w `localize_result` stoją w gałęzi `Lang::Pl`. Linie zaczynające się od `//` są pomijane;
   - etykiety cytowane w `autonomous_rule()` dla obu języków występują w oknie: etykiety uprawnień w `app.js`, nazwa sekcji w `index.html` (polska w tekście, angielska w `data-en`).
   - Te sprawdzenia nie łapią polskich słów bez polskich liter ("brak", "Konto"). To znane ograniczenie: resztę łapie review i kryterium 2.
9. `cargo xtask ci` zielone na Windows i ubuntu.

## 11. Poza zakresem

- CLI, opisy narzędzi MCP, logi i JSON `status`.
- Tłumaczenie `Display` wspólnych błędów i powodów w `Outcome`. Wymagałoby kodów błędów przez wszystkie crate'y.
- Teksty od modułów: manifesty i ich błędy.
- Trzeci język, języki pisane od prawej, format dat i liczb.
- Podpisy przycisków, które rysuje Windows (rfd `YesNo`, `confirm()`/`alert()` w WebView2). Idą za językiem Windows, nie za wyborem ręcznym.

## 12. Rejestr rebutali

(pusty)

## Changelog rund

R1:
- Powody braku automatycznej instalacji są tłumaczone przy odczycie, a nie zapamiętywane przy starcie (Codex P2).
- Znacznik kopii zapasowej wewnątrz powodu wyniku: zapis po angielsku i tłumaczenie przy odczycie (Codex P2).
- `set_title` otwartego okna po zmianie języka (GLM P2).
- Zakres to każdy polski literał w kodzie produkcyjnym, z brakującymi miejscami wymienionymi wprost, oraz test literałów Rusta (GLM P2).
- Zapis demona `przerwano: {err}` idzie przez `Outcome` (GLM P2).
- Jeden język na plik testów, ustawiany w bazie przed startem demona, i przypadki angielskie w osobnym pliku zamiast globalnego `set` w równoległych testach (GLM P2).
- Odmowy logowania dla modułu po angielsku (GLM P3).
- Ograniczenie testu słów bez polskich liter opisane wprost (GLM P3).
- `set_language` za blokadą zapisów w trakcie aktualizacji (GLM P3).

R2 (GLM zatwierdził):
- Jedna funkcja `apply_language` dla startu, otwarcia okna i `set_language`. Przebudowuje menu zasobnika, tytuł okna i nazwy AUMID przy każdej zmianie języka, także wykrytej przy otwarciu okna (Codex P2, GLM P3).
- Wariant `UnknownTaskDataDir` i tłumaczenie powodu także w błędzie `start_install` (Codex P3, GLM P3).
- Poprawione wskazanie `not_held` (GLM P3).
- `build.rs` i `capabilities/main.json` w dotkniętych plikach (GLM P3).
- `Accounts::add` ("konto {} jest usuwane") wymienione wprost (GLM P3).
- Pomocnicy testów wołają `apply_language` (GLM P3).

R3 (GLM zatwierdził):
- Zdarzenie `language-changed` przeładowuje okno już otwarte, gdy `apply_language` zmieni język (Codex P2).
- AUMID zapisywany tylko w procesie, który go zarejestrował, więc nie w testach (GLM P3).
- Test etykiet `autonomous_rule()` czyta też `index.html` (GLM P3).

R4: Codex i GLM zatwierdzili. Uwagi P3 wprowadzone bez kolejnej rundy:
- tytuł otwartego okna logowania zmienia się z językiem (Codex);
- warianty `NotInstallable` tylko dla powodów, które kod produkuje (GLM);
- test literałów Rusta dopuszcza `plural_pl(` i linie kontynuacji po rustfmt (GLM).
