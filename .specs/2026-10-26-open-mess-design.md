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
- webhooki wyzwalaczy: wysyłałyby treść wiadomości do dowolnych adresów poza platformą i agentami, które użytkownik podłączył (AGENTS.md); jeśli kiedyś wrócą, to z osobnym specem i jawną zgodą na to, co i dokąd wychodzi (recenzje R2 i R5).

Przepływ treści (recenzja R5): treść wiadomości trafia wyłącznie do platformy i do agentów podłączonych przez użytkownika (MCP, CLI, akcje wyzwalaczy), a przez nich do dostawców ich modeli - to jest cel open_mess i użytkownik godzi się na to, podłączając agenta albo tworząc regułę z akcją 2. open_mess sam nie wysyła treści nigdzie indziej.

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

- `om daemon` - uruchamiany automatycznie przy logowaniu do Windows. Zawiera: środowisko modułów WASM, sesje kont, połączenia na żywo, lokalne API (MCP + API dla CLI), wyzwalacze, ikonę w zasobniku i okno konfiguracji otwierane na żądanie. Zawsze działa co najwyżej jedna instancja demona na użytkownika Windows (blokada pojedynczej instancji, np. nazwany mutex): drugie uruchomienie kończy się komunikatem, że demon już działa, a równoczesne uruchomienia z CLI nie tworzą drugiej instancji (recenzja R4).
- `om <polecenie>` - krótko żyjące wywołanie CLI, które łączy się z działającym demonem przez lokalne API. Jeśli demon nie działa, CLI go uruchamia albo zwraca czytelny błąd (wybór budującego).

Lokalne API nasłuchuje wyłącznie na `127.0.0.1`, wymaga losowego tokenu Bearer (generowanego przy pierwszym uruchomieniu demona) i nigdy nie jest wystawiane w sieci. Port jest stały (domyślna wartość ustalona przez budującego, zmienialna w oknie) i zapisany w konfiguracji, żeby adres `http://127.0.0.1:<port>/mcp` dodany do Claude Code i Codexa działał po każdym restarcie; zajęty port to błąd widoczny w oknie i `om status`, nie losowanie nowego (recenzja R6).

### 8.2 Wejścia: MCP, CLI, okno

- **MCP**: Streamable HTTP pod `http://127.0.0.1:<port>/mcp`, jak w `teams-wrapper`. Jedna instancja obsługuje równolegle wiele sesji Claude Code i Codexa. Okno konfiguracji pokazuje gotowe polecenia `claude mcp add ...` i `codex mcp add ...`.
- **CLI**: polecenia generowane z tego samego rejestru narzędzi co MCP, więc nazwy, parametry i wyniki są identyczne. Wynik domyślnie czytelny dla człowieka, `--json` dla skryptów.
- **Okno konfiguracji**: sekcja 8.8.

### 8.3 Moduły WASM

Decyzja projektowa: WebAssembly Component Model (np. `wasmtime`), kontrakt opisany w WIT. Powody: stabilny kontrakt niezależny od wersji kompilatora, izolacja (moduł robi tylko to, na co pozwoli rdzeń), awaria modułu nie kładzie demona, moduły można pisać w dowolnym języku kompilowanym do WASM. Koszt: silnik WASM powiększa `om.exe` o około 10-15 MB. DLL odrzucone: brak stabilnego ABI w Rust, brak izolacji, awaria kładzie cały proces.

Podział odpowiedzialności:

- **Moduł** zna protokół platformy: jakie zapytania wysłać, jak zinterpretować odpowiedzi i zdarzenia, jak przebiega logowanie.
- **Rdzeń (host)** wykonuje wszystko, co dotyka świata zewnętrznego, i udostępnia to modułowi jako importy:
  - HTTP i WebSocket (z limitami tempa, ponawianiem po 429 / `Retry-After`, logowaniem bez sekretów). Rdzeń używa systemowych ustawień proxy Windows (w tym PAC/WPAD) i systemowego magazynu zaufanych certyfikatów, żeby działać w sieciach firmowych z wymuszonym proxy i inspekcją TLS. Przy połączeniu przez proxy lista hostów i blokada adresów prywatnych dotyczą hosta docelowego (rozwiązanego przez rdzeń przed wysłaniem do proxy); sam adres proxy, ustawiony przez system albo organizację, jest dozwolony, a przypięcie adresu z punktu niżej działa tylko przy połączeniach bezpośrednich (recenzja R5),
  - lista dozwolonych hostów sieciowych per moduł: deklarowana w manifeście (dokładne nazwy albo wzorce sufiksu, np. `*.sharepoint.com` dla domeny SharePoint organizacji, nieznanej w chwili budowy modułu; wzorzec musi leżeć poniżej domeny rejestrowalnej według listy sufiksów publicznych, więc `*.com` albo `*.co.uk` są odrzucane - recenzja R7), pokazywana użytkownikowi do zatwierdzenia przy instalacji i przy każdej zmianie, egzekwowana przez rdzeń. Niezależnie od listy rdzeń dopuszcza wyłącznie globalnie routowalne adresy unicast: blokuje pętlę zwrotną, link-local, RFC 1918, CGNAT `100.64.0.0/10` (np. Tailscale), unikalne lokalne IPv6 `fc00::/7`, adresy nieokreślone, rozgłoszeniowe i multicast oraz pozostałe zakresy specjalnego przeznaczenia z rejestru IANA, także w postaci IPv6 mapowanej z IPv4 (recenzja R9), sprawdzając adres po rozwiązaniu nazwy (ochrona przed DNS rebinding), więc żaden moduł nie dosięgnie usług lokalnych ani sieci LAN. Rdzeń łączy się dokładnie z adresem, który sprawdził (bez drugiego rozwiązywania nazwy), i nie podąża automatycznie za przekierowaniami HTTP: każdy krok przekierowania przechodzi tę samą kontrolę listy hostów i adresu, a limit kroków jest stały,
  - trwałe przechowywanie danych modułu (klucz-wartość) z limitem rozmiaru per moduł; zapis ponad limit zwraca błąd,
  - sekrety (tokeny odświeżania itp.) w Menedżerze poświadczeń Windows - moduł nigdy nie dostaje dostępu do sekretów innego modułu,
  - zegar i timery, kryptograficznie bezpieczne liczby losowe (weryfikatory PKCE, identyfikatory urządzeń),
  - "prymitywy logowania" wyświetlane w oknie konfiguracji: pokaż kod urządzenia, pokaż kod QR, otwórz stronę logowania w osadzonym oknie przeglądarki i zwróć adres, na który nastąpiło przekierowanie, użyj konta Windows (WAM) - szczegóły w sekcji 9.1. Tożsamość logowania jest częścią manifestu i zgody przy instalacji (recenzja R3): moduł deklaruje hosty stron logowania, a dla WAM identyfikator aplikacji i zasoby/zakresy, o które prosi. Okno pokazuje to użytkownikowi do zatwierdzenia przy instalacji i przy każdej zmianie. Rdzeń otwiera w osadzonym oknie tylko adresy HTTPS z zatwierdzonych hostów logowania, zawsze pokazuje w oknie aktualny host strony i przerywa logowanie, gdy strona przejdzie na host spoza listy; WAM dostaje wyłącznie zatwierdzony identyfikator aplikacji i zakresy,
  - emisja zdarzeń (nowa wiadomość, zmiana stanu sesji) do rdzenia, który obsługuje wyzwalacze.
- **Konto należy do dokładnie jednego modułu** (recenzja R4): kilka modułów może deklarować tę samą platformę, ale każde konto jest przypięte do modułu, który je zalogował; narzędzia i zdarzenia konta obsługuje tylko ten moduł.
- **Moduł eksportuje**: manifest (nazwa, wersja, platforma, dozwolone hosty, obsługiwane operacje, dodatkowe narzędzia), logowanie i przywracanie sesji, operacje ze wspólnego zestawu (8.5), obsługę zdarzeń na żywo.
- **Dodatkowe narzędzia modułu**: moduł może zadeklarować własne narzędzia spoza wspólnego zestawu (np. w przyszłości `teams.list_meetings`); rdzeń wystawia je w MCP i CLI z przedrostkiem modułu. Manifest deklaruje dla każdego dodatkowego narzędzia, czy czyta, czy pisze; narzędzia piszące przechodzą przez te same uprawnienia i zatwierdzanie co wspólny zestaw (8.6), a czytające - przez uprawnienie czytania (recenzje R6 i R7). To jest furtka dla modułów funkcji (spotkania) z etapów 2-3.

Limity zasobów (decyzja projektowa, recenzje R1 i R3): każde wywołanie modułu ma limit czasu wykonania i limit pamięci. Limit czasu liczy się zegarowo dla całego wywołania, łącznie z czasem w importach rdzenia: epoch/fuel przerywa kod WASM, a importy HTTP i WebSocket mają własne limity połączenia i odczytu, `Retry-After` dłuższy niż ustalony maksymalny czas oczekiwania kończy się błędem zamiast czekania, a po przekroczeniu limitu całego wywołania rdzeń anuluje trwające operacje sieciowe; przekroczenie przerywa wywołanie, moduł jest wyładowywany, a stan i błąd pokazują `status` i okno. Wyjątek: czas, w którym import czeka na człowieka przy logowaniu (strona logowania, kod urządzenia, kod QR, okno WAM), nie wlicza się do tego limitu; ma osobny, długi budżet, użytkownik może przerwać logowanie w oknie, a przekroczenie budżetu albo przerwanie kończy logowanie błędem, nie licząc się jako awaria modułu (recenzja R4). Odpowiedzi HTTP i komunikaty WebSocket mają limit rozmiaru; większe są przerywane z błędem (recenzja R4). Po kilku awariach z rzędu rdzeń nie ładuje modułu ponownie automatycznie. Inne moduły i konta działają dalej. Konkretne wartości limitów - wybór budującego.

Instalacja modułu: plik `.wasm` w katalogu modułów (`%APPDATA%\open_mess\modules\`) albo "Dodaj z pliku" w sekcji Moduły okna. Moduły można włączać i wyłączać bez restartu demona. Konto jest przypięte do modułu po jego nazwie z manifestu. Podmiana pliku modułu (aktualizacja) wymaga zgody jak przy instalacji: okno pokazuje, że nowy plik przejmie konta i sekrety poprzednika, oraz różnice w manifeście; bez zgody nowy plik się nie ładuje (recenzja R6). W katalogu modułów może być tylko jeden plik o danej nazwie z manifestu: drugi plik o tej samej nazwie jest traktowany jak podmiana i czeka na zgodę, a do tego czasu ładuje się dotychczasowy (recenzja R7). Wyłączenie albo podmiana modułu przełącza jego konta w stan "offline" (sekrety, reguły i stan zostają, oczekujące akcje wygasają); usunięcie modułu pyta, czy usunąć też jego konta (wtedy jak przy usuwaniu konta w 8.8) (recenzja R5).

Model asynchroniczny wywołań między rdzeniem a modułem (natywne async komponentów vs. model zdarzeń/odpytywania) - wybór budującego, z jednym ograniczeniem (recenzja R7): trwałe połączenia na żywo (WebSocket zdarzeń) trzyma rdzeń, a moduł obsługuje każde zdarzenie krótkim wywołaniem z normalnym limitem czasu - nie ma wywołania modułu trwającego przez cały czas życia demona.

### 8.4 Dane i sekrety

- Sekrety (tokeny odświeżania, token lokalnego API): Menedżer poświadczeń Windows.
- Stan (konfiguracja, kursory rozmów, identyfikatory widzianych wiadomości dla wyzwalaczy, akcje piszące we wszystkich stanach raportowanych przez `pending`, łącznie z wykonanymi samodzielnie i niepewnymi - przez ustalony czas przechowywania (recenzja R9), stan wyzwalaczy): SQLite w `%APPDATA%\open_mess\`.
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
- `get_attachment` - pobranie załącznika (zrzut ekranu, PDF) do pliku lokalnego, żeby agent mógł go przeczytać. Ścieżkę wybiera rdzeń: plik trafia do zarządzanego katalogu (`%APPDATA%\open_mess\attachments\`) pod oczyszczoną nazwą, nigdy nie nadpisuje istniejących plików, a narzędzie zwraca ścieżkę; agent nie podaje ścieżki docelowej (recenzja R2). Pojedynczy załącznik ma limit rozmiaru, a katalog ma limit łącznego rozmiaru i czas przechowywania: najstarsze pliki są usuwane po przekroczeniu limitu albo czasu, ale nigdy pliki pobrane w ciągu ostatnich kilku minut (żeby agent zdążył je przeczytać). Pobieranie załączników ma własny limit rozmiaru zamiast ogólnego limitu odpowiedzi HTTP (recenzje R4 i R5),
- `writing_context` - przykłady wiadomości napisanych przez użytkownika w danej rozmowie, żeby agent pisał jego stylem (przeniesione z `teams-wrapper`, łącznie z filtrami: tylko wiadomości użytkownika, bez sekretów, kodu, samych linków, danych operacyjnych).

Pisanie:
- `send` - wysłanie wiadomości; odpowiedź w wątku i odpowiedź na konkretną wiadomość jako parametry,
- `edit_message`, `react`,
- `pending` - stan akcji piszących: czekających na zatwierdzenie i wykonanych samodzielnie (oczekuje / wykonano / odrzucono / wygasło / niepewne).

Wynik niepewny (recenzja R7): jeśli platforma mogła przyjąć akcję, ale odpowiedź nie dotarła (np. limit czasu), akcja ma stan "niepewne" i nie jest automatycznie ponawiana. Moduł nadaje każdej wysyłce własny identyfikator klienta (jeśli platforma go obsługuje) i przy następnej okazji sprawdza w rozmowie, czy wiadomość istnieje, zmieniając stan na "wykonano" albo pozwalając ponowić bez duplikatu. Dla edycji i reakcji moduł sprawdza aktualną treść wiadomości albo obecność reakcji (recenzja R8).

Każde narzędzie piszące zachowuje się według uprawnień (8.6): "samodzielnie" - wykonuje od razu; "z potwierdzeniem" - tworzy akcję czekającą na zatwierdzenie i zwraca jej identyfikator; "nigdy" - zwraca błąd. Osobne narzędzia do przygotowania i wysłania (`prepare_message` / `send_prepared` z `teams-wrapper`) są zbędne, bo zatwierdzenie nie odbywa się już w czacie z agentem (decyzja projektowa, recenzja R3).

Stan:
- `status` - stan demona, modułów, sesji.

Czytanie nigdy nie oznacza wiadomości jako przeczytanych.

Wyniki każdego narzędzia, które zwraca treść wiadomości albo dane od innych osób (m.in. `unread`, `mentions`, `read_messages`, `search`, `list_conversations`, `get_attachment`, `writing_context` - wiadomości użytkownika mogą cytować innych), oddzielają treść pochodzącą od innych osób (tekst, nazwy, nazwy plików) od metadanych i oznaczają ją jako niezaufaną, tak samo jak powiadomienia wyzwalaczy (AGENTS.md).

Identyfikatory rozmów i wiadomości są nieprzezroczyste dla agenta i jednoznacznie wskazują platformę i konto. Treść wiadomości dla agenta: zwykły tekst / Markdown (konwersja z HTML Teams i z powrotem po stronie modułu).

### 8.6 Uprawnienia agentów

Ustawiane w sekcji "Uprawnienia agentów" okna, per konto, opcjonalnie zawężone do wybranych rozmów:

- czytanie: tak / nie,
- pisanie: nigdy / z potwierdzeniem / samodzielnie.

**Samodzielne wysyłanie jest opcją do wyboru** (decyzja użytkownika): agent może wysyłać bez potwierdzenia tylko tam, gdzie użytkownik świadomie ustawił "samodzielnie" - per konto, opcjonalnie zawężone do wybranych rozmów. Nowe konto startuje z "z potwierdzeniem" (decyzja projektowa).

Kto ma jakie uprawnienia (decyzja projektowa, recenzje R1 i R2): **jedne zasady dla każdego wywołującego.** Rdzeń egzekwuje wyłącznie uprawnienia konta/rozmowy, tak samo dla MCP, CLI, sesji obudzonej powiadomieniem i agenta uruchomionego przez wyzwalacz. Nie ma tokenów zakresowych ani uprawnień per reguła: każdy proces na koncie użytkownika może wywołać CLI ze wspólnym tokenem, więc węższe uprawnienia dla jednego wywołującego dałoby się obejść zwykłym `om send` z narzędzia Bash. Kto nie chce, żeby agent obudzony przez wyzwalacz wysyłał bez zatwierdzenia, ustawia dla konta albo danej rozmowy "z potwierdzeniem". Okno konfiguracji ostrzega przy zapisie reguły wyzwalacza, która obejmuje konto albo rozmowę z "samodzielnie".

"Z potwierdzeniem" oznacza, że każda akcja pisząca - wysłanie, edycja i reakcja - czeka na zatwierdzenie **poza zasięgiem agenta**: powiadomienie Windows z dokładną rozmową, odbiorcami, treścią (dla edycji: przed i po, dla reakcji: która i pod czym) oraz przyciskami Zatwierdź / Odrzuć, a także lista "Do zatwierdzenia" w oknie konfiguracji. Niezatwierdzona akcja wygasa po ustalonym czasie. Przed wykonaniem rdzeń sprawdza, czy cel się nie zmienił: dla DM i czatów grupowych skład uczestników musi być taki jak w zatwierdzonym podglądzie, dla edycji, reakcji i odpowiedzi na konkretną wiadomość - treść wiadomości docelowej (recenzja R7); jeśli się zmienił, akcja przepada i trzeba ją zatwierdzić od nowa (kanał jest celem sam w sobie, jego skład nie jest porównywany). Liczba oczekujących akcji jest ograniczona, a powiadomienia są łączone w jedno ("N akcji czeka"), gdy przychodzą szybko po sobie; takie zbiorcze powiadomienie ma tylko przycisk otwierający listę "Do zatwierdzenia" z podglądem każdej akcji - nie ma zatwierdzania wielu akcji naraz (recenzja R7), żeby zalew akcji nie skłaniał do zatwierdzania bez czytania (recenzja R5). Zgoda wyrażona w czacie z agentem nie wystarcza, bo agent mógłby ją zmyślić. Dotyczy to agentów w sesji interaktywnej, agentów uruchomionych przez wyzwalacz i CLI. Decyzja projektowa.

CLI nie ma żadnych dodatkowych przywilejów: podlega tym samym uprawnieniom i tej samej ścieżce zatwierdzania co MCP. Demon nie umie wiarygodnie odróżnić człowieka w terminalu od agenta, który sam otwiera konsolę (np. `Start-Process` z narzędzia Bash), więc rozróżnianie po terminalu byłoby obejściem uprawnień. Człowiek wysyłający z CLI na koncie "z potwierdzeniem" klika Wyślij w powiadomieniu tak jak przy agencie; kto chce wysyłać bez tego, ustawia "samodzielnie". Decyzja projektowa (recenzja Codex i GLM commita d82e768 odrzuciła wcześniejszą regułę "CLI w interaktywnym terminalu = użytkownik").

Rdzeń egzekwuje uprawnienia - nie polega na tym, że agent się do nich zastosuje.

### 8.7 Wyzwalacze (budzenie agenta)

Sekcja "Wyzwalacze" w oknie. Reguła = warunek + akcja.

Warunki: DM do użytkownika, wzmianka, nowa wiadomość w wybranej rozmowie, wiadomość od wybranych osób; zawsze w ramach konkretnego konta. Własne wiadomości użytkownika (wysłane z dowolnego klienta) nigdy nie odpalają reguł. Reguła nie odpala dla rozmów, których agent nie może czytać (8.6) - cofnięcie uprawnienia czytania natychmiast wycisza związane z nim wyzwalacze (recenzja R7).

Akcje:
1. **Wpis do otwartej sesji Claude Code** przez kanał MCP (`notifications/claude/channel`, z deklaracją możliwości `experimental: { "claude/channel": {} }` - mechanizm używany przez wtyczkę Telegram, potwierdzony dla transportu stdio). Jeśli kanał nie działa przez transport HTTP, budujący dodaje cienki most stdio (`om mcp-stdio`), który tylko przekazuje komunikaty do demona. Claude Code sam uruchamia most jako swój serwer MCP (stdio) i kończy go razem z sesją; most niczego nie uruchamia, tylko przekazuje komunikaty - po jednym na sesję z włączonymi kanałami. Gdy demon zniknie albo się zrestartuje, most ponawia połączenie z lokalnym API z rosnącym odstępem; do tego czasu narzędzia w tej sesji zwracają czytelny błąd "demon niedostępny" zamiast cicho milczeć (recenzja R9). To jedyny wyjątek od zasady jednego procesu (sekcja 6): bez logiki platform, bez sesji kont, bez własnego stanu. Claude Code dostarcza kanały tylko do sesji uruchomionych z włączonymi kanałami (flaga przy starcie; dla serwera spoza listy zatwierdzonych wtyczek prawdopodobnie tryb deweloperski) - to akceptowalne, okno konfiguracji pokazuje gotowe polecenie startu. Gdy takich sesji jest kilka, powiadomienie dostaje ostatnio aktywna (bez duplikatów); gdy nie ma żadnej, działa akcja 2, jeśli reguła ją ma. Decyzja projektowa (pytanie z przeglądu dokumentu).
2. **Uruchomienie agenta od zera** gdy nie ma sesji: demon uruchamia skonfigurowany program (np. `claude -p` albo Codex) ze stałą listą argumentów ustaloną w regule, bezpośrednio, bez powłoki. Treść zdarzenia nigdy nie jest wklejana do polecenia ani argumentów: trafia do programu przez stdin albo jako ścieżka do pliku z treścią, oznaczoną jako niezaufana. Takie pliki leżą w katalogu tymczasowym demona, mają limit rozmiaru i są usuwane po zakończeniu uruchomienia, a pozostałości (np. po awarii demona) są usuwane przy jego starcie (recenzje R5 i R6). Proces korzysta z MCP/CLI na tych samych zasadach co każdy inny wywołujący (8.6). Decyzja projektowa (recenzja R1).

Zabezpieczenia (decyzje projektowe):
- grupowanie: zdarzenia z jednej rozmowy są zbierane i wysyłane jako jedno powiadomienie po chwili ciszy, ale nie później niż po ustalonym maksymalnym czasie od pierwszego zdarzenia, nawet jeśli rozmowa nie cichnie (recenzja R4),
- cisza nocna i limit pobudek na godzinę per reguła,
- treść od innych osób jest przekazywana jako dane, wyraźnie oznaczona jako niezaufana; agent obudzony albo uruchomiony przez wyzwalacz ma uprawnienia konta/rozmowy (8.6), a okno ostrzega przy regułach obejmujących "samodzielnie".

### 8.8 Okno konfiguracji

Układ wybrany przez użytkownika: klasyczne ustawienia z bocznym menu (sekcje po lewej, szczegóły po prawej). Zamknięcie okna nie zatrzymuje demona (potwierdzone przez użytkownika); okno otwiera się z ikony w zasobniku albo `om config`.

Sekcje:
- **Konta** - lista kont ze stanem połączenia, "Dodaj konto", "Zaloguj ponownie", usuwanie konta (usuwa też sekrety konta z Menedżera poświadczeń, stan i dane konta z bazy, oczekujące akcje i pobrane załączniki; tam, gdzie platforma to umożliwia, wylogowuje sesję po stronie serwera - recenzja R4),
- **Uprawnienia agentów** - sekcja 8.6,
- **Wyzwalacze** - sekcja 8.7,
- **Do zatwierdzenia** - akcje piszące (wysłanie, edycja, reakcja) czekające na decyzję (może być częścią innej sekcji, wybór budującego),
- **Moduły** - zainstalowane moduły, włącz / wyłącz, dodaj z pliku,
- **Integracje (MCP / CLI)** - gotowe polecenia do podłączenia MCP, ścieżka do CLI,
- **Demon i logi** - stan, pamięć, połączenia, ostatnie błędy.

Szkic układu (lokalny, poza repo): wariant A z sesji projektowej - boczne menu z sekcjami, po prawej lista kont z ikoną platformy, nazwą konta, opisem, stanem połączenia (kropka + tekst) i menu akcji; u dołu menu stan demona (pamięć, liczba połączeń).

Technologia okna - decyzja projektowa: Tauri 2 (WebView2, wbudowany w Windows 11). Powody: logowanie Microsoftu i tak wymaga osadzonej przeglądarki (sekcja 9.1), HTML/CSS ułatwia "ładny layout", a WebView2 działa tylko gdy okno jest otwarte, więc nie łamie zakazu stale działającego silnika przeglądarki. Budujący może wybrać inną technologię, jeśli zachowa te trzy własności.

## 9. Moduł Teams

Fakty o protokole pochodzą z analizy statycznej REA pakietu nowego Teams (MSTeams 26260.1717.5229.7049, zawiera pełny klient web) - szczegóły, dowody i lista rzeczy do sprawdzenia na żywo: `docs/protocols/teams.md`. Każdy fakt oznaczony tam jako niepotwierdzony (sekcja "Needs live capture") wymaga jednego oczyszczonego przechwytu na żywo (9.5), zanim kod będzie od niego zależał.

### 9.1 Logowanie

Moduł loguje się tak jak oficjalny klient: logowanie Microsoft (Entra ID) z identyfikatorem aplikacji Teams, a potem sam odświeża tokeny. Identyfikatory (potwierdzone w kodzie klienta): desktop `1fec8e78-bce4-4aaf-ab1b-5451cc387264` z przekierowaniem `https://login.microsoftonline.com/common/oauth2/nativeclient` - tego używa open_mess; web `5e3ce6c0-2b1f-4285-8d4b-75ee78787346` (SPA, przekierowanie `https://teams.microsoft.com/v2`) - tylko jako zapas.

Sposoby logowania, w kolejności preferencji (pierwsze zadanie implementacji to próba na koncie użytkownika, przy nim - 9.5):
1. **Konto Windows (WAM)** - tak robi oficjalny desktop. Potwierdzone tylko dla wywołującego, którym jest pakiet MSTeams; WAM może odmówić programowi innej firmy.
2. **Osadzona strona logowania** - kod autoryzacji + PKCE (`S256`) w oknie WebView2 z przekierowaniem `nativeclient`, które rdzeń przechwytuje. To konkretna droga zapasowa; przekierowania na localhost dla tych identyfikatorów nie ma.
3. **Kod urządzenia** - żaden z klientów go nie używa; niesprawdzony, ostatni (9.5).

Tokeny: dla każdej usługi osobny token z tabeli odbiorców w `docs/protocols/teams.md` (m.in. czaty i Trouter, middle tier, wyszukiwanie `https://outlook.office.com/`, SharePoint organizacji). Klient prosi o tokeny CAE (`CP1`), więc moduł odświeża token nie tylko przed wygaśnięciem, ale też po wyzwaniu CAE (401 z `WWW-Authenticate` żądającym claims).

Po zalogowaniu (decyzja projektowa, recenzja R2): przy logowaniu przez WAM aplikacja nie dostaje tokenu odświeżania - sesję trzyma broker Windows, a odnowienie to ciche pobranie tokenu przez WAM; przy osadzonej stronie logowania i kodzie urządzenia token odświeżania trafia do sekretów. Błąd 401 bez wyzwania CAE -> jedna próba odświeżenia -> dopiero potem stan "zaloguj ponownie". Hasło użytkownika nigdy nie przechodzi przez open_mess poza stroną logowania Microsoftu.

### 9.2 Współistnienie z oficjalnym Teams

Wymaganie nadrzędne (sekcja 6). open_mess zachowuje się jak osobne urządzenie, np. telefon obok komputera:
- własna rejestracja punktu końcowego: raz wygenerowany i zapamiętany UUID v4 (`PUT v2/users/ME/endpoints/<uuid>`, ten sam UUID jako `epid` w Trouter i `registrationId` w rejestratorze) i własny identyfikator urządzenia; nigdy identyfikatory oficjalnego klienta,
- nie czyta ani nie zapisuje niczego w danych oficjalnego klienta (`%LOCALAPPDATA%\Packages\MSTeams_8wekyb3d8bbwe\` i podobne),
- nie zapisuje stanu przeczytania. Osobnego endpointu potwierdzeń odczytu nie ma; zakazane są konkretne zapisy: właściwość `consumptionhorizon`, `consumptionHorizonBookmark`, `isread` wiadomości, `l2ch` w metadanych użytkownika, "oznacz wszystko jako przeczytane" (`appConsumptionHorizon`) i zbiorcze zapisy właściwości. Moduł zapisuje właściwości wyłącznie z listy dozwolonych nazw, na której jest tylko `emotions` (reakcje),
- nie wywołuje `activeendpoint`, nie publikuje obecności ani aktywności użytkownika w Trouter (żeby nie przejmować powiadomień z telefonu ani statusu),
- szanuje limity serwera tak jak oficjalny klient: honoruje `Retry-After`, rezygnuje, gdy przekracza on 300 s, najwyżej 4 ponowienia z wykładniczym odstępem, zapytania w tle oznaczone niskim priorytetem,
- test akceptacyjny: tydzień równoległej pracy z oficjalnym klientem bez objawów z sekcji 6.

### 9.3 Wiadomości, zdarzenia, wyszukiwanie

- **Katalog usług:** wymiana tokenu w authsvc pod `https://teams.microsoft.com/api/authsvc/v1.0/authz` (stary host `authsvc.teams.microsoft.com` jest w kodzie oznaczony jako przestarzały). Pole `regionGtms` odpowiedzi to katalog adresów: usługa czatów (`chatServiceAfd`), Trouter, rejestrator, AMS, middle tier, wyszukiwanie.
- **Token Skype jest wycofywany:** klient przechodzi na tokeny Entra/CAE (usługa czatów, Trouter i AMS przyjmują token dla `https://ic3.teams.office.com` - do potwierdzenia na żywo); token Skype zostaje dla middle tier i jako zapas, zgodnie z `WWW-Authenticate: token_types=`.
- **Usługa czatów:** baza z `regionGtms.chatServiceAfd` (`teams.microsoft.com/api/chatsvc/<region>/`), z obsługą przekierowań 404 + `Location` (`BehaviorOverride: redirectAs404`). Lista rozmów, historia (stronicowanie `syncState` / `backwardLink`), pojedyncza wiadomość, wysyłka, edycja, usuwanie, reakcje (`properties?name=emotions`), wątki. Treść wiadomości to HTML (`RichText/Html`), wzmianki w `properties.mentions`. Nowy klient bierze listę rozmów z agregatora (CSA) - open_mess go nie potrzebuje.
- **`unread`** liczy stan nieprzeczytania regułą klienta z `docs/protocols/teams.md` (horyzont odczytu rozmowy, zakładka "oznacz jako nieprzeczytane", `appConsumptionHorizon` z "oznacz wszystko jako przeczytane") - wyłącznie odczytując te wartości.
- **Zdarzenia na żywo:** Trouter v4 (alokacja `go.trouter.teams.microsoft.com/v4/a`, Socket.IO, rejestracja w rejestratorze); identyfikator aplikacji powiadomień, który rejestrator przyjmie od open_mess, do ustalenia na żywo. Odpytywanie usługi czatów jako zapas, gdy połączenie na żywo zawiedzie.
- **Wyszukiwanie:** `POST https://substrate.office.com/search/api/v2/query` z tokenem `https://outlook.office.com/`.
- **Załączniki:** obrazy przez AMS (baza z `regionGtms`); pliki w OneDrive/SharePoint - host organizacji odkrywany przez middle tier (`beta/me/files/availability`) i dane zespołu. Manifest modułu Teams deklaruje dla nich wzorzec `*.sharepoint.com` (8.3).

Gdy odpowiedź serwera ma nieznany kształt, operacja kończy się jawnym błędem "niezgodny protokół" i stan modułu to pokazuje - bez zgadywania pól i bez wysyłania częściowo zbudowanych wiadomości (wzorzec z `teams-wrapper`).

### 9.4 Rola REA

Analiza statyczna REA jest zrobiona (`docs/protocols/teams.md`). Dalej REA służy do przechwytów na żywo z listy "Needs live capture" - przy użytkowniku, na koncie firmowym, z minimalnym ruchem (9.5) - i do ponownej analizy po większych aktualizacjach klienta Teams. Wyniki trafiają do `docs/protocols/teams.md`, zawsze oczyszczone z tokenów, ciasteczek, identyfikatorów tenantu i użytkowników oraz treści wiadomości.

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

Do przeniesienia: zatwierdzanie wysyłki (u nas poza zasięgiem agenta, 8.6), czytanie bez oznaczania jako przeczytane, `writing_context` z filtrami, MCP przez HTTP w jednym procesie dla Claude i Codexa, jawny błąd przy niezgodnym protokole. Webhook z `teams-wrapper` świadomie nie przechodzi do etapu 1 (sekcja 5).

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
- moduły WASM spoza repo: izolacja chroni komputer, sieć lokalną, inne moduły i inne konta (lista hostów, brak dostępu do cudzych sekretów, limity). Nie chroni konta, które użytkownik danemu modułowi powierzył: moduł z tokenami konta i dostępem do hostów platformy może wykonać na tym koncie dowolną operację, bo rdzeń nie rozumie znaczenia jego zapytań HTTP. Uprawnienia agentów (8.6) chronią przed agentami, nie przed modułem. Instalacja modułu i zalogowanie nim konta to zaufanie do jego kodu, jak instalacja aplikacji - okno mówi to wprost przy instalacji i logowaniu (rebutal R4, sekcja 16),

Zasada zasobów (zamrożona, recenzja R8): wszystko, co moduł, wyzwalacz albo agent może mnożyć - połączenia (w tym trwałe WebSockety per moduł), uruchomienia agentów z akcji 2 (równocześnie per reguła i łącznie; nadmiarowe zdarzenia czekają w kolejce albo są łączone, nie uruchamiają kolejnych procesów), pliki, akcje oczekujące, magazyn modułu, rozmiar odpowiedzi - ma limit liczby albo rozmiaru egzekwowany przez rdzeń. Konkretne wartości to wybór budującego. Zgłoszenie postaci "brak limitu dla X", gdzie X podlega tej zasadzie, jest rozstrzygnięte przez tę zasadę.

Granica twardnienia (zamrożona): nie bronimy się przed złośliwym kodem działającym na koncie Windows użytkownika z zamiarem obejścia open_mess - taki kod może i tak czytać Menedżer poświadczeń, symulować terminal albo klikać w powiadomienia. Bronimy się przed agentem, który przez pomyłkę albo manipulację próbuje zrobić więcej, niż pozwalają uprawnienia, używając zwykłych narzędzi (MCP, CLI z narzędzia Bash).

## 16. Rejestr rebutali

- R4, Codex P1 ("niezaufany moduł WASM może wysłać wiadomość zwykłym zapytaniem HTTP do zatwierdzonego hosta, omijając uprawnienia"): odrzucone jako błąd specu, przyjęte jako granica zaufania. Rdzeń nie może egzekwować uprawnień na poziomie surowych zapytań modułu do API platformy bez rozumienia każdego protokołu, co przeczy idei modułów. Moduł, któremu użytkownik powierzył konto, jest zaufany dla tego konta; izolacja chroni resztę systemu. Zapisane w threat modelu (sekcja 15).
- R6, Codex P1 w recenzji commita 6d1e45f ("przez firmowe proxy moduł może dosięgnąć LAN, gdy zatwierdzona domena odpowiada proxy innym, prywatnym adresem"): przyjęte jako znane ograniczenie, nie jako błąd specu. Przy połączeniu przez proxy nazwę rozwiązuje proxy, więc rdzeń nie może przypiąć adresu; ochrona LAN zależy wtedy od proxy organizacji. Atak wymaga, żeby użytkownik zatwierdził w liście hostów modułu domenę kontrolowaną przez atakującego. Bez proxy ochrona działa w pełni (przypięcie adresu). Okno przy instalacji modułu spoza repo mówi, że przy aktywnym proxy systemowym ochrona sieci lokalnej zależy od proxy.
- R6, GLM P3 (świeże załączniki chwilowo przekraczają limit katalogu): przyjęte bez zmian - przekroczenie trwa tylko przez okno ochrony świeżych plików.
Pozostałe zgłoszenia R1-R9 przyjęto.

## Changelog rund

- R1 (Codex REJECT, GLM REJECT): reguły wyzwalaczy tylko zawężają uprawnienia, tokeny zakresowe dla uruchomień z akcji 2, akcja 1 dziedziczy uprawnienia konta z ostrzeżeniem (Codex P1, GLM P2); limity czasu i pamięci modułów WASM (Codex P2); most stdio jako jawny, długożyjący wyjątek od zasady jednego procesu (Codex P2); akcja 2 bez powłoki i bez wklejania treści do polecenia (GLM P2); lista hostów modułu zatwierdzana przez użytkownika, wzorce sufiksu, twarda blokada adresów lokalnych i prywatnych (GLM P2, P3); oznaczanie niezaufanej treści w wynikach narzędzi (GLM P3); doprecyzowanie Linuksa w CI (GLM P3); token API przy pierwszym uruchomieniu (GLM P3).
- R2 (Codex REJECT, GLM REJECT): usunięte tokeny zakresowe i uprawnienia per reguła - wszyscy wywołujący mają uprawnienia konta/rozmowy, bo agent może obejść węższe uprawnienia przez `om send` (Codex P1); WAM bez tokenu odświeżania, odnowienie przez broker (Codex P2); przypięcie połączenia do sprawdzonego adresu i kontrola każdego przekierowania (GLM P2); `get_attachment` zapisuje tylko do zarządzanego katalogu (GLM P2); webhooki usunięte z etapu 1, bo wysyłałyby treść poza maszynę (GLM P2); limit magazynu modułu, cykl życia mostu stdio, `get_attachment` w oznaczaniu niezaufanej treści, rejestr rebutali (GLM P3).
- R3 (Codex REJECT, GLM REJECT): każda akcja pisząca (wysłanie, edycja, reakcja) przechodzi przez zatwierdzenie poza agentem, a `prepare_message`/`send_prepared` zastąpione jednym `send` i narzędziem `pending` (Codex P2, GLM P3); zegarowy limit wywołania modułu obejmuje importy sieciowe, limit `Retry-After` (Codex P2, GLM P2); hosty stron logowania i tożsamość WAM w manifeście i zgodzie przy instalacji, kontrola hosta strony logowania (GLM P2); naprawiona ścieżka katalogu załączników (GLM P3, Codex P2 w recenzji commita 3f7dacc).
- R4 (Codex REJECT, GLM REJECT): grupowanie z maksymalnym opóźnieniem (Codex P2); limit rozmiaru i retencja katalogu załączników, limit rozmiaru odpowiedzi HTTP i WebSocket (Codex P2, GLM P3); oznaczanie niezaufanej treści dla każdego narzędzia zwracającego treść, w tym `writing_context` (GLM P2); jedna instancja demona (GLM P2); czekanie na człowieka przy logowaniu poza limitem czasu wywołania (GLM P2 w recenzji commita ef8d7e8); usuwanie konta czyści sekrety i dane (GLM P3); konto przypięte do jednego modułu (GLM P3); granica zaufania modułu wobec powierzonego konta (Codex P1 - rebutal, sekcja 16).
- R5 (Codex REJECT, GLM REJECT): przepływ treści do podłączonych agentów i ich dostawców modeli zapisany wprost w specu i AGENTS.md (Codex P2); zatwierdzenie wiąże się ze składem uczestników i treścią celu, zmiana unieważnia akcję (Codex P2); proxy systemowe i systemowy magazyn certyfikatów (GLM P2); losowość dla modułów, cykl życia kont przy wyłączaniu/usuwaniu modułu, limit oczekujących akcji i łączenie powiadomień, sprzątanie plików akcji 2, ochrona świeżych załączników i ich własny limit rozmiaru (GLM P3).
- R6 (Codex REJECT, GLM APPROVE): zatwierdzenie reakcji wiąże się z treścią wiadomości docelowej (Codex P2); stały port lokalnego API (GLM P3); klasa czytanie/pisanie dla dodatkowych narzędzi modułów (GLM P3); podmiana modułu wymaga zgody na przejęcie kont (GLM P3); sprzątanie plików akcji 2 przy starcie (GLM P3); proxy a ochrona LAN i chwilowe przekroczenie limitu załączników - rebutale (sekcja 16).
- R7 (Codex REJECT, GLM APPROVE): wyzwalacze respektują uprawnienie czytania (Codex P2); stan "niepewne" dla akcji bez potwierdzenia i identyfikator klienta zamiast automatycznego ponawiania (Codex P2); kontrola celu obejmuje odpowiedź na konkretną wiadomość (Codex P2); minimalna głębokość wzorców hostów, jeden plik na nazwę modułu, własne wiadomości nie odpalają reguł, zbiorcze powiadomienie bez zatwierdzania naraz, połączenia na żywo po stronie rdzenia, dodatkowe narzędzia czytające pod uprawnieniem czytania (GLM P3).
- R8 (Codex REJECT, GLM APPROVE): zamrożona zasada zasobów - limit równoczesnych uruchomień agentów z akcji 2 i liczby trwałych połączeń modułu, oraz ogólna reguła dla każdego mnożonego zasobu (Codex P2 x2); `pending` obejmuje akcje samodzielne, rozstrzyganie niepewnych edycji i reakcji (GLM P3).
- R9 (Codex REJECT, GLM APPROVE): dozwolone tylko globalnie routowalne adresy unicast, w tym blokada CGNAT `100.64.0.0/10` (Codex P2); trwały zapis wszystkich stanów `pending` (GLM P3); most stdio ponawia połączenie z demonem (GLM P3). Sekcja 9 przepisana na podstawie analizy statycznej REA (`docs/protocols/teams.md`).
