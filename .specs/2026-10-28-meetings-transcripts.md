# open_mess - spotkania i transkrypcje Teams (etap 2)

Data: 2026-10-28
Status: do review (Codex, GLM)
Powiązane: `.specs/2026-10-26-open-mess-design.md` (8.3, 8.5, 8.6, 12, 15), `docs/protocols/teams.md` (sekcja 9, "Live results" 9, "Needs live capture" 13-16), `wit/module.wit`.

## 1. Cel

Agent (przez MCP) i człowiek (przez CLI) mogą:
1. zobaczyć spotkania z kalendarza użytkownika w zadanym okresie, z identyfikatorem czatu spotkania (`19:meeting_...`), którego już dziś używają `read_messages` i `search`;
2. pobrać transkrypcję, którą zrobił sam Teams (ktoś na spotkaniu włączył transkrypcję), jako tekst z mówcami i czasami.

Podsumowanie robi agent; open_mess niczego nie streszcza, nie nagrywa i nie transkrybuje sam (spec 12). Etap 3 (agent na spotkaniu) jest poza zakresem.

## 2. Decyzje właściciela

- Etap 2 = spotkania, na których użytkownik jest, i transkrypcje Teams; podsumowuje agent (spec 3, 12).
- Live check tylko do odczytu, przy właścicielu, za zgodą w czacie - zrobiony 2026-10-27 (`docs/protocols/teams.md`, "Live results" 9).

Otwarte decyzje właściciela - sekcja 10. Ten spec ich nie rozstrzyga.

## 3. Kontekst w kodzie i protokole

Kod (sprawdzony w repo):
- `wit/module.wit`: `tool-decl {name, description, input-schema, writes}` i `manifest.extra-tools` istnieją; `call(account, tool, args-json)` uruchamia narzędzie wspólne albo dodatkowe; `sign-in.wam-token(scopes, interactive, claims)`.
- `crates/open-mess/src/modules.rs` (`canonical`): dodatkowe narzędzia i zakresy WAM wchodzą do skrótu manifestu, więc ich zmiana wymaga ponownej zgody (spec 8.3). `crates/open-mess/src/ui/commands.rs` pokazuje je w oknie przy zgodzie.
- `crates/open-mess/src/tools/mod.rs`: `Registry::list()` zwraca tylko `status` i narzędzia wspólne (`shared::tools()`); `Registry::call` waliduje argumenty ścisłymi typami z `shared.rs` (`validate`), sprawdza czytanie przez `readable()` (argument `conversation`) i odfiltrowuje wyniki przez `drop_unreadable()` (pole `items[].conversation`), dodaje `account` do elementów i notkę o niezaufanej treści (`shared::with_notice`). **Dodatkowe narzędzia modułów nie są dziś wystawiane w MCP ani CLI** - to trzeba dobudować (krok 2).
- `crates/open-mess/src/cli.rs`: CLI pyta demona o listę narzędzi, więc nowe narzędzia rejestru pojawią się w CLI bez zmian w CLI.
- `crates/open-mess-host/src/sign_in.rs` (`allowed`): WAM dostaje wyłącznie zakresy wpisane dosłownie w manifeście. `crates/open-mess-host/src/hosts.rs`: wzorce hostów `*.<sufiks>` z regułą domeny rejestrowalnej (spec 8.3, R7).
- `modules/teams/src/manifest.rs`: hosty m.in. `teams.microsoft.com`, `substrate.office.com`, `*.sharepoint.com`; zakresy WAM `https://api.spaces.skype.com/.default`, `https://ic3.teams.office.com/.default`, `https://outlook.office.com/.default`; `extra_tools` puste.
- `modules/teams/src/authsvc.rs`: odpowiedź authsvc daje opcjonalny `skype_token` i katalog `regionGtms` z opcjonalnym `middle_tier`, ale `api::establish` zachowuje w `Session` tylko katalog - token Skype jest dziś odrzucany (do zmiany, 5.3); `modules/teams/src/session.rs` zna `tid` konta (pole `tenant`); `modules/teams/src/api.rs`: `Service` wybiera token po odbiorcy; `modules/teams/src/tools.rs` (`call`) rozdziela narzędzia.
- `crates/open-mess-host/src/limits.rs`: `CALL_TIMEOUT` 30 s na wywołanie modułu, `MAX_BODY` 16 MiB na odpowiedź HTTP.

Protokół (`docs/protocols/teams.md` sekcja 9, analiza REA i live check 2026-10-27):
- **Kalendarz**: middle tier `GET <regionGtms.middleTier>/v2.0/me/calendars/default/calendarView?StartDate=&EndDate=&shouldDecryptData=false`, token `https://api.spaces.skype.com` + `x-skypetoken` - tak czyta oficjalny klient (enterprise); live: HTTP 200, obiekt `{type, value}`, 130 zdarzeń z 30 dni w jednej odpowiedzi (290 KB, bez stronicowania), klucze zdarzenia m.in. `objectId, iCalUID, startTime, endTime, subject, organizerName, isOnlineMeeting, skypeTeamsMeetingUrl, eventType, myResponseType, seriesMasterId`. Wątek czatu spotkania jest zakodowany w linku `skypeTeamsMeetingUrl` (`/l/meetup-join/<19:meeting_...@thread.v2>/0?...`).
- **Artefakty spotkania**: MCPS `GET <regionGtms.mcpService>/contents?threadId=<wątek>&iCalUid=<id>`, odbiorca tokenu `6bc3b958-689b-49f5-9006-36d165f30e00`; live: HTTP 200, `resources[]` typów m.in. `TranscriptV2`; `TranscriptV2.metadata` ma `driveId, driveItemId, transcriptId, startTime, endTime, callId, iCalUid, instanceICalUid, ownerTenantId, organizerTenantId, webUrl`, a `location` wskazuje SharePoint właściciela nagrania.
- **Treść**: SharePoint/OneDrive właściciela: `GET <origin>/_api/v2.1/drives/<driveId>/items/<driveItemId>/media/transcripts` (lista: `id, languageTag, isDefault, isVisible, transcriptType, temporaryDownloadUrl...`), potem `GET .../media/transcripts/<id>/streamContent?is=1` = WebVTT z tagami mówców `<v Imię>`; token dla `<origin>/.default` (origin = `https://<tenant>-my.sharepoint.com`, zależny od organizacji). **UNCONFIRMED na żywo**: jedyna znaleziona transkrypcja była z innej organizacji i jej SharePoint odrzucił token (401) - pobranie we własnej organizacji to "Needs live capture" 13 (krok 0).
- **Odrzucone ścieżki**: Graph `onlineMeetings/.../transcripts` - klient go nie używa, a token Graph klienta Teams nie ma zakresów `OnlineMeeting*` (live). Starsze transkrypcje typu `Transcript` z `api.cortana.ai/MeetingArtifacts` (9.6) - nie w tym specu. Substrate `WorkingSetFiles` znajduje nagrania, nie transkrypcje - zbędne.

## 4. Narzędzia

Dwa dodatkowe narzędzia modułu Teams, oba klasy "czyta" (`writes: false`). Nazwa w MCP i CLI: `<moduł>_<narzędzie>` (sekcja 5.1), czyli `teams_list_meetings` i `teams_get_transcript`. Oba przyjmują opcjonalne `account` (dokłada je rdzeń, 5.1).

### 4.1 `teams_list_meetings`

Argumenty: `from`, `to` (RFC 3339 w UTC; domyślnie teraz minus 7 dni i teraz plus 1 dzień; `to` > `from`; zakres najwyżej 31 dni). Bez `limit`: moduł nie obcina listy, bo rdzeń filtruje ją dopiero później według uprawnień (5.1) i obcięcie przed filtrem gubiłoby dozwolone spotkania (recenzja R1); ilość reguluje zakres dat.

Moduł: jedno zapytanie kalendarza middle tier (sekcja 3). Brak `middleTier` w katalogu albo brak tokenu Skype dla konta = błąd "not supported yet: no calendar service for this account". Odpowiedź dekodowana ściśle: brak `value` albo wymaganego pola (`objectId`, `iCalUID`, `startTime`, `endTime`) = błąd "niezgodny protokół" (spec 9.3). Więcej niż 1000 zdarzeń w zakresie (live: 130 na 30 dni) = błąd "too many events, narrow from/to" zamiast cichego obcięcia. Live check widział odpowiedź bez stronicowania (klucze tylko `type` i `value`); wskaźnik kontynuacji w odpowiedzi (`@odata.nextLink`, `nextLink`, `skipToken`, `continuationToken` albo inny nieznany klucz najwyższego poziomu) = "niezgodny protokół", nigdy cicha pierwsza strona (recenzja R2). Krok 0 sprawdza gęste okno 31 dni. Zostają tylko spotkania Teams, w których użytkownik uczestniczy: zdarzenie z wątkiem czatu spotkania (`conversation` niżej) i bez odpowiedzi "odrzucone" (`myResponseType` równe `declined` bez względu na wielkość liter; wartości tego pola potwierdza krok 0). Odrzucone zaproszenia, terminy bez spotkania Teams i prywatne wpisy kalendarza nie są zwracane wcale - ani temat, ani organizator (recenzja R3). Zdarzenia sortowane rosnąco po `startTime`.

Wynik:
```json
{"items": [{"id": "<objectId>", "ical_uid": "...", "start": "...Z", "end": "...Z",
  "kind": "<eventType>", "my_response": "<myResponseType>",
  "conversation": "19:meeting_...@thread.v2",
  "untrusted": {"subject": "...", "organizer": "..."}}]}
```
- `conversation` (zawsze obecny, bo zdarzenia bez niego są pomijane): wątek z `skypeTeamsMeetingUrl` (segment ścieżki po `/l/meetup-join/`, po dekodowaniu procentowym) zaczyna się od `19:meeting_`, kończy na `@thread.v2`, ma najwyżej 256 znaków i nie zawiera białych znaków, znaków sterujących ani `/`, `?`, `#`; inny link = zdarzenie pominięte. Alfabet nie jest zawężany dalej, bo nie jest potwierdzony (krok 0 go sprawdza); identyfikator trafia do ścieżek zapytań wyłącznie zakodowany procentowo, jak w etapie 1.
- Temat i organizator to dane od innych osób - pod `untrusted` (spec 8.5). Opis (`bodyPreview`), miejsce, uczestnicy i link dołączenia nie są zwracane (minimalizacja danych; link wróci w etapie 3).

### 4.2 `teams_get_transcript`

Argumenty: `conversation` (wymagany, wątek `19:meeting_...` z `teams_list_meetings` albo `list_conversations`), `ical_uid` (opcjonalny), `start`, `end` (opcjonalne, czasy wystąpienia z `teams_list_meetings` - wybierają wystąpienie spotkania cyklicznego, które ma jeden wątek na całą serię), `transcript` (opcjonalny identyfikator z poprzedniego wyniku), `cursor` (opcjonalny, numer pierwszego fragmentu), `max_bytes` (4096-786432, domyślnie 131072).

Moduł:
1. MCPS `contents` dla wątku (i `iCalUid`, jeśli podany). Odpowiedź ma `paginationInfo` (live), którego kształt ustala krok 0; moduł pobiera dalsze strony według tego kształtu (najwyżej 10 stron), a wskaźnik kontynuacji w nieznanej postaci = "niezgodny protokół" (recenzja R2). Wskaźnik kontynuacji na dziesiątej stronie = błąd "too many meeting artifacts, pass start and end", nie wybór z niepełnej listy (recenzja R3). Zostają zasoby `TranscriptV2`; przy `start`/`end` tylko te, których `[startTime, endTime]` nachodzi na `[start, end]`. Zasoby `Transcript` (starszy rodzaj, 9.6 w docs) są wymienione w wyniku z `supported: false`.
2. Wybór: `transcript`, jeśli podany (brak takiego = `invalid-args`), inaczej najnowszy po `startTime`. Brak transkrypcji = pusty wynik z `reason: "no transcript"`, nie błąd.
3. Gdy `metadata.ownerTenantId` różni się od `tid` konta: błąd "not supported yet: the transcript is stored by another organization" (live: SharePoint innej organizacji odrzuca token, docs 9 "Live results" 9).
4. Origin z `location` musi mieć postać `https://<etykieta>.sharepoint.com` (etykieta `[a-z0-9-]{1,63}`), `driveId` i `driveItemId` tylko znaki `[A-Za-z0-9!_-]`, do 256 znaków; inaczej "niezgodny protokół".
5. `media/transcripts`; pusta lista albo brak pozycji z `isVisible` = wynik z `reason: "transcript not ready"` (Teams jeszcze przetwarza nagranie), nie błąd (recenzja R3). Wybór pozycji: `id == metadata.transcriptId`, inaczej `isDefault`, inaczej pierwsza z `isVisible`. Związek `transcriptId` z MCPS i `id` z ODSP to "Needs live capture" 13. `id` wybranej pozycji przechodzi tę samą kontrolę co `driveId` (krok 4: dozwolone znaki, długość) i trafia do ścieżki zakodowany procentowo; inaczej "niezgodny protokół" (recenzja R2).
6. `streamContent?is=1` (WebVTT, do `MAX_BODY`). Parser: nagłówek `WEBVTT` wymagany; fragmenty = linia czasu `HH:MM:SS.mmm --> HH:MM:SS.mmm` + tekst; mówca z `<v Imię>`; pozostałe tagi usuwane; najwyżej 100 000 fragmentów i 4000 znaków tekstu na fragment (nadmiar obcięty ze znacznikiem); zły format = "niezgodny protokół", nigdy panika.
7. Strona: fragmenty od `cursor`, dopóki suma długości fragmentów zserializowanych do JSON (w bajtach, z ucieczkami znaków) nie przekroczy `max_bytes`, najwyżej 2000 fragmentów (co najmniej jeden fragment; pojedynczy fragment ma najwyżej 4000 znaków, więc kilkadziesiąt KB); `next_cursor` albo `null`. Górna granica `max_bytes` zostawia zapas do limitu wyniku 1 MiB (5.1) na resztę wyniku (recenzje R1, R2). Każda strona pobiera VTT od nowa (ponytail: bez pamięci podręcznej; transkrypcja godzinnego spotkania to ok. 100 KB; pamięć podręczna, gdy pomiar pokaże potrzebę).

Wynik:
```json
{"items": [{"conversation": "19:meeting_...", "transcript": "<id zasobu MCPS>", "start": "...Z",
  "end": "...Z", "supported": true, "selected": true}],
 "language": "pl-pl", "next_cursor": null, "reason": null,
 "untrusted": {"cues": [{"start": "00:00:03.120", "end": "00:00:07.900", "speaker": "...", "text": "..."}]}}
```

Odpowiedzi 401/403 SharePoint po jednym odnowieniu tokenu (także po wyzwaniu CAE, jak w etapie 1) = błąd platformy "access to this transcript was refused"; 404 = "transcript not found". Moduł wysyła w tych narzędziach wyłącznie `GET` (poza istniejącym POST authsvc przy odnawianiu sesji); test to sprawdza.

## 5. Zmiany w rdzeniu i hoście

### 5.1 Dodatkowe narzędzia modułów w rejestrze (`crates/open-mess/src/tools/`)

- **Lista**: `Registry::list()` dokłada narzędzia z manifestów modułów w stanie `ModuleState::Active` (załadowany i zatwierdzony); moduły wyłączone, czekające na zgodę albo padnięte nie wystawiają narzędzi. Lista jest liczona przy każdym `tools/list`. Gdy zmienia się zbiór wystawionych narzędzi (moduł zatwierdzony, włączony, wyłączony, padnięty, podmieniony albo usunięty), demon wysyła `notifications/tools/list_changed` do każdej otwartej sesji MCP: deklaruje `tools.listChanged` w `get_info` (`crates/open-mess/src/api.rs`, dziś tylko `enable_tools()`) i pamięta wszystkie sesje, nie tylko te z kanałem (`triggers/channel.rs` `Sessions` trzyma dziś tylko sesje z kanałem). Most stdio już przekazuje to powiadomienie dalej (`bridge.rs`, `on_tool_list_changed`). Klient, który powiadomienie zignoruje, przy wywołaniu narzędzia niedostępnego modułu dostaje czytelny błąd ("module teams is disabled", jak dziś `tools::module`) (recenzja R2).
- **Nazwa**: `<nazwa modułu>_<nazwa narzędzia>`, z `-` w nazwie modułu zamienionym na `_`. Separator `_`, nie `.`, bo nazwy narzędzi w API modeli muszą pasować do `^[a-zA-Z0-9_-]{1,64}$` (spec 8.3 mówi tylko o przedrostku). Nazwa narzędzia w manifeście: `[a-z][a-z0-9_]{0,31}`, a całość najwyżej 64 znaki; narzędzie z inną nazwą, nazwa równa nazwie narzędzia wspólnego albo powtórzona w innym module nie jest wystawiana, a `status` pokazuje to przy module. Moduł dostaje w `call` nazwę bez przedrostka.
- **Klasa**: wystawiane są tylko narzędzia `writes: false`. Narzędzie piszące z manifestu nie jest wystawiane ("not supported yet: module write tools"), bo zatwierdzanie (spec 8.6) wymaga opisu celu, którego `tool-decl` nie ma - osobny spec, gdy pojawi się potrzeba.
- **Schemat**: `input-schema` musi być obiektem JSON z `"type": "object"`, obiektem `properties` (najwyżej 32 klucze, nazwy `[a-z][a-z0-9_]{0,31}`), opcjonalną tablicą `required` złożoną z kluczy `properties` i bez `additionalProperties` innego niż `false`; inaczej narzędzie nie jest wystawiane, a `status` podaje przyczynę (recenzja R2).
- **Argumenty**: obiekt JSON najwyżej 16 KiB. Rdzeń publikuje schemat modułu z dodanym opcjonalnym `account` (moduł nie może sam deklarować `account` - taki manifest nie wystawia narzędzia). Rdzeń sprawdza: obiekt, brak kluczy spoza `properties` schematu, obecność kluczy z `required`, `account` i `conversation` jako tekst. Typy i zakresy pozostałych wartości sprawdza moduł (`invalid-args`). Komentarz w `wit/module.wit` zostaje poprawiony, żeby mówił dokładnie to.
- **Konto**: `account` podane - to konto, musi należeć do tego modułu; pominięte - jedyne konto modułu, które agent może czytać; kilka takich = błąd "name the account" z listą identyfikatorów.
- **Uprawnienia** (spec 8.6): przed wywołaniem `readable()` jak dla narzędzi wspólnych (z `conversation` w argumentach - prawo czytania tej rozmowy, bez - prawo czytania konta gdziekolwiek). Po wywołaniu `drop_unreadable()` na `items[].conversation`; element bez `conversation` zostaje tylko przy prawie czytania na poziomie konta. Dla spotkań: przy uprawnieniu zawężonym do wybranych rozmów agent widzi tylko spotkania, których czat jest na liście, i tylko ich transkrypcje.
- **Obcinanie**: moduł nie obcina list według liczby elementów przed filtrem uprawnień. Narzędzie, które potrzebuje limitu, deklaruje argument `limit`, a rdzeń obcina `items` dopiero po `drop_unreadable()`, jak dziś `call_account` (recenzja R1).
- **Wynik**: tekst JSON z modułu najwyżej 1 MiB (większy = błąd modułu), musi być obiektem; rdzeń dokłada `account` do `items[]` i notkę `untrusted_notice`, gdy wynik ma gdziekolwiek klucz `untrusted` (`shared::with_notice`). Innych pól rdzeń nie interpretuje - moduł jest zaufany dla swojego konta (spec 15).
- Limit czasu wywołania bez zmian (`CALL_TIMEOUT`).

### 5.2 Wzorce zakresów WAM (`crates/open-mess-host/src/sign_in.rs`, `hosts.rs`)

Token SharePoint ma odbiorcę zależnego od organizacji (`https://<tenant>-my.sharepoint.com/.default`), więc nie da się go wpisać dosłownie do manifestu modułu budowanego raz dla wszystkich.

- Wpis `wam.scopes` może być wzorcem `https://*.<sufiks>/.default`: `*` zastępuje dokładnie jedną etykietę DNS `[a-z0-9-]{1,63}`, `<sufiks>` podlega tej samej regule co wzorce hostów (poniżej domeny rejestrowalnej, spec 8.3 R7). Inne miejsca `*` albo inne ścieżki niż `/.default` = manifest odrzucony przy ładowaniu.
- `allowed()`: zakres bez `*` w manifeście porównywany jest dosłownie, jak dziś - bez żadnej reguły hostów (dotychczasowe zakresy, np. `https://api.spaces.skype.com/.default` czy zakres MCPS w postaci identyfikatora aplikacji, mają odbiorców spoza listy hostów i muszą dalej działać). Tylko dopasowanie do wzorca ma dodatkowe warunki: zakres postaci `https://<etykieta>.<sufiks>/.default`, małe litery ASCII, bez portu, użytkownika, zapytania i fragmentu, a `<etykieta>.<sufiks>` musi być dozwolony przez listę hostów modułu (recenzja R1).
- Okno przy zgodzie pokazuje wzorzec z opisem "token do dowolnej witryny SharePoint/OneDrive Twojej organizacji".

### 5.3 Moduł Teams (`modules/teams/src/`)

- `manifest.rs`: zakresy WAM + `6bc3b958-689b-49f5-9006-36d165f30e00/.default` (MCPS) i `https://*.sharepoint.com/.default`; hosty bez zmian (`teams.microsoft.com` obejmuje middle tier i MCPS, `*.sharepoint.com` już jest); `extra_tools`: dwa narzędzia z sekcji 4 ze schematami. Zmiana manifestu wymaga ponownej zgody w oknie (spec 8.3) - oczekiwane.
- `authsvc.rs`: katalog dostaje opcjonalne `mcp_service` z `regionGtms.mcpService` (tylko https); brak klucza = `teams_get_transcript` zwraca "not supported yet: no meeting artifacts service for this account".
- `session.rs` i `api.rs` (`establish`): `Session` przechowuje token Skype z authsvc i jego ważność (`expiresIn`; `SecretString`, nigdy w logach ani `Debug`). Przed zapytaniem do middle tier token ważny krócej niż margines odnowienia (`RENEW_BEFORE_MS`, jak tokeny WAM) jest odnawiany ponowną wymianą authsvc z cichym tokenem `https://api.spaces.skype.com` (odświeża też katalog). 401 z middle tier = jedna taka wymiana i ponowienie, potem błąd jak w etapie 1 (recenzja R1).
- `api.rs`: nowe usługi `MiddleTier` (token `https://api.spaces.skype.com` + `x-skypetoken`, `x-ms-client-type`), `Mcps` (token MCPS, `x-ms-correlation-id`), `SharePoint(origin)` (token `<origin>/.default`). Ponawianie i limity jak w etapie 1 (`retry.rs`, spec 9.2).
- Nowy `meetings.rs`: dekoder kalendarza, dekoder MCPS, wybór transkrypcji, parser WebVTT, stronicowanie; `tools.rs` (`call`) rozdziela `list_meetings` i `get_transcript`.

## 6. Uprawnienia i przepływ treści

- Oba narzędzia czytają: podlegają prawu czytania konta/rozmowy (5.1), nigdy nie piszą na platformę i nie zmieniają stanu "przeczytane" (tylko GET do kalendarza, MCPS i SharePoint).
- Nic nie jest zapisywane lokalnie: brak pamięci podręcznej transkrypcji, brak plików, w logach tylko kody statusu i liczby (spec 8.4).
- Treść transkrypcji, tematy i nazwiska trafiają tylko do agentów podłączonych przez użytkownika (i dalej do dostawców ich modeli), jak wiadomości (spec 5, AGENTS.md) - z zastrzeżeniem otwartej decyzji D2 (sekcja 10).
- Brak wyzwalaczy w tym etapie ("transkrypcja gotowa" - poza zakresem).

## 7. Kroki

0. **Live capture przy właścicielu** (docs "Needs live capture" 13-16), tylko odczyt, tylko kody, klucze i liczby: kształt `paginationInfo` MCPS i odpowiedź kalendarza dla gęstego okna 31 dni (czy pojawia się stronicowanie), pobranie transkrypcji we własnej organizacji (lista ODSP, `streamContent?is=1`, związek `transcriptId` z `id`), MCPS dla spotkania cyklicznego i bez transkrypcji, kształt `Media_CallTranscript`, czy pobranie transkrypcji liczy się jako wyświetlenie nagrania, alfabet i długość identyfikatorów `19:meeting_...` (tylko klasy znaków i długości, nie wartości), zbiór wartości `myResponseType` i `eventType`. Wyniki do `docs/protocols/teams.md`. Jeśli pobranie wymaga innej drogi niż 4.2 kroki 4-6 albo kalendarz okaże się stronicowany, spec wraca do review.
1. Host: wzorce zakresów WAM (5.2) z testami.
2. Rdzeń: dodatkowe narzędzia w rejestrze (5.1); moduł testowy `modules/test-echo` deklaruje jedno narzędzie czytające do testów rejestru; poprawiony komentarz w `wit/module.wit`.
3. Teams: `teams_list_meetings` z testami dekodera na oczyszczonych, syntetycznych odpowiedziach o kształcie z live check.
4. Teams: `teams_get_transcript` z testami (MCPS, wybór, inna organizacja, 401/404, parser VTT).
5. Test akceptacyjny na żywo przy właścicielu (sekcja 9), wydanie dopiero po decyzji D2.

## 8. Dotknięte pliki

- `wit/module.wit` (komentarz o walidacji argumentów),
- `crates/open-mess-host/src/sign_in.rs`, `crates/open-mess-host/src/hosts.rs` (wzorce zakresów), `crates/open-mess-host/src/runtime.rs` (`Engine::load_bytes`: walidacja wzorców zakresów przy ładowaniu, obok nazwy i hostów), testy w `crates/open-mess-host/tests/sign_in.rs` i `crates/open-mess-host/tests/load.rs`,
- `crates/open-mess/src/tools/mod.rs`, `crates/open-mess/src/tools/shared.rs` (rejestr dodatkowych narzędzi), `crates/open-mess/src/api.rs` i `crates/open-mess/src/modules.rs` (powiadomienie o zmianie listy narzędzi), `crates/open-mess/src/ui/` i `ui/app.js` (opis wzorca przy zgodzie), testy w `crates/open-mess/tests/shared_tools.rs` i `crates/open-mess/tests/modules.rs`,
- `modules/test-echo/src/lib.rs` (narzędzie testowe),
- `modules/teams/src/manifest.rs`, `authsvc.rs`, `session.rs`, `api.rs`, `tools.rs`, nowy `meetings.rs`, testy jednostkowe w tych plikach (jak dziś w module),
- `docs/protocols/teams.md` (krok 0).

## 9. Kryteria akceptacji

- `tools/list` w MCP i `om` w CLI pokazują `teams_list_meetings` i `teams_get_transcript` dla aktywnego modułu Teams; po wyłączeniu modułu znikają, a otwarta sesja MCP (bezpośrednio i przez most stdio) dostaje `notifications/tools/list_changed`.
- Narzędzie piszące, nazwa niezgodna z regułą, kolizja nazw, `account` w schemacie modułu albo schemat niezgodny z 5.1: narzędzie nie jest wystawiane, `status` mówi dlaczego (test z `test-echo`).
- Nieznany argument, brak wymaganego, wynik powyżej 1 MiB albo nie-obiekt: czytelny błąd, demon działa.
- Uprawnienie czytania zawężone do jednej rozmowy: `teams_list_meetings` zwraca tylko spotkanie z tym czatem, także gdy w zakresie dat jest przed nim kilkaset innych zdarzeń; odrzucone zaproszenia i terminy bez spotkania Teams nie pojawiają się nigdy; `teams_get_transcript` dla innego czatu = "reading this conversation is not allowed".
- WAM: dotychczasowe zakresy dosłowne (`https://api.spaces.skype.com/.default`, `https://ic3.teams.office.com/.default`, `https://outlook.office.com/.default`) i zakres MCPS przechodzą jak dotąd (test regresji); zakres `https://contoso-my.sharepoint.com/.default` przechodzi przy wzorcu w manifeście; `https://a.b.sharepoint.com/.default`, `https://sharepoint.com/.default`, `https://x.sharepoint.com.evil.example/.default`, inna ścieżka albo port - odmowa bez kontaktu z Windows (test z udawanym WAM).
- `teams_list_meetings` na koncie właściciela: spotkania z ostatnich 7 dni z `conversation` dla spotkań Teams; temat tylko pod `untrusted`; po wygaśnięciu tokenu Skype (ok. 75 minut po wymianie authsvc) nadal działa dzięki odnowieniu.
- `teams_get_transcript` dla spotkania własnej organizacji z transkrypcją: fragmenty z mówcami i czasami, `next_cursor` przy długiej transkrypcji; dla spotkania innej organizacji: błąd "stored by another organization"; dla spotkania bez transkrypcji: `reason: "no transcript"`.
- Parser VTT: zły nagłówek, ucięty plik, brak `-->`, ogromna liczba fragmentów, bardzo długa linia, nie-UTF-8 - błąd albo obcięcie, nigdy panika (testy regresji).
- Żadne z narzędzi nie wysyła zapytania innego niż GET do kalendarza, MCPS i SharePoint (test na nagranych wywołaniach `http.fetch`).
- Oficjalny klient Teams działa jak dotąd; stan "przeczytane" czatów spotkań bez zmian.

## 10. Otwarte decyzje właściciela

- **D1. Lokalna transkrypcja dźwięku (np. Whisper)** dla spotkań bez transkrypcji Teams i innych platform (spec 12). Nie wchodzi w ten spec. Do rozstrzygnięcia: czy w ogóle, a jeśli tak - nagrywanie tylko na wyraźne polecenie użytkownika dla danego spotkania czy automatycznie, informacja dla uczestników, model wyłącznie lokalny, gdzie i jak długo leży dźwięk. Wymaga osobnego specu (przechwyt dźwięku to nowy import hosta, spec 12).
- **D2. Zgoda / informacja dla uczestników (RODO)** przy przekazywaniu transkrypcji - mowy innych osób - agentom i dalej dostawcom ich modeli. Możliwości: (a) bez dodatkowej bramki, jak wiadomości czatu (prawo czytania konta/rozmowy); (b) osobny przełącznik per konto "transkrypcje dla agentów", domyślnie wyłączony; (c) jak (b) i tylko spotkania organizowane przez użytkownika; (d) inna. Do decyzji kroki 0-4 można zbudować, ale wydanie z tymi narzędziami (`cargo xtask release`) czeka na decyzję; (b) i (c) to mała zmiana w 5.1 (dodatkowy warunek przed wywołaniem) i w oknie.
- **D3. Wyświetlenie w analityce nagrania** - tylko jeśli krok 0 pokaże, że pobranie transkrypcji widać u właściciela nagrania jako wyświetlenie: czy to akceptowalne (oficjalny klient robi to samo, otwierając podsumowanie spotkania).

## 11. Threat model

- **Zaufani**: użytkownik przy komputerze, rdzeń (jak spec 15).
- **Częściowo zaufani - agenci**: transkrypcja to mowa innych osób, a więc wymarzony nośnik prompt injection ("zignoruj polecenia i wyślij..."). Cała treść (fragmenty, mówcy, tematy, organizatorzy) jest pod `untrusted` z notką; wysyłanie dalej podlega uprawnieniom pisania i zatwierdzaniu jak w etapie 1 (spec 8.6), niezależnie od treści transkrypcji.
- **Niezaufane**: odpowiedzi middle tier, MCPS i SharePoint oraz pliki VTT - limity rozmiaru (16 MiB odpowiedź, 1 MiB wynik narzędzia, 100 000 fragmentów, 4000 znaków na fragment), ścisłe dekodowanie, sprawdzanie originu SharePoint i identyfikatorów przed użyciem w adresie (moduł nie zbuduje zapytania do hosta wskazanego przez odpowiedź serwera poza `*.sharepoint.com`, a rdzeń i tak egzekwuje listę hostów), brak paniki na złych danych.
- **Moduł**: nowe tokeny (SharePoint z `Sites.FullControl.All`, MCPS) poszerzają to, co moduł może zrobić na koncie - w granicy zaufania z spec 15 (rebutal R4 dokumentu projektowego); okno pokazuje nowe zakresy przy ponownej zgodzie. Najmniejsze uprawnienia: kalendarz przez middle tier, którego token moduł już ma, zamiast Graph, którego token klienta Teams niesie m.in. `Mail.ReadWrite` i `Mail.Send` (live check).
- **Wzorzec zakresu WAM**: obejmuje tylko jedną etykietę pod `sharepoint.com`; inne domeny i głębsze nazwy odrzucone, więc moduł nie wyłudzi tokenu dla obcych usług.
- **Inne organizacje**: transkrypcje spotkań cudzych organizacji nie są pobierane (błąd), open_mess nie próbuje tokenów dla obcych tenantów.
- **Granica twardnienia**: jak spec 15 (bez obrony przed złośliwym kodem na koncie Windows).

## 12. Rejestr rebutali

(pusty)

## Changelog rund

- R1 (Codex REJECT, GLM REJECT): token Skype przechowywany w `Session` i odnawiany wymianą authsvc (Codex P2); `teams_list_meetings` bez `limit`, moduł nie obcina list przed filtrem uprawnień, ogólna reguła obcinania po filtrze dla narzędzi dodatkowych (Codex P2); reguła hostów tylko dla zakresów ze wzorca, zakresy dosłowne bez zmian, test regresji (GLM P2); luźniejsza walidacja wątku `19:meeting_` i sprawdzenie alfabetu w kroku 0 (GLM P3); błąd przy braku `middleTier` albo tokenu Skype (GLM P3); budżet strony transkrypcji z narzutem na fragment i limitem 2000 fragmentów, `max_chars` do 150000, żeby strona mieściła się w 1 MiB (własne).
- R2 (Codex REJECT, GLM REJECT): powiadomienie `tools/list_changed` do wszystkich sesji MCP przy zmianie modułów (Codex P2); wskaźniki kontynuacji kalendarza = niezgodny protokół, stronicowanie MCPS według kształtu z kroku 0, krok 0 sprawdza gęste okno (GLM P2); kontrola `id` transkrypcji ODSP przed użyciem w ścieżce (GLM P3); budżet strony w bajtach zserializowanego JSON (`max_bytes`) zamiast znaków (GLM P3); walidacja `input-schema` przy wystawianiu narzędzia (GLM P3).
- R3 (Codex REJECT, GLM APPROVE): `teams_list_meetings` zwraca tylko spotkania Teams bez odpowiedzi "odrzucone", inne wpisy kalendarza wcale (Codex P2); wskaźnik kontynuacji na dziesiątej stronie MCPS = błąd (Codex P2, GLM P3); pusta lista ODSP = `reason: "transcript not ready"` (GLM P3); `runtime.rs` i `tests/load.rs` w dotkniętych plikach (GLM P3).
