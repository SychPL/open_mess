# open_mess - wtyczka Paperclip i tryb serwerowy

Data: 2026-11-03
Status: zatwierdzony (Codex, GLM, runda 6)
Powiązane: `.specs/2026-10-26-open-mess-design.md` (8.1 lokalne API, 8.4 sekrety, 8.6 uprawnienia, 9.1 logowanie, 9.5 testy na koncie firmowym), `docs/protocols/teams.md` (1.3, "Needs live capture" 2), `crates/open-mess/src/{api,daemon,secrets,paths}.rs`, `crates/open-mess/src/ui/commands.rs`, `crates/open-mess-host/src/{secrets,sign_in}.rs`.

## 1. Cel

open_mess staje się konektorem w Paperclip, platformie, w której agenci (Claude Code, Codex) pracują nad zadaniami firmy właściciela.

Po wdrożeniu wszystkich etapów właściciel:
- instaluje w Paperclip wtyczkę "open_mess";
- klika "Połącz", loguje się do Teams w przeglądarce i wkleja jeden adres;
- ustawia uprawnienia agentów i zatwierdza ich wysyłki w panelu Paperclip.

Agenci Paperclip dostają narzędzia open_mess (te same nazwy, parametry i wyniki co MCP i CLI). Demon open_mess działa na serwerze obok Paperclip, w osobnym kontenerze. Wersja Windows działa jak dotąd.

Ten spec opisuje całość w etapach. Etap 1 (rozdz. 5) jest implementowany od razu. Etapy 2-4 dostaną własne, krótsze specy przed implementacją, jeśli review uzna, że ich szczegóły tego wymagają.

## 2. Decyzje właściciela (2026-11-03)

1. open_mess ma być jednym z konektorów w Paperclip, w formie wtyczki Paperclip.
2. Paperclip nie może mieć dostępu do komputera właściciela, więc demon open_mess działa na serwerze obok Paperclip.
3. **Tokeny Teams leżą na serwerze właściciela**, a Teams widzi ruch z adresu serwera (centrum danych). Właściciel przyjął to ryzyko po wyjaśnieniu przed próbą na żywo (rozdz. 3.3). Dla tego wdrożenia serwer jest "maszyną użytkownika" w rozumieniu AGENTS.md ("Secrets never leave the user's machine").
4. **Wszystkie ustawienia i zatwierdzanie są w Paperclip**, jak w konfiguracji wersji Windows. Właściciel świadomie przyjął, że agenci Paperclip mają pełną powłokę w kontenerze Paperclip. W teorii mogą więc użyć poświadczeń wtyczki, zatwierdzić własną wysyłkę albo zmienić uprawnienia. Granica "zatwierdzenie poza zasięgiem agenta" z 8.6 projektu nie obowiązuje w tym wdrożeniu (rozdz. 8).

Decyzje projektowe (nie właściciela, do review):
- **Demon w osobnym kontenerze, nie w kontenerze Paperclip.** Tokeny odświeżania Teams nie leżą tam, gdzie agenci mają powłokę: nie da się ich wynieść, najwyżej użyć przez API open_mess, póki demon działa.
- **Wtyczka jest cienka.** Nie zna protokołu Teams i niczego nie decyduje. Przekazuje wywołania narzędzi do API demona, a akcje panelu do API administracyjnego demona.
- **Logowanie przez jedno wklejenie adresu.** Identyfikator aplikacji Teams ma zarejestrowane tylko przekierowanie `nativeclient` (`docs/protocols/teams.md` 1.3), więc Microsoft nie przekieruje z powrotem do Paperclip. Własna rejestracja aplikacji wymagałaby zgody administratora organizacji i innego API (Graph), czyli byłaby innym produktem.

## 3. Kontekst (sprawdzony)

### 3.1 Paperclip

Dane z działającej instancji właściciela (`ghcr.io/paperclipai/paperclip`, kod w `/app`):

- **Kontener i agenci.** Paperclip działa w kontenerze Docker w sieci mostkowej compose (`paperclip_default`). W tym samym kontenerze są serwer, wbudowany Postgres i procesy wtyczek. Są tam też agenci `claude_local` i `codex_local`, uruchamiani z `dangerouslySkipPermissions` / `dangerouslyBypassApprovalsAndSandbox`, czyli z pełną powłoką. Panel jest wystawiony tylko na `127.0.0.1` serwera, a właściciel otwiera go tunelem SSH.
- **Wtyczki** (`packages/plugins/sdk/README.md`):
  - paczka npm z manifestem, procesem roboczym Node (`entrypoints.worker`, JSON-RPC z hostem) i paczką UI dla slotów (`settingsPage`, `page`, `sidebar`, `dashboardWidget` itd.);
  - możliwości deklarowane w manifeście: `agent.tools.register` (narzędzia dla agentów, `ctx.tools.register`, wynik `{content, data, error}`), `http.outbound` (zwykły `fetch` Node też działa), `secrets.read-ref` (`ctx.secrets.resolve` dla referencji do sekretów Paperclip w konfiguracji wtyczki), `ui.page.register`, `instance.settings.register`, `jobs.schedule`, `events.subscribe`;
  - UI woła proces roboczy przez `usePluginData` / `usePluginAction`;
  - narzędzia deklarowane statycznie w `manifest.tools`, z nazwami poprzedzanymi identyfikatorem wtyczki;
  - SDK mówi wprost, że kod wtyczki (proces i UI) jest zaufany, a UI działa w tej samej domenie co panel, z sesją zarządu.
- **Zatwierdzenia Paperclip.** Wtyczka może je czytać i rozstrzygać (`ctx.approvals`), ale tworzyć nie (brak takiej możliwości). Lista "do zatwierdzenia" open_mess będzie więc stroną wtyczki, a nie natywnym zatwierdzeniem Paperclip.

### 3.2 open_mess (`main` 85b63f3)

- **Linux.** Kompiluje się i przechodzi testy w CI na `ubuntu-latest` (`ci.yml`). Zależności Windows są pod `cfg(windows)`: Tauri, zasobnik, powiadomienia, WAM, Menedżer poświadczeń, aktualizacje.
- **Sekrety.** Poza Windows `secrets::credential_manager_entry` zwraca błąd, więc `local_api_token` nie działa i demon nie wstaje. `open_mess_host::secrets::platform_store` sięga po fasadę `keyring` 4, która na Linuksie szuka Secret Service przez D-Bus; w kontenerze go nie ma.
- **API.** `api::bind` słucha tylko na `Ipv4Addr::LOCALHOST`. Jeden token Bearer chroni `/api/tools`, `/api/call/{tool}`, `/api/ui/open` i `/mcp`.
- **Okno.** Komendy okna (`ui/commands.rs`) to zwykłe funkcje na `&UiState`, wołane z Tauri (`window.rs`). `UiState` powstaje na każdej platformie (`daemon.rs`). Logowanie w oknie: `sign_in_start` czeka na koniec logowania, a okno w tym czasie odpytuje `sign_in_request` i odpowiada `sign_in_answer`.
- **Strona logowania.** Prośba modułu `open-page(url, redirect-prefix)` (WIT `sign-in`) trafia do okna jako `SignInRequest::OpenPage`. Na Windows otwiera ją WebView z kontrolą hostów (`ui/sign_in_page.rs`). `SignInPromptDto` dla `page` niesie dziś tylko host strony, nie pełny adres.
- **Moduł Teams** loguje się wyłącznie przez WAM (`modules/teams/src/api.rs`).

### 3.3 Próba na żywo (2026-11-03, właściciel obecny, za jego zgodą, 9.5)

Cel: kod autoryzacji + PKCE z identyfikatorem `1fec8e78-...` i przekierowaniem `nativeclient`.
- Logowanie: przeglądarka na komputerze właściciela.
- Wymiana kodu i odświeżanie: z serwera (inny adres IP, centrum danych).

Wyniki (tylko statusy i nazwy pól):
- wymiana kodu (zakres `https://api.spaces.skype.com/.default offline_access`): 200, token odświeżania wydany, bez `refresh_token_expires_in` (to nie jest rejestracja SPA), token dostępu na 70 min;
- odświeżenie na `https://ic3.teams.office.com/.default`: 200, `scp = Teams.AccessAsUser.All`;
- `authsvc`: 200;
- lista rozmów z `chatServiceAfd`: 200.

Polityki dostępu organizacji przyjęły tę drogę. Niewiadome: czas życia tokenu odświeżania i późniejsze alarmy ochrony tożsamości. Wynik trafi do `docs/protocols/teams.md` ("Live results" 4) w etapie 2.

## 4. Architektura docelowa

```
 przeglądarka właściciela --tunel SSH--> panel Paperclip (wtyczka: strona open_mess)
                                              |
 kontener paperclip: serwer + agenci + proces wtyczki
        |  /api/call/{tool}  (token agentów)
        |  /admin/{komenda}  (token administracyjny)
        v
 kontener open-mess: om daemon (Linux), wolumen z bazą i sekretami, moduł Teams (WASM)
        |
        v
 Teams (Microsoft)
```

**Kontener `open-mess`:**
- `om daemon` z wolumenem `/data` (`OPEN_MESS_DATA_DIR`);
- słucha na `0.0.0.0:<port>` wewnątrz kontenera, wyłącznie w sieci `paperclip_default`, bez publikacji portu na hoście;
- z sieci osiąga go tylko kontener Paperclip.

**Dwa tokeny** generuje demon (5.3). Właściciel odczytuje je raz (`docker exec open-mess om server-tokens`) i wpisuje jako sekrety Paperclip, z których czyta je wtyczka:
- **token agentów** - dzisiejszy token lokalnego API: `/api/tools`, `/api/call/{tool}`, `/mcp`;
- **token administracyjny** - wyłącznie `/admin/*`: to, co na Windows robi okno konfiguracji.

**Wtyczka** (etap 3):
- narzędzia agentów = narzędzia rejestru, przekazywane do `/api/call/{tool}`;
- strona "open_mess" w panelu: konta i "Połącz", uprawnienia, "Do zatwierdzenia", stan;
- jej akcje wołają `/admin/*`.

## 5. Etap 1 - tryb serwerowy demona (Rust, ten PR)

Wszystko poza Windows (`cfg(not(windows))`), chyba że napisano inaczej. Wersja Windows nie zmienia zachowania.

### 5.1 Adres nasłuchu

- **Zmienna `OPEN_MESS_LISTEN`:** jedyna dozwolona wartość to `0.0.0.0` (wszystkie interfejsy IPv4 kontenera). Bez zmiennej demon słucha na `127.0.0.1`. Inna wartość to błąd startu z czytelnym komunikatem, nie cichy powrót do domyślnej.
- **Dlaczego tylko ta.** Konkretny adres interfejsu odciąłby CLI, które zawsze łączy się z `127.0.0.1` (`cli.rs`), a `0.0.0.0` obejmuje też pętlę zwrotną. IPv6 nie jest potrzebne w sieci compose.
- **Windows:** zmienna jest ignorowana, a demon słucha tylko na `127.0.0.1` (8.1 projektu bez zmian).
- **Port** jak dotąd: z bazy, domyślnie 7341.
- Linia startowa na stdout (`open_mess daemon listening on ...`) podaje rzeczywisty adres nasłuchu zamiast stałego `127.0.0.1`. Bez tokenów. Demon nie ma innego ujścia logów.

### 5.2 Magazyn sekretów w pliku

- **Plik i format.** Nowy magazyn `FileStore` (implementacja `keyring_core::api::CredentialStoreApi` i `CredentialApi`) w `<data>/secrets.json`. Mapa `service -> user -> sekret`, w JSON. Każdy zapis i usunięcie od razu zapisuje cały plik atomowo: plik tymczasowy w tym samym folderze, `fsync` pliku, `rename`, potem `fsync` folderu (Unix). Bez tego ostatniego wpis po `rename` może zniknąć po awarii zasilania.
- **Współbieżność.** Jedna blokada (`Mutex`) w procesie obejmuje odczyt pliku, zmianę i zapis. Równoległe zapisy (np. `remove_account` i logowanie w tle innego modułu) nie gubią się wzajemnie. Pisze tylko demon; CLI tylko czyta (5.3), więc blokada między procesami nie jest potrzebna.
- **Uprawnienia.** Plik ma tryb 0600, folder danych 0700; przy starcie poprawiane, jeśli są szersze.
- **Uszkodzony plik.** Błąd startu z czytelną przyczyną. Pliku nie nadpisujemy, żeby nie zgubić sekretów.
- **Limity.** Rozmiar pliku ograniczony (np. 1 MiB): magazyn nie przyjmie zapisu ponad limit. Limity modułu (`MAX_SECRETS`) działają jak dotąd.
- **Instalacja.** Jedna funkcja `secrets::install_platform_store(data_dir)` instaluje `FileStore` jako domyślny magazyn `keyring_core`. Woła ją przed pierwszym użyciem sekretów zarówno demon, jak i CLI (`cli.rs`, przed `token()`). `open_mess_host::secrets::platform_store` zwraca magazyn domyślny, jeśli już jest ustawiony, i nie sięga wtedy po fasadę `keyring`. Fasada przy pierwszym użyciu podmienia magazyn domyślny na platformowy, na Linuksie nietrwały albo wymagający D-Bus. Na Windows nic się nie zmienia (Menedżer poświadczeń).

Plik jest niezaszyfrowany. Klucz musiałby leżeć obok, więc szyfrowanie nic by nie dało. Ochroną jest izolacja kontenera i wolumenu (rozdz. 8).

### 5.3 Tokeny

- **Generuje je demon,** nie właściciel, żeby żaden token nie był słaby. Przy pierwszym starcie poza Windows demon tworzy:
  - token agentów, jak dziś (`local_api_token`);
  - token administracyjny (`admin_api_token`, użytkownik `admin-api` w usłudze `open_mess`).

  Oba to 32 losowe bajty w hex, zapisane w `FileStore`.
- **Polecenie `om server-tokens`** (tylko poza Windows) wypisuje oba tokeny, żeby właściciel wkleił je do sekretów Paperclip. Uruchamia się je przez `docker exec open-mess om server-tokens`, czyli wyłącznie z uprawnieniami do Dockera na serwerze. Kontener Paperclip ma do `open-mess` tylko sieć, więc tego polecenia nie uruchomi. To polecenie konfiguracji jak `om config` czy `om daemon`, nie narzędzie rejestru: agenci go nie widzą. Działa także wtedy, gdy demon nie działa: czyta `FileStore`, a gdy tokenów jeszcze nie ma, mówi, że najpierw trzeba uruchomić demona.
- **CLI w kontenerze** czyta token agentów z `FileStore` (5.2). `OPEN_MESS_TOKEN` działa w CLI jak dotąd, jako nadpisanie.
- **Wymiana tokenu:** usunięcie wpisu z `secrets.json` (przy zatrzymanym demonie) i start. Osobne polecenie jest poza zakresem.
- **Na Windows** token administracyjny nie powstaje, a `/admin/*` odpowiada 404. Okno robi to samo przez Tauri.

### 5.3a Zatrzymanie przez SIGTERM

`docker stop`, `compose down` i restart wysyłają SIGTERM. Demon jako PID 1 bez obsługi tego sygnału go ignoruje i po limicie czasu dostaje SIGKILL, a zatwierdzona wysyłka w toku zostaje ucięta. Dlatego poza Windows `until_stopped` (`daemon.rs`) czeka na Ctrl+C **albo** SIGTERM (`tokio::signal::unix`) i zamyka się tą samą ścieżką co po Ctrl+C. Na Windows bez zmian.

Domyślne 10 s Dockera to mniej niż `CALL_TIMEOUT` (30 s) jednego wywołania modułu. Fragment compose z etapu 4 ustawia więc `stop_grace_period: 60s`, żeby trwająca wysyłka zdążyła się skończyć.

### 5.4 API administracyjne

`POST /admin/{komenda}` z ciałem JSON (argumenty) i odpowiedzią JSON. Dostęp:
- tylko z tokenem administracyjnym (porównanie w stałym czasie, istniejące `constant_time_eq`);
- token agentów na `/admin/*` i token administracyjny na `/api/*` i `/mcp` dostają 401.

Komendy to istniejące funkcje `ui/commands.rs`, bez nowej logiki. Wynik `Err(String)` daje 400 z `{"error": ...}`. Zestaw na etap 1:
- `status`, `accounts`, `remove_account {id}`;
- `modules`, `approve_module {name, file_sha256, manifest_sha256}`, `set_module_enabled {name, enabled}`, `rescan_modules`: moduł na serwerze też wymaga zgody na manifest (8.3), a podmiana pliku `teams.wasm` w wolumenie wymaga ponownego przejrzenia;
- `permissions {account}`, `set_permissions {account, conversation?, read, write}`, `clear_permission_override {account, conversation}`;
- `waiting`, `decide {action, approve}`;
- `sign_in_start {module, account?}`, `sign_in_request`, `sign_in_answer {id, answer}`, `sign_in_result`.

Wszystkie przechodzą przez te same blokady co w oknie (`changes_allowed` w trakcie aktualizacji, której na serwerze i tak nie ma). Wyzwalacze i reszta okna wejdą w późniejszym etapie, jeśli właściciel ich zechce.

**Logowanie bez okna:**
- `sign_in_start` uruchamia logowanie w tle i od razu zwraca `{"started": true}`. Drugie logowanie w tym czasie dostaje błąd, jak w oknie.
- **Implementacja.** `commands::sign_in_start` dzielimy na dwa kroki:
  1. synchroniczny: `changes_allowed` i `SignIns::begin`, zwraca strażnika `Active`;
  2. asynchroniczny: `accounts.sign_in` i `AccountDto`, ze strażnikiem przy sobie.

  Komenda okna woła oba kroki po kolei, więc jej semantyka (czekanie na koniec) się nie zmienia. Handler API wykonuje krok 1 w samym żądaniu: drugie równoczesne `sign_in_start` dostaje błąd "trwa już inne logowanie" od razu, zanim cokolwiek się uruchomi. Dopiero potem handler uruchamia krok 2 w zadaniu tokio. Wynik zadania trafia do jednego miejsca na wynik, które czyta `sign_in_result`. Krok 1 zeruje to miejsce na `running`, więc żaden starszy wynik nie nadpisze stanu trwającego logowania.
- `sign_in_result` zwraca stan: `idle`, `running`, `done` z `AccountDto` albo `failed` z komunikatem. Wynik trzymamy do następnego `sign_in_start`.
- `sign_in_request` dla prośby `page` zwraca w nowym polu `open_url` pełny adres strony logowania do otwarcia w przeglądarce. Adres zawiera wyzwanie PKCE i `state`; to nie są sekrety.
- **Kontrola hostów, jak na Windows.** Zanim prośba `page` stanie się widoczna, rdzeń sprawdza ją tym samym `ui::sign_in_page::check_page` z `modules::approved_sign_in_hosts` co okno: HTTPS, host adresu i prefiksu z zatwierdzonych hostów logowania modułu (8.3, R10). Prośba, która nie przejdzie, nie trafia do `sign_in_request`: rdzeń odpowiada na nią od razu `refused` z przyczyną. To ta sama techniczna odmowa, jaką dziś daje okno Windows po nieudanym `check_page` (`window.rs`): moduł może spróbować innej drogi albo zakończyć logowanie błędem, widocznym w `sign_in_result`. Filtr działa na każdej platformie w ścieżce próśb rdzenia. Na Windows moduł dostaje tę samą odmowę co dziś, tylko zanim okno cokolwiek otworzy. Kontrola obejmuje tylko adres początkowy. Dalsze przejścia (np. firmowy serwer federacyjny) dzieją się już w przeglądarce właściciela, poza zasięgiem open_mess; na Windows WebView o nie pyta, tu nie ma czego zapytać.
- `sign_in_answer` dla prośby `page` przyjmuje `{"done": "<wklejony adres>"}` i sprawdza go, zanim odda modułowi:
  - po usunięciu białych znaków adres zaczyna się od `redirect_prefix` prośby;
  - jest HTTPS;
  - ma nie więcej niż limit długości prośby (`MAX_PROMPT_TEXT`).

  Inny adres daje błąd komendy, a prośba czeka dalej.

**Opisy narzędzi.** Opis `send` (i odsyłające do niego `edit_message`, `react`) wspomina dziś tylko "a Windows notification" (`tools/shared.rs`). Zmieniamy go na neutralny: "waits for the user's approval outside the agent's reach (a Windows notification or the open_mess page in Paperclip)", żeby agent na serwerze nie kierował właściciela do nieistniejącego powiadomienia.

Na Windows `open_url` też jest wypełniany, ale okno go nie używa (WebView jak dotąd). Odpowiedź `done` na prośbę `page` z komendy okna jest na Windows dalej odrzucana, jak dziś ("stronę logowania można tylko anulować"). Tam odpowiada wyłącznie WebView po kontroli każdego przejścia. Przyjmowanie wklejonego adresu dotyczy tylko budowy poza Windows.

### 5.5 Kryteria akceptacji etapu 1

Każde ma test, który bez zmiany nie przechodzi. Testy zależne od platformy działają w CI na Linuksie.

1. `OPEN_MESS_LISTEN`:
   - `0.0.0.0` jest przyjmowane;
   - `::`, `10.0.0.5`, `localhost` i `abc` dają błąd startu;
   - bez zmiennej demon słucha na `127.0.0.1`;
   - na Windows zmienna jest ignorowana (test jednostkowy funkcji wyboru adresu z flagą platformy);
   - przy `0.0.0.0` CLI łączy się przez `127.0.0.1`.
2. `FileStore`:
   - zapis, odczyt i usunięcie;
   - drugi `FileStore` na tym samym pliku widzi zapisane sekrety;
   - plik ma tryb 0600 (Unix);
   - uszkodzony plik daje błąd i zostaje nietknięty;
   - zapis ponad limit daje błąd, a plik się nie zmienia.
3. Sekrety modułu (Teams) przechodzą przez `FileStore`, gdy jest domyślnym magazynem: istniejący test sekretów hosta na magazynie plikowym.
4. Tokeny:
   - pierwszy start (Linux) tworzy w `FileStore` dwa różne tokeny po 64 znaki hex;
   - drugi start ich nie zmienia;
   - `om server-tokens` wypisuje oba, a bez nich mówi, że trzeba uruchomić demona;
   - CLI (Linux) bez `OPEN_MESS_TOKEN` woła `status` działającego demona tokenem z `FileStore`.
5. API administracyjne:
   - `api::serve` bez tokenu administracyjnego (jak na Windows): każde `/admin/*` daje 404;
   - z nim: token administracyjny przechodzi `/admin/status`;
   - token agentów na `/admin/status` daje 401, a token administracyjny na `/api/tools` i `/mcp` daje 401;
   - nieznana komenda daje 404, złe argumenty 400.
5a. SIGTERM wysłany do demona (Linux, test demona) kończy go tą samą ścieżką co Ctrl+C, z kodem 0.
6. Logowanie bez okna, modułem testowym (`test-echo` z prośbą `open-page`):
   - `sign_in_start` wraca od razu;
   - `sign_in_request` daje `page` z `open_url`;
   - `sign_in_answer` z adresem spoza `redirect_prefix` daje błąd, a prośba zostaje;
   - poprawny adres kończy logowanie, a `sign_in_result` daje `done`;
   - prośba `page` z adresem spoza zatwierdzonych hostów logowania (test filtra próśb z ręcznie zbudowanym `SignInRequest::OpenPage` i manifestem, jak w `tests/sign_in.rs`, bo `test-echo` otwiera tylko hosty ze swojego manifestu): nie trafia do `sign_in_request`, a moduł dostaje `refused`.
7. Demon wstaje na Linuksie z `OPEN_MESS_DATA_DIR` w folderze tymczasowym, bez D-Bus i bez zmiennej `APPDATA` (test demona).
8. `cargo xtask ci` zielone na Windows i Linuksie.

## 6. Etap 2 - logowanie Teams bez WAM (moduł)

Kierunek (szczegóły w osobnym specu, jeśli review tego wymaga):
- **Wybór drogi.** Moduł próbuje WAM. Gdy host go nie ma, przechodzi na kod autoryzacji + PKCE przez `open-page`. Na Windows to też droga zapasowa z 9.1. Poza Windows `wam-token` zwraca dziś `refused("Windows account sign-in exists only on Windows")` (`crates/open-mess-host/src/wam.rs`). Etap 2 ustali, po czym moduł to rozpoznaje: po tekście albo po nowym wariancie błędu WIT.
- **Token odświeżania** w sekretach modułu. Tokeny dla każdego odbiorcy z tabeli 1.4 pobiera grantem `refresh_token` z zakresem odbiorcy. Odświeża przed wygaśnięciem i po wyzwaniu CAE.
- **Testy** dekoderów i przepływu na oczyszczonych nagraniach.
- **Dokumentacja:** wynik z 3.3 do `docs/protocols/teams.md`.

## 7. Etapy 3 i 4 - wtyczka i wdrożenie

**Etap 3. Wtyczka** (TypeScript, `integrations/paperclip/` w tym repo, z SDK `@paperclipai/plugin-sdk` w wersji instancji):
- **Narzędzia.** Manifest deklaruje narzędzia z pliku generowanego przez xtask z `Registry::list()`. Test w Ruście porównuje ten plik z `Registry::list()`, żeby wtyczka nie rozjechała się z MCP i CLI (zasada jednego rejestru).
- **Mapowanie pól:** `ToolInfo.name` -> `name` bez zmian, `description` -> `description`, `input_schema` -> `parametersSchema`, `displayName` = nazwa.
- **Przedrostek Paperclip.** Paperclip dokleja do nazwy narzędzia identyfikator wtyczki. To przestrzeń nazw hosta, taka sama jak `mcp__open_mess__status` w Claude Code dla serwera MCP. Nazwa w manifeście i to, co wtyczka przekazuje do `/api/call/{tool}`, jest dokładnie nazwą z rejestru. Test porównuje nazwy bez przedrostka, bo tylko te deklaruje wtyczka.
- **Bez `get_attachment`.** Wyjątek jawnie zapisany w teście: narzędzie zapisuje plik w wolumenie kontenera `open-mess` i zwraca ścieżkę, której agent Paperclip nie otworzy. Wróci, gdy będzie zwracać treść (rozdz. 10).
- **Wywołania.** Proces wtyczki woła `/api/call/{tool}`. Wynik zwraca agentowi jako `content` (JSON z polami niezaufanymi tak, jak oddaje je rdzeń) i `data`.
- **Strona i konfiguracja.** Strona `open_mess` w panelu przez akcje procesu wtyczki:
  - **Moduły:** lista z przeglądem manifestu (hosty sieciowe, hosty logowania, tożsamość), "Zatwierdź" i "Włącz/Wyłącz", "Odśwież" (`rescan_modules`). Pierwsze uruchomienie z `teams.wasm` zaczyna się tutaj, bo niezatwierdzony moduł nie zaloguje konta (8.3).
  - **Konta** z "Połącz" (link do otwarcia, pole na wklejony adres).
  - **Uprawnienia.**
  - **"Do zatwierdzenia"** z pełnym podglądem i przyciskami. Strona odpytuje listę, póki jest otwarta. Akcja czeka `APPROVAL_TTL` (10 min, `pending.rs`); bez powiadomień niewidziana akcja wygasa, a agent dostaje "expired". Zapisane w `docs/paperclip.md`.
  - **Stan.**

  Konfiguracja wtyczki: adres demona i dwie referencje do sekretów Paperclip.
- **Testy** narzędziem testowym SDK, w osobnym zadaniu CI z Node.

**Etap 4. Wdrożenie:**
- `Dockerfile` (Linux `om` + `teams.wasm`);
- fragment compose: serwis w sieci Paperclip, wolumen, `OPEN_MESS_LISTEN=0.0.0.0`, `stop_grace_period: 60s` (5.3a), bez publikacji portu;
- `docs/paperclip.md` z instrukcją: budowa, uruchomienie, zgoda na moduł, instalacja wtyczki, sekrety w Paperclip.

Na serwerze wykonuje to właściciel albo agent za jego zgodą.

## 8. Threat model

**Zaufane:**
- właściciel;
- kontener `open-mess` i jego wolumen;
- root serwera.

**Częściowo zaufane: kontener Paperclip.** To serwer Paperclip, proces i UI wtyczki oraz agenci z pełną powłoką. Ma oba tokeny, więc może wszystko, co API administracyjne i narzędzia:
- czytać rozmowy dozwolone do czytania;
- zmieniać uprawnienia;
- zatwierdzać akcje;
- logować i usuwać konta.

To decyzja właściciela (2.4). open_mess nadal gwarantuje:
- **Tokeny Teams nie opuszczają kontenera `open-mess`.** Żadna komenda ani narzędzie nie zwraca sekretów modułu, a wolumen nie jest podłączony do Paperclip. Wyniesienie tokenów wymaga przejęcia kontenera `open-mess` albo roota serwera.
- **Uprawnienia działają dla każdego wywołania narzędzia** (8.6): bez zmiany ustawień przez API administracyjne agent nie pisze tam, gdzie "nigdy", i nie pisze bez zatwierdzenia tam, gdzie "z potwierdzeniem".
- **Brak dostępu do komputera właściciela.**

Złośliwa wiadomość w Teams, która nakłoni agenta do użycia tokenu administracyjnego, nie jest tu zablokowana technicznie. Pozostaje ostrzeżenie w opisie narzędzi i w instrukcjach wtyczki, że treść od innych jest danymi, a nie poleceniami. Szczelniejszy wariant (zatwierdzanie poza Paperclip) jest opisany w rozdz. 10.

**Niezaufane:**
- treść wiadomości od innych osób: oznaczona jak dotąd (8.5);
- inne kontenery i procesy na serwerze: port nie jest publikowany na hoście, a w sieci `paperclip_default` są tylko Paperclip i open_mess; bez tokenu dostają 401 albo 404;
- sieć między serwerem a Microsoftem: TLS jak dotąd.

**Przepływ danych.** Treść wiadomości trafia do Teams oraz przez Paperclip do agentów i ich dostawców modeli (agenci, których użytkownik podłącza, AGENTS.md). Nigdzie indziej. Logi bez tokenów i treści (bez zmian).

**Ryzyko konta.** Ruch Teams z adresu serwera (centrum danych) może uruchomić alarmy ochrony tożsamości organizacji (2.3). To ryzyko przyjęte przez właściciela; open_mess robi tylko tyle zapytań, ile trzeba (bez zmian).

## 9. Dotknięte pliki (etap 1)

- `crates/open-mess/src/api.rs`, `daemon.rs` (adres, SIGTERM, token administracyjny), `secrets.rs` (`admin_api_token`, `install_platform_store`), `cli.rs` (`install_platform_store` przed `token()`, `om server-tokens`), `lib.rs` (błędy), nowy `crates/open-mess/src/file_store.rs`
- `crates/open-mess/src/tools/shared.rs` (opis `send`)
- `crates/open-mess/src/ui/commands.rs` i `ui/sign_in.rs` (`open_url`, kontrola hostów prośby `page`, sprawdzenie odpowiedzi `page`, logowanie w tle, `sign_in_result`)
- `crates/open-mess-host/src/secrets.rs` (`platform_store`)
- `crates/open-mess/tests/api.rs`, `tests/sign_in.rs`, `tests/daemon.rs`, nowy `tests/file_store.rs`
- `.specs/2026-10-26-open-mess-design.md`: w 8.1, 8.4 i 8.6 jedno zdanie z odesłaniem do tego specu (tryb serwerowy)

## 10. Poza zakresem

- **Zatwierdzanie poza Paperclip** (strona open_mess za tunelem SSH, z tokenem, którego Paperclip nie ma). Szczelniejsze, ale właściciel wybrał wszystko w Paperclip. Dodać, jeśli zmieni zdanie: wystarczy nie dawać wtyczce tokenu administracyjnego.
- Wyzwalacze tworzące zadania w Paperclip i powiadomienia o akcjach czekających (strona wtyczki pokazuje listę; powiadomienia później).
- Szyfrowanie `secrets.json`.
- Publikacja wtyczki w npm i obrazu w rejestrze.
- `get_attachment` zwracający treść pliku zamiast ścieżki w kontenerze `open-mess` (agent Paperclip tej ścieżki nie otworzy).

## 11. Rejestr rebutali

(pusty)

## Changelog rund

R1:
- CLI w kontenerze: token agentów to `OPEN_MESS_TOKEN`, zmienna już czytana przez CLI. CLI na Linuksie instaluje `FileStore` tą samą funkcją co demon (Codex P2, GLM P2).
- Prośba `page` sprawdzana `check_page` wobec zatwierdzonych hostów logowania, zanim pojawi się w `open_url`; niezatwierdzona kończy logowanie błędem (Codex P2, GLM P2).
- `FileStore` serializuje odczyt, zmianę i zapis jedną blokadą w procesie (GLM P3).
- Poprawny cytat błędu WAM poza Windows (GLM P3).
- Wtyczka bez `get_attachment`, wyjątek zapisany w teście zgodności z rejestrem (GLM P3).
- Adres nasłuchu na stderr zamiast nieistniejącego logu.

R2:
- `OPEN_MESS_LISTEN` przyjmuje tylko `0.0.0.0`, więc CLI przez `127.0.0.1` zawsze sięga demona (Codex P2, GLM P3).
- Tokeny generuje demon (32 losowe bajty) zamiast zmiennych środowiskowych z ręcznie wpisaną wartością. Właściciel odczytuje je poleceniem `om server-tokens` przez `docker exec` (Codex P2).
- SIGTERM kończy demona jak Ctrl+C (`docker stop`) (GLM P2).
- Na Windows wklejonego adresu dla prośby `page` dalej nie przyjmujemy; odpowiada tylko WebView.

R3 (GLM zatwierdził):
- Nazwy narzędzi wtyczki: dokładnie nazwy rejestru, przedrostek dokleja Paperclip; jawne mapowanie pól manifestu (Codex P2).
- `sign_in_start` w API administracyjnym: istniejąca komenda w zadaniu tokio, wynik w jednym miejscu dla `sign_in_result`; okno bez zmian (GLM P3).
- Test niezatwierdzonego hosta strony na poziomie filtra próśb, nie przez `test-echo` (GLM P3).

R4 (GLM zatwierdził):
- `sign_in_start` podzielone na krok synchroniczny (`begin`, w żądaniu) i asynchroniczny (w zadaniu), więc drugie równoczesne logowanie dostaje błąd od razu, a wynik nie nadpisuje trwającego (Codex P2).
- Strona wtyczki ma przegląd i zatwierdzanie modułów; API administracyjne ma `rescan_modules` (Codex P2, GLM P3).
- Limit `APPROVAL_TTL` bez powiadomień opisany w etapie 3 i w docs (GLM P3).
- Linia startowa podaje rzeczywisty adres nasłuchu (GLM P3).

R5 (GLM zatwierdził):
- `fsync` folderu po `rename` w `FileStore` (Codex P2).
- `stop_grace_period: 60s` w compose, dłuższy niż `CALL_TIMEOUT` (Codex P2).
- Opis `send` bez odesłania wyłącznie do powiadomienia Windows (Codex P3).
- Generowanie narzędzi wtyczki przez xtask; odmowa niezatwierdzonej strony to ta sama techniczna odmowa co na Windows, filtr na każdej platformie (GLM P3).
