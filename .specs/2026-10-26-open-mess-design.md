# open_mess - dokument projektowy

Data: 2026-10-26
Status: do akceptacji
Zakres szczegółowy: etap 1 (rdzeń + moduł Teams). Etapy 2 i 3 tylko jako kierunek.

## 1. Po co

Użytkownik korzysta z komunikatorów (Teams, Slack, Discord, w przyszłości inne) głównie przez agentów AI (Claude Code, Codex). Dziś ma do tego osobne, doklejone narzędzia: `discord-self` (Python, discord.py-self) i `teams-wrapper` (Node + ukryty Chromium z Teams Web). Problemy, które go irytują:

- trzeba wyciągać tokeny z cudzych aplikacji zamiast zalogować się normalnie, jak robi to oficjalny klient,
- każde narzędzie ma inną składnię poleceń i inny rdzeń,
- każde to kolejny proces w systemie,
- `teams-wrapper` ciągle się wylogowuje, a gdy działa, oficjalny Teams "świruje" (rozmowy z długą historią pokazują się puste), więc w praktyce nie jest używany.

open_mess ma to zastąpić jednym lekkim programem w Rust: jeden proces, wspólny rdzeń, platformy jako moduły, logowanie jak w oficjalnych aplikacjach, jedna składnia dla agentów (MCP) i dla człowieka (CLI).

## 2. Czym jest (w skrócie)

- **Demon** - jeden proces działający w tle od zalogowania do Windows. Trzyma sesje wszystkich kont, połączenia na żywo i moduły.
- **MCP** - główne wejście dla agentów. Serwowane przez demona (HTTP na localhost), więc wiele sesji Claude Code / Codex korzysta z jednego procesu.
- **CLI** - `om <polecenie>`, te same operacje co MCP, dla człowieka i skryptów.
- **Okno konfiguracji** - jedyny interfejs graficzny. Służy do logowania kont, uprawnień agentów, modułów, wyzwalaczy i podglądu stanu. Zamknięcie okna nie zatrzymuje demona.
- **Moduły WASM** - każda platforma (Teams, potem Discord, Slack, inne) i każda większa funkcja (w przyszłości spotkania) to osobny plik `.wasm` ładowany w trakcie działania, bez przebudowy programu.
- **Brak interfejsu do czatowania dla człowieka.** Wiadomości czytają i piszą agenci (MCP) albo człowiek przez CLI.

## 3. Etapy

1. **Wiadomości** (ten dokument): rdzeń, okno konfiguracji, CLI, MCP, wyzwalacze, moduł Teams. Następnie, w ramach tego samego rdzenia, moduły Discord (zastępuje `discord-self`) i Slack - każdy z nich dostanie krótką osobną rundę planowania protokołu, rdzeń się nie zmienia.
2. **Spotkania, na których użytkownik jest** (kierunek, sekcja 12).
3. **Agent zamiast użytkownika na spotkaniu** (kierunek, sekcja 12).

Kolejność platform w etapie 1: Teams (najbardziej potrzebny), potem Discord, potem Slack. Kolejność Discord/Slack - decyzja projektowa, nie użytkownika.

## 4. Cele etapu 1 i kryteria sukcesu

Etap 1 jest skończony, gdy:

1. Konto Teams loguje się z okna konfiguracji, bez ręcznego wyciągania tokenów, ciasteczek ani profili przeglądarki.
2. Sesja Teams utrzymuje się co najmniej 7 dni bez ręcznej interwencji (jeśli organizacja nie unieważni sesji). Gdy sesja naprawdę wygaśnie, okno i `om status` pokazują "zaloguj ponownie", a narzędzia zwracają czytelny błąd.
3. Przez cały ten czas oficjalny klient Teams działa normalnie: pełna historia rozmów, brak wylogowań, a czytanie przez agenta nie zmienia stanu "przeczytane" w oficjalnym kliencie.
4. Agent przez MCP (i człowiek przez CLI) ma wszystkie operacje z sekcji 8.5, działające na Teams.
5. Wyzwalacz "DM do mnie" dostarcza powiadomienie do otwartej sesji Claude Code; grupowanie i limity działają (sekcja 8.7).
6. Moduł Teams jest plikiem `.wasm`; dodanie drugiego modułu (choćby testowego) nie wymaga przebudowy `om.exe`.
7. Lekkość: demon z załadowanym modułem Teams i zamkniętym oknem zajmuje orientacyjnie poniżej 60 MB RAM i nie trzyma uruchomionego silnika przeglądarki. (Liczba orientacyjna - decyzja projektowa, do skorygowania po pomiarze.)

## 5. Poza zakresem etapu 1

- moduły Discord i Slack (następne w kolejce, ale nie w tym kawałku pracy),
- spotkania, transkrypcje, głos, wideo (etapy 2 i 3),
- statusy obecności, tworzenie zespołów i kanałów, zaproszenia, dołączanie do zespołów/serwerów, rozpoczynanie rozmów z nowymi osobami,
- interfejs do czatowania dla człowieka,
- Linux i macOS jako platforma produktu (nie wydajemy, nie testujemy okna, logowania WAM ani autostartu). CI uruchamia jednak testy rdzenia także na Linuksie, żeby rdzeń nie zależał od Windows,
- sklep / podpisywanie modułów od osób trzecich,
- oznaczanie wiadomości jako przeczytane (świadomie brak takiego narzędzia),
- webhooki wyzwalaczy: wysyłałyby treść wiadomości poza komputer użytkownika (AGENTS.md); jeśli kiedyś wrócą, to z osobnym specem i jawną zgodą na to, co opuszcza maszynę (recenzja R2).

## 6. Czego nie wolno (porażka, nawet jeśli reszta działa)

- open_mess w jakikolwiek sposób psuje oficjalnego klienta Teams (puste rozmowy, wylogowania, zmiana stanu przeczytania przez samo czytanie).
- Działanie wymaga wyciągania tokenów z innych aplikacji albo z przeglądarki.
- Platformy, konta albo połączenia działają w osobnych procesach, albo każda sesja agenta potrzebuje własnej kopii rdzenia. Jedyny wyjątek to cienki most stdio z 8.7 (tylko przekazuje komunikaty do demona, bez logiki platform), żyjący tyle co sesja Claude Code, która go uruchomiła.
- W tle stale działa silnik przeglądarki (jak Chromium w `teams-wrapper`).
- Agent wysyła wiadomość jako użytkownik wbrew ustawionym uprawnieniom (np. bez potwierdzenia, gdy wymagane jest potwierdzenie).
- Treść wiadomości od innych osób jest traktowana przez system jako polecenie.

## 7. Ograniczenia

- Język: Rust (decyzja użytkownika).
- Platforma docelowa: Windows 11 (maszyna użytkownika).
- Moduły dodawalne bez przebudowy, jako DLL albo WASM (decyzja użytkownika); wybrany WASM (sekcja 8.3).
- Konto Teams jest firmowe (tenant pracodawcy). Użytkownik akceptuje, że nieoficjalny klient może być niezgodny z zasadami firmy, a zasady dostępu warunkowego (Conditional Access) mogą blokować niektóre sposoby logowania.
- Każda wiadomość wychodzi jako użytkownik.

## 8. Architektura

### 8.1 Jeden proces

Jeden plik `om.exe`:

- `om daemon` - uruchamiany automatycznie przy logowaniu do Windows. Zawiera: środowisko modułów WASM, sesje kont, połączenia na żywo, lokalne API (MCP + API dla CLI), wyzwalacze, ikonę w zasobniku i okno konfiguracji otwierane na żądanie.
- `om <polecenie>` - krótko żyjące wywołanie CLI, które łączy się z działającym demonem przez lokalne API. Jeśli demon nie działa, CLI go uruchamia albo zwraca czytelny błąd (wybór budującego).

Lokalne API nasłuchuje wyłącznie na `127.0.0.1`, wymaga losowego tokenu Bearer (generowanego przy pierwszym uruchomieniu demona) i nigdy nie jest wystawiane w sieci.

### 8.2 Wejścia: MCP, CLI, okno

- **MCP**: Streamable HTTP pod `http://127.0.0.1:<port>/mcp`, jak w `teams-wrapper`. Jedna instancja obsługuje równolegle wiele sesji Claude Code i Codexa. Okno konfiguracji pokazuje gotowe polecenia `claude mcp add ...` i `codex mcp add ...`.
- **CLI**: polecenia generowane z tego samego rejestru narzędzi co MCP, więc nazwy, parametry i wyniki są identyczne. Wynik domyślnie czytelny dla człowieka, `--json` dla skryptów.
- **Okno konfiguracji**: sekcja 8.8.

### 8.3 Moduły WASM

Decyzja projektowa: WebAssembly Component Model (np. `wasmtime`), kontrakt opisany w WIT. Powody: stabilny kontrakt niezależny od wersji kompilatora, izolacja (moduł robi tylko to, na co pozwoli rdzeń), awaria modułu nie kładzie demona, moduły można pisać w dowolnym języku kompilowanym do WASM. Koszt: silnik WASM powiększa `om.exe` o około 10-15 MB. DLL odrzucone: brak stabilnego ABI w Rust, brak izolacji, awaria kładzie cały proces.

Podział odpowiedzialności:

- **Moduł** zna protokół platformy: jakie zapytania wysłać, jak zinterpretować odpowiedzi i zdarzenia, jak przebiega logowanie.
- **Rdzeń (host)** wykonuje wszystko, co dotyka świata zewnętrznego, i udostępnia to modułowi jako importy:
  - HTTP i WebSocket (z limitami tempa, ponawianiem po 429 / `Retry-After`, logowaniem bez sekretów),
  - lista dozwolonych hostów sieciowych per moduł: deklarowana w manifeście (dokładne nazwy albo wzorce sufiksu, np. `*.sharepoint.com` dla domeny SharePoint organizacji, nieznanej w chwili budowy modułu), pokazywana użytkownikowi do zatwierdzenia przy instalacji i przy każdej zmianie, egzekwowana przez rdzeń. Niezależnie od listy rdzeń twardo blokuje adresy pętli zwrotnej, link-local i prywatne (IPv4 i IPv6), sprawdzając adres po rozwiązaniu nazwy (ochrona przed DNS rebinding), więc żaden moduł nie dosięgnie usług lokalnych ani sieci LAN. Rdzeń łączy się dokładnie z adresem, który sprawdził (bez drugiego rozwiązywania nazwy), i nie podąża automatycznie za przekierowaniami HTTP: każdy krok przekierowania przechodzi tę samą kontrolę listy hostów i adresu, a limit kroków jest stały,
  - trwałe przechowywanie danych modułu (klucz-wartość) z limitem rozmiaru per moduł; zapis ponad limit zwraca błąd,
  - sekrety (tokeny odświeżania itp.) w Menedżerze poświadczeń Windows - moduł nigdy nie dostaje dostępu do sekretów innego modułu,
  - zegar i timery,
  - "prymitywy logowania" wyświetlane w oknie konfiguracji: pokaż kod urządzenia, pokaż kod QR, otwórz stronę logowania w osadzonym oknie przeglądarki i zwróć adres, na który nastąpiło przekierowanie, użyj konta Windows (WAM) - szczegóły w sekcji 9.1,
  - emisja zdarzeń (nowa wiadomość, zmiana stanu sesji) do rdzenia, który obsługuje wyzwalacze.
- **Moduł eksportuje**: manifest (nazwa, wersja, platforma, dozwolone hosty, obsługiwane operacje, dodatkowe narzędzia), logowanie i przywracanie sesji, operacje ze wspólnego zestawu (8.5), obsługę zdarzeń na żywo.
- **Dodatkowe narzędzia modułu**: moduł może zadeklarować własne narzędzia spoza wspólnego zestawu (np. w przyszłości `teams.list_meetings`); rdzeń wystawia je w MCP i CLI z przedrostkiem modułu. To jest furtka dla modułów funkcji (spotkania) z etapów 2-3.

Limity zasobów (decyzja projektowa, recenzja R1): każde wywołanie modułu ma limit czasu wykonania (przerywanie przez epoch/fuel silnika WASM) i limit pamięci; przekroczenie przerywa wywołanie, moduł jest wyładowywany, a stan i błąd pokazują `status` i okno. Po kilku awariach z rzędu rdzeń nie ładuje modułu ponownie automatycznie. Inne moduły i konta działają dalej. Konkretne wartości limitów - wybór budującego.

Instalacja modułu: plik `.wasm` w katalogu modułów (`%APPDATA%\open_mess\modules\`) albo "Dodaj z pliku" w sekcji Moduły okna. Moduły można włączać i wyłączać bez restartu demona.

Model asynchroniczny wywołań między rdzeniem a modułem (natywne async komponentów vs. model zdarzeń/odpytywania) - wybór budującego.

### 8.4 Dane i sekrety

- Sekrety (tokeny odświeżania, token lokalnego API): Menedżer poświadczeń Windows.
- Stan (konfiguracja, kursory rozmów, identyfikatory widzianych wiadomości dla wyzwalaczy, przygotowane wiadomości oczekujące na zatwierdzenie, stan wyzwalaczy): SQLite w `%APPDATA%\open_mess\`.
- Treści wiadomości nie są archiwizowane lokalnie; źródłem prawdy jest serwer platformy. Krótkotrwała pamięć podręczna jest dozwolona, jeśli budujący jej potrzebuje.
- Logi nie zawierają tokenów, ciasteczek ani treści wiadomości.

### 8.5 Wspólny zestaw operacji (jedna składnia)

Te same narzędzia dla każdej platformy. Nazwy poniżej są robocze; ostateczne nazewnictwo i kształt parametrów - wybór budującego, byle jedno dla MCP i CLI.

Czytanie:
- `accounts` / `whoami` - konta i ich stan,
- `list_conversations` - DM, czaty grupowe, zespoły i kanały, z filtrem po typie i nazwie,
- `unread` - to, czego użytkownik nie przeczytał w oficjalnym kliencie (stan "nieprzeczytane" platformy, tak jak `unread` w `discord-self`). Moduł może czytać znaczniki przeczytania, nigdy ich nie zapisuje. Do "co nowego od chwili X" służy `read_messages` z parametrem `since`. Decyzja projektowa (pytanie z przeglądu dokumentu),
- `mentions` - wzmianki o użytkowniku,
- `read_messages` - historia rozmowy / kanału / wątku, ze stronicowaniem,
- `search` - wyszukiwanie wiadomości,
- `get_attachment` - pobranie załącznika (zrzut ekranu, PDF) do pliku lokalnego, żeby agent mógł go przeczytać. Ścieżkę wybiera rdzeń: plik trafia do zarządzanego katalogu (`%APPDATA%\open_messttachments\`) pod oczyszczoną nazwą, nigdy nie nadpisuje istniejących plików, a narzędzie zwraca ścieżkę; agent nie podaje ścieżki docelowej (recenzja R2),
- `writing_context` - przykłady wiadomości napisanych przez użytkownika w danej rozmowie, żeby agent pisał jego stylem (przeniesione z `teams-wrapper`, łącznie z filtrami: tylko wiadomości użytkownika, bez sekretów, kodu, samych linków, danych operacyjnych).

Pisanie:
- `prepare_message` -> jednorazowy token przygotowania (wygasa po kilku minutach),
- `send_prepared` - wysłanie przygotowanej wiadomości,
- `send` - wysłanie od razu, dostępne tylko tam, gdzie uprawnienia pozwalają agentowi pisać samodzielnie,
- `edit_message`, `react`,
- odpowiedź w wątku i odpowiedź na konkretną wiadomość jako parametry `prepare_message` / `send`.

Stan:
- `status` - stan demona, modułów, sesji.

Czytanie nigdy nie oznacza wiadomości jako przeczytanych.

Wyniki narzędzi czytających (`unread`, `mentions`, `read_messages`, `search`, `list_conversations`, `get_attachment`) oddzielają treść pochodzącą od innych osób (tekst, nazwy, nazwy plików) od metadanych i oznaczają ją jako niezaufaną, tak samo jak powiadomienia wyzwalaczy (AGENTS.md).

Identyfikatory rozmów i wiadomości są nieprzezroczyste dla agenta i jednoznacznie wskazują platformę i konto. Treść wiadomości dla agenta: zwykły tekst / Markdown (konwersja z HTML Teams i z powrotem po stronie modułu).

### 8.6 Uprawnienia agentów

Ustawiane w sekcji "Uprawnienia agentów" okna, per konto, opcjonalnie zawężone do wybranych rozmów:

- czytanie: tak / nie,
- pisanie: nigdy / z potwierdzeniem / samodzielnie.

**Samodzielne wysyłanie jest opcją do wyboru** (decyzja użytkownika): agent może wysyłać bez potwierdzenia tylko tam, gdzie użytkownik świadomie ustawił "samodzielnie" - per konto, opcjonalnie zawężone do wybranych rozmów. Nowe konto startuje z "z potwierdzeniem" (decyzja projektowa).

Kto ma jakie uprawnienia (decyzja projektowa, recenzje R1 i R2): **jedne zasady dla każdego wywołującego.** Rdzeń egzekwuje wyłącznie uprawnienia konta/rozmowy, tak samo dla MCP, CLI, sesji obudzonej powiadomieniem i agenta uruchomionego przez wyzwalacz. Nie ma tokenów zakresowych ani uprawnień per reguła: każdy proces na koncie użytkownika może wywołać CLI ze wspólnym tokenem, więc węższe uprawnienia dla jednego wywołującego dałoby się obejść zwykłym `om send` z narzędzia Bash. Kto nie chce, żeby agent obudzony przez wyzwalacz wysyłał bez zatwierdzenia, ustawia dla konta albo danej rozmowy "z potwierdzeniem". Okno konfiguracji ostrzega przy zapisie reguły wyzwalacza, która obejmuje konto albo rozmowę z "samodzielnie".

"Z potwierdzeniem" oznacza, że działa tylko ścieżka `prepare_message` -> `send_prepared`, a `send_prepared` przechodzi dopiero po zatwierdzeniu **poza zasięgiem agenta**: powiadomienie Windows z dokładną rozmową, odbiorcami i tekstem oraz przyciskami Wyślij / Odrzuć, a także lista "Do zatwierdzenia" w oknie konfiguracji. Zgoda wyrażona w czacie z agentem nie wystarcza, bo agent mógłby ją zmyślić albo wywołać `send_prepared` sam. Dotyczy to agentów w sesji interaktywnej, agentów uruchomionych przez wyzwalacz i CLI. Decyzja projektowa.

CLI nie ma żadnych dodatkowych przywilejów: podlega tym samym uprawnieniom i tej samej ścieżce zatwierdzania co MCP. Demon nie umie wiarygodnie odróżnić człowieka w terminalu od agenta, który sam otwiera konsolę (np. `Start-Process` z narzędzia Bash), więc rozróżnianie po terminalu byłoby obejściem uprawnień. Człowiek wysyłający z CLI na koncie "z potwierdzeniem" klika Wyślij w powiadomieniu tak jak przy agencie; kto chce wysyłać bez tego, ustawia "samodzielnie". Decyzja projektowa (recenzja Codex i GLM commita d82e768 odrzuciła wcześniejszą regułę "CLI w interaktywnym terminalu = użytkownik").

Rdzeń egzekwuje uprawnienia - nie polega na tym, że agent się do nich zastosuje.

### 8.7 Wyzwalacze (budzenie agenta)

Sekcja "Wyzwalacze" w oknie. Reguła = warunek + akcja.

Warunki: DM do użytkownika, wzmianka, nowa wiadomość w wybranej rozmowie, wiadomość od wybranych osób; zawsze w ramach konkretnego konta.

Akcje:
1. **Wpis do otwartej sesji Claude Code** przez kanał MCP (`notifications/claude/channel`, z deklaracją możliwości `experimental: { "claude/channel": {} }` - mechanizm używany przez wtyczkę Telegram, potwierdzony dla transportu stdio). Jeśli kanał nie działa przez transport HTTP, budujący dodaje cienki most stdio (`om mcp-stdio`), który tylko przekazuje komunikaty do demona. Claude Code sam uruchamia most jako swój serwer MCP (stdio) i kończy go razem z sesją; most niczego nie uruchamia, tylko przekazuje komunikaty - po jednym na sesję z włączonymi kanałami. To jedyny wyjątek od zasady jednego procesu (sekcja 6): bez logiki platform, bez sesji kont, bez własnego stanu. Claude Code dostarcza kanały tylko do sesji uruchomionych z włączonymi kanałami (flaga przy starcie; dla serwera spoza listy zatwierdzonych wtyczek prawdopodobnie tryb deweloperski) - to akceptowalne, okno konfiguracji pokazuje gotowe polecenie startu. Gdy takich sesji jest kilka, powiadomienie dostaje ostatnio aktywna (bez duplikatów); gdy nie ma żadnej, działa akcja 2, jeśli reguła ją ma. Decyzja projektowa (pytanie z przeglądu dokumentu).
2. **Uruchomienie agenta od zera** gdy nie ma sesji: demon uruchamia skonfigurowany program (np. `claude -p` albo Codex) ze stałą listą argumentów ustaloną w regule, bezpośrednio, bez powłoki. Treść zdarzenia nigdy nie jest wklejana do polecenia ani argumentów: trafia do programu przez stdin albo jako ścieżka do pliku z treścią, oznaczoną jako niezaufana. Proces korzysta z MCP/CLI na tych samych zasadach co każdy inny wywołujący (8.6). Decyzja projektowa (recenzja R1).

Zabezpieczenia (decyzje projektowe):
- grupowanie: zdarzenia z jednej rozmowy są zbierane i wysyłane jako jedno powiadomienie po chwili ciszy,
- cisza nocna i limit pobudek na godzinę per reguła,
- treść od innych osób jest przekazywana jako dane, wyraźnie oznaczona jako niezaufana; agent obudzony albo uruchomiony przez wyzwalacz ma uprawnienia konta/rozmowy (8.6), a okno ostrzega przy regułach obejmujących "samodzielnie".

### 8.8 Okno konfiguracji

Układ wybrany przez użytkownika: klasyczne ustawienia z bocznym menu (sekcje po lewej, szczegóły po prawej). Zamknięcie okna nie zatrzymuje demona (potwierdzone przez użytkownika); okno otwiera się z ikony w zasobniku albo `om config`.

Sekcje:
- **Konta** - lista kont ze stanem połączenia, "Dodaj konto", "Zaloguj ponownie", usuwanie konta,
- **Uprawnienia agentów** - sekcja 8.6,
- **Wyzwalacze** - sekcja 8.7,
- **Do zatwierdzenia** - przygotowane wiadomości czekające na decyzję (może być częścią innej sekcji, wybór budującego),
- **Moduły** - zainstalowane moduły, włącz / wyłącz, dodaj z pliku,
- **Integracje (MCP / CLI)** - gotowe polecenia do podłączenia MCP, ścieżka do CLI,
- **Demon i logi** - stan, pamięć, połączenia, ostatnie błędy.

Szkic układu (lokalny, poza repo): wariant A z sesji projektowej - boczne menu z sekcjami, po prawej lista kont z ikoną platformy, nazwą konta, opisem, stanem połączenia (kropka + tekst) i menu akcji; u dołu menu stan demona (pamięć, liczba połączeń).

Technologia okna - decyzja projektowa: Tauri 2 (WebView2, wbudowany w Windows 11). Powody: logowanie Microsoftu i tak wymaga osadzonej przeglądarki (sekcja 9.1), HTML/CSS ułatwia "ładny layout", a WebView2 działa tylko gdy okno jest otwarte, więc nie łamie zakazu stale działającego silnika przeglądarki. Budujący może wybrać inną technologię, jeśli zachowa te trzy własności.

## 9. Moduł Teams

Wszystkie konkretne identyfikatory i adresy poniżej pochodzą z publicznej wiedzy o nieoficjalnym API Teams i **muszą zostać potwierdzone przez REA** na aktualnym kliencie przed implementacją.

### 9.1 Logowanie

Moduł loguje się tak jak oficjalny klient: logowanie Microsoft (Entra ID) z identyfikatorem aplikacji Teams, a potem sam odświeża tokeny. Znane identyfikatory: Teams desktop/mobile `1fec8e78-bce4-4aaf-ab1b-5451cc387264`, Teams web `5e3ce6c0-2b1f-4285-8d4b-75ee78787346` (do potwierdzenia).

Sposoby logowania, w kolejności preferencji (pierwsze zadanie implementacji to próba wszystkich na koncie użytkownika, bo zasady firmy mogą część blokować):
1. **Konto Windows (WAM)** - logowanie bez hasła kontem, którym użytkownik jest zalogowany do Windows; tak robi nowy Teams.
2. **Osadzona strona logowania** - kod autoryzacji + PKCE w oknie WebView2, rdzeń przechwytuje przekierowanie.
3. **Kod urządzenia** - użytkownik wpisuje kod na stronie Microsoftu.

Po zalogowaniu (decyzja projektowa, recenzja R2): przy logowaniu przez WAM aplikacja nie dostaje tokenu odświeżania - sesję trzyma broker Windows, a odnowienie to ciche pobranie tokenu przez WAM; przy osadzonej stronie logowania i kodzie urządzenia token odświeżania trafia do sekretów. W obu przypadkach tokeny dostępu (m.in. dla API czatów, wyszukiwania) odświeżane z wyprzedzeniem, przed wygaśnięciem. Błąd 401 -> jedna próba odświeżenia -> dopiero potem stan "zaloguj ponownie". Hasło użytkownika nigdy nie przechodzi przez open_mess poza stroną logowania Microsoftu.

### 9.2 Współistnienie z oficjalnym Teams

Wymaganie nadrzędne (sekcja 6). open_mess zachowuje się jak osobne urządzenie, np. telefon obok komputera:
- własna sesja i własna rejestracja urządzenia / punktu końcowego z własnym, raz wygenerowanym i zapamiętanym identyfikatorem; nigdy nie używa identyfikatorów oficjalnego klienta,
- nie czyta ani nie zapisuje niczego w danych oficjalnego klienta (`%LOCALAPPDATA%\Packages\MSTeams_8wekyb3d8bbwe\` i podobne),
- nie wywołuje operacji potwierdzenia odczytu (`consumptionhorizon`, `markRead`, `readReceipt` i ich odpowiedników),
- szanuje limity serwera (tempo zapytań, `Retry-After`), żeby nie powodować ograniczania konta,
- test akceptacyjny: tydzień równoległej pracy z oficjalnym klientem bez objawów z sekcji 6.

### 9.3 Wiadomości, zdarzenia, wyszukiwanie

Do potwierdzenia przez REA:
- wymiana tokenu Entra ID na token usługi czatów (authsvc), z którego wynika też regionalny adres usługi czatów,
- usługa czatów (`.../v1/users/ME/conversations`, `.../messages`) - lista rozmów, historia, wysyłka, edycja, reakcje,
- zdarzenia na żywo przez Trouter (WebSocket); odpytywanie jako zapas, gdy połączenie na żywo zawiedzie,
- wyszukiwanie przez usługę wyszukiwania Microsoft 365 (substrate),
- załączniki: obrazy w wiadomościach (usługa mediów Teams) i pliki w OneDrive/SharePoint (wymaga tokenu dla domeny SharePoint organizacji).

Gdy odpowiedź serwera ma nieznany kształt, operacja kończy się jawnym błędem "niezgodny protokół" i stan modułu to pokazuje - bez zgadywania pól i bez wysyłania częściowo zbudowanych wiadomości (wzorzec z `teams-wrapper`).

### 9.4 Rola REA

REA (MCP do inżynierii wstecznej) służy do ustalenia rzeczywistego protokołu: analiza paczki aplikacji Teams Web (`teams.microsoft.com/v2`) i nowego klienta desktop (pakiet MSTeams, WebView2), przechwycenie przepływu logowania, wymiany tokenów, wywołań usługi czatów, Trouter i wyszukiwania na koncie testowym lub na koncie użytkownika w osobnym profilu. Wyniki zapisywane jako dokument protokołu w repo - bez tokenów, ciasteczek, identyfikatorów tenantu i treści wiadomości (wzorzec `docs/protocol-capture.md` z `teams-wrapper`).

### 9.5 Testy na koncie firmowym

Użytkownik nie ma konta ani organizacji testowej Microsoft 365 - wszystko odbywa się na koncie firmowym (decyzja użytkownika). Zasady:
- **Próba logowania tylko przy obecności użytkownika.** Kolejność 9.1 jest jednocześnie kolejnością od najmniej podejrzanego: najpierw WAM, potem osadzona strona logowania. Kod urządzenia z identyfikatorem aplikacji Teams to znany wzorzec phishingu i może uruchomić alarm bezpieczeństwa albo zablokować konto - próbujemy go tylko wtedy, gdy dwa pierwsze sposoby zawiodą, i tylko za wyraźną zgodą użytkownika w danej chwili.
- **Testy zapisu na żywo tylko w czacie z samym sobą** (notatki dla siebie). Wysyłka, edycja i reakcje nigdy nie dotykają rozmów z innymi osobami podczas testów.
- **Testy na żywo nie działają automatycznie:** są oznaczone `#[ignore]`, wymagają jawnej zmiennej środowiskowej i obecności użytkownika. CI ich nigdy nie uruchamia.
- **Mały ruch:** testy i przechwytywanie protokołu robią tylko tyle zapytań, ile trzeba, żeby nie wyglądać jak atak i nie powodować ograniczania konta.
- **Przechwyty protokołu** (REA) z konta firmowego są czyszczone z tokenów, ciasteczek, identyfikatorów tenantu i użytkowników oraz treści wiadomości, zanim cokolwiek trafi do repo.

## 10. Wnioski z `teams-wrapper` (`C:\Projekty\sych.pl\teams_wraper`)

Przyczyny problemów (analiza kodu, 2026-10-26):
- **Wylogowania** (prawie pewne): wrapper uruchamia ukryty Chromium z pełnym Teams Web i podsłuchuje z niego nagłówki z tokenem. Sam tokenu nie odświeża - polega na tym, że ukryta strona wyśle kolejne zapytanie. Po wygaśnięciu tokenu (około godziny) dostaje 401 i przechodzi w "auth_required".
- **Puste rozmowy w oficjalnym Teams** (niepotwierdzone, dwie hipotezy): (a) drugi pełny klient Teams działający cały czas w tle z zablokowanymi zapytaniami o potwierdzenia odczytu; (b) skrypt skilla `log-time` (`teams-peer-resolver.ps1`) kopiuje pliki IndexedDB oficjalnego Teams w trakcie jego pracy - otwarty uchwyt może uniemożliwić Teams porządkowanie bazy i doprowadzić do uznania pamięci podręcznej za uszkodzoną. Hipoteza (b) dotyczy narzędzia poza open_mess i warto ją sprawdzić niezależnie.

Do przeniesienia: dwuetapowa wysyłka, czytanie bez oznaczania jako przeczytane, `writing_context` z filtrami, MCP przez HTTP w jednym procesie dla Claude i Codexa, jawny błąd przy niezgodnym protokole. Webhook z `teams-wrapper` świadomie nie przechodzi do etapu 1 (sekcja 5).

Do uniknięcia: przeglądarka w tle, podsłuchiwanie tokenów, dotykanie danych oficjalnego klienta.

## 11. Ryzyka i niewiadome

1. **Zasady dostępu warunkowego** firmy mogą blokować logowanie kodem urządzenia, wymagać urządzenia zgodnego z zasadami albo blokować nieznane aplikacje. Dlatego pierwsze zadanie to próba logowania (9.1). Jeśli wszystkie trzy sposoby zawiodą, etap 1 dla Teams jest zablokowany i wracamy do użytkownika.
2. **Zgodność z zasadami firmy i warunkami Microsoftu** - zaakceptowane przez użytkownika.
3. **Zmiany prywatnego protokołu Teams** bez ostrzeżenia - łagodzone jawnym stanem "niezgodny protokół" i dokumentem protokołu.
4. **Async w komponentach WASM** - dojrzałość natywnego async w Component Model; budujący wybiera model.
5. **Kanał MCP przez HTTP** - niepotwierdzone; zapasem jest most stdio (8.7).

## 12. Kierunek etapów 2 i 3 (tylko po to, żeby rdzeń ich nie blokował)

Etap 2 - spotkania, na których użytkownik jest:
- lista spotkań z kalendarza i linki,
- pobieranie transkrypcji, które zrobił Teams (gdy ktoś włączył transkrypcję); podsumowanie robi agent, open_mess go nie generuje,
- lokalna transkrypcja dźwięku z komputera (np. Whisper) dla dowolnej platformy; kwestia zgody uczestników (RODO) do rozstrzygnięcia w tamtym etapie.

Etap 3 - agent zamiast użytkownika:
- samodzielne dołączanie do spotkań: najpierw Discord (znany protokół głosowy, ale nowe szyfrowanie end-to-end do sprawdzenia), potem Teams (zamknięty stos audio/wideo, bardzo trudne, wymaga REA),
- słuchanie na żywo i odpowiadanie głosem przez szybki model, gdy ktoś zwróci się do użytkownika,
- zalecenie: agent przedstawia się jako asystent użytkownika, nie udaje go.

Co z tego wynika dla etapu 1: moduły mogą deklarować własne narzędzia (8.3); kontrakt hosta da się rozszerzyć o nowe importy (np. strumienie audio) w nowej wersji WIT bez psucia istniejących modułów. Nic poza tym nie budujemy na zapas. Szybka ścieżka audio poza WASM może okazać się potrzebna w etapie 3 - decyzja wtedy.

Discord (etap 1, po Teams) będzie wymagał od rdzenia klienta HTTP/WebSocket podszywającego się pod przeglądarkę (odcisk TLS jak Chrome), tak jak dziś `discord-self` - to uwaga na przyszłość, nie wymaganie dla wersji z samym Teams.

## 13. Kto co zdecydował

Decyzje użytkownika:
- Rust; jeden lekki program dla wielu komunikatorów; maksymalnie modułowy,
- sterowanie przez CLI i MCP; najważniejsze funkcje dostępne dla agentów,
- logowanie jak w normalnych aplikacjach, bez wyciągania tokenów; jedna składnia; jeden proces,
- okno służy tylko do konfiguracji; układ A; zamknięcie okna nie zatrzymuje reszty,
- moduły dodawalne bez przebudowy, jako DLL albo WASM,
- zaczynamy od Teams; inne komunikatory później jako moduły,
- brak konta testowego: wszystko na koncie firmowym (zasady w 9.5),
- kolejność etapów 1 -> 2 -> 3; szczegółowo planujemy tylko etap 1,
- opcja automatycznego budzenia agenta, gdy ktoś napisze,
- agent może wysyłać samodzielnie, ale tylko gdy użytkownik to wybierze,
- nic nie może psuć oficjalnego Teams; sesja ma się trzymać.

Decyzje projektowe (moje, do sprawdzenia przy czytaniu):
- WASM (Component Model) zamiast DLL,
- MCP przez HTTP na localhost w demonie (jak `teams-wrapper`), CLI przez to samo lokalne API,
- Tauri 2 / WebView2 dla okna konfiguracji,
- kolejność po Teams: Discord, potem Slack,
- trzy poziomy pisania (nigdy / z potwierdzeniem / samodzielnie), nowe konto startuje z "z potwierdzeniem",
- zatwierdzanie wysyłki tylko poza zasięgiem agenta (powiadomienie Windows, lista "Do zatwierdzenia"), nigdy samą zgodą w czacie; CLI podlega tym samym uprawnieniom co MCP,
- grupowanie, cisza nocna i limity wyzwalaczy; domyślnie obudzony agent nie wysyła bez zatwierdzenia,
- sekrety w Menedżerze poświadczeń Windows, stan w SQLite, brak lokalnego archiwum wiadomości,
- brak narzędzia do oznaczania jako przeczytane,
- jedne uprawnienia (konta/rozmowy) dla każdego wywołującego, bez tokenów zakresowych i uprawnień per reguła; brak webhooków w etapie 1 (8.6, 5),
- limity czasu i pamięci dla modułów WASM; lista hostów modułu zatwierdzana przez użytkownika, adresy lokalne i prywatne zawsze zablokowane (8.3),
- docelowe zużycie pamięci poniżej ~60 MB,
- nazwa pliku wykonywalnego `om.exe`, dane w `%APPDATA%\open_mess\`,
- `unread` = stan nieprzeczytania platformy, tylko do odczytu (8.5),
- powiadomienie z wyzwalacza trafia do ostatnio aktywnej sesji z włączonymi kanałami, bez duplikatów (8.7),
- repozytorium na wzór SolidCraft: dokumenty repo po angielsku, licencja MIT OR Apache-2.0, specy w `.specs/`, każdy spec i każdy commit recenzowany przez Codex i GLM (`AGENTS.md`).

## 14. Zostawione budującemu

- dokładne nazwy i parametry narzędzi (byle jedne dla MCP i CLI),
- kształt WIT i model async rdzeń-moduł,
- struktura repo (workspace Cargo, podział na crate'y), biblioteki (HTTP, WebSocket, SQLite, zasobnik),
- sposób autostartu demona i zachowanie CLI, gdy demon nie działa,
- format wyników CLI,
- konwersja HTML <-> Markdown dla Teams,
- strategia testów (zalecane: dekodery protokołu testowane na nagranych i oczyszczonych odpowiedziach, testowy moduł WASM do testów kontraktu, tygodniowy test współistnienia z oficjalnym Teams).

## 15. Threat model

Zaufani:
- użytkownik przy komputerze: okno konfiguracji i przyciski w powiadomieniach Windows (CLI nie jest kanałem zaufanym - może go wywołać agent),
- rdzeń open_mess i jego zapis stanu w `%APPDATA%\open_mess\`.

Częściowo zaufani - agenci (Claude Code, Codex, agenci z wyzwalaczy): działają w dobrej wierze, ale mogą się mylić albo zostać zmanipulowani treścią wiadomości (prompt injection). Dlatego rdzeń sam egzekwuje uprawnienia (8.6), a wysyłka "z potwierdzeniem" wymaga zatwierdzenia poza ich zasięgiem.

Niezaufane:
- treść wiadomości, nazwy, załączniki i wszystko inne pochodzące od innych osób,
- odpowiedzi serwerów platform i zdarzenia WebSocket (mogą mieć nieznany albo zepsuty kształt),
- moduły WASM spoza repo (izolacja, lista dozwolonych hostów, brak dostępu do cudzych sekretów),

Granica twardnienia (zamrożona): nie bronimy się przed złośliwym kodem działającym na koncie Windows użytkownika z zamiarem obejścia open_mess - taki kod może i tak czytać Menedżer poświadczeń, symulować terminal albo klikać w powiadomienia. Bronimy się przed agentem, który przez pomyłkę albo manipulację próbuje zrobić więcej, niż pozwalają uprawnienia, używając zwykłych narzędzi (MCP, CLI z narzędzia Bash).

## 16. Rejestr rebutali

Brak odrzuconych zgłoszeń: w R1 i R2 wszystkie zgłoszenia przyjęto.

## Changelog rund

- R1 (Codex REJECT, GLM REJECT): reguły wyzwalaczy tylko zawężają uprawnienia, tokeny zakresowe dla uruchomień z akcji 2, akcja 1 dziedziczy uprawnienia konta z ostrzeżeniem (Codex P1, GLM P2); limity czasu i pamięci modułów WASM (Codex P2); most stdio jako jawny, długożyjący wyjątek od zasady jednego procesu (Codex P2); akcja 2 bez powłoki i bez wklejania treści do polecenia (GLM P2); lista hostów modułu zatwierdzana przez użytkownika, wzorce sufiksu, twarda blokada adresów lokalnych i prywatnych (GLM P2, P3); oznaczanie niezaufanej treści w wynikach narzędzi (GLM P3); doprecyzowanie Linuksa w CI (GLM P3); token API przy pierwszym uruchomieniu (GLM P3).
- R2 (Codex REJECT, GLM REJECT): usunięte tokeny zakresowe i uprawnienia per reguła - wszyscy wywołujący mają uprawnienia konta/rozmowy, bo agent może obejść węższe uprawnienia przez `om send` (Codex P1); WAM bez tokenu odświeżania, odnowienie przez broker (Codex P2); przypięcie połączenia do sprawdzonego adresu i kontrola każdego przekierowania (GLM P2); `get_attachment` zapisuje tylko do zarządzanego katalogu (GLM P2); webhooki usunięte z etapu 1, bo wysyłałyby treść poza maszynę (GLM P2); limit magazynu modułu, cykl życia mostu stdio, `get_attachment` w oznaczaniu niezaufanej treści, rejestr rebutali (GLM P3).
