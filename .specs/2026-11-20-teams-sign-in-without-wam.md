# open_mess - logowanie Teams bez WAM (etap 2 wtyczki Paperclip)

Data: 2026-11-20
Status: R11, zatwierdzony (Codex i GLM, runda R10; rundy R1-R2 autora były bez nich); kroki 1-3 zrobione; krok 4 (test na żywo 2026-11-20, `docs/protocols/teams.md`, "Live results" 21) zrobiony dla logowania, odtworzenia po restarcie, IC3, middle tier i `outlook.office.com`; MCPS, zasób nagrań i SharePoint czekają na próbę na żywo ("Needs live capture" 2)
Powiązane: `.specs/2026-11-03-paperclip-plugin.md` (rozdz. 3.3, 6, 8), `.specs/2026-10-26-open-mess-design.md` (8.3, 8.4, 9.1, 9.5), `.specs/2026-11-18-account-restore-retry.md`, `docs/protocols/teams.md` (1.1, 1.3, 1.4, "Needs live capture" 2), `wit/module.wit`, `modules/teams/src/{api,session,manifest}.rs`, `crates/open-mess-host/src/{wam,sign_in,bindings,secrets}.rs`, `crates/open-mess/src/accounts.rs`.

## 1. Cel

Moduł Teams loguje konto i odnawia tokeny tam, gdzie nie ma WAM (Linux, tryb serwerowy z etapu 1): kod autoryzacji + PKCE (`S256`) przez prośbę `open-page`, potem token odświeżania w sekretach modułu. To druga droga z 9.1 projektu ("osadzona strona logowania") i etap 2 specu wtyczki Paperclip.

Na Windows nic się nie zmienia: WAM jest, więc moduł loguje się i odnawia tokeny jak dotąd.

## 2. Zakres i decyzje

1. **Wybór drogi po nowej funkcji hosta, nie po błędzie.** Host mówi modułowi, czy ma WAM (`sign-in.wam-available`, 4.1). Moduł nie zgaduje po tekście błędu `wam-token`.
2. **Tylko gdy WAM nie ma.** Na Windows strona logowania nie jest drogą zapasową po odmowie WAM; to osobna decyzja (rozdz. 10).
3. **Identyfikator i przekierowanie jak w 9.1:** desktop `1fec8e78-bce4-4aaf-ab1b-5451cc387264`, `https://login.microsoftonline.com/common/oauth2/nativeclient`. Tę drogę sprawdził właściciel na żywo 2026-11-03 (spec wtyczki 3.3): wymiana kodu, odświeżenie na innego odbiorcę, authsvc i lista rozmów - 200 z adresu serwera.
4. **Urząd `organizations`** dla autoryzacji, wymiany i odświeżania, jak `AUTHORITY_URL` w `wam.rs`: tylko konta służbowe, jak WAM.
5. **Token odświeżania to sekret konta** (`secrets.set(account, "refresh-token", ...)`), jak przewiduje 9.1 projektu. Tokeny dostępu i token Skype zostają tylko w pamięci instancji (`Session`), jak dziś.

## 3. Kontekst (sprawdzony w kodzie, `main` 9241172)

- `modules/teams/src/api.rs`: każdy token przechodzi przez `wam(scope, interactive, claims, account)`. Woła ją `sign_in` (interaktywnie), `open` (przy `restore` i po przeładowaniu instancji, `with_session`) i `renew` (wygasły token i wyzwanie CAE w `send`, `middle_tier`, `conversation`, `sharepoint`). `establish` robi wymianę authsvc i tworzy `Session`. `exchange` ma politykę ponowień z docs 8 i budżet wywołania (`CALL_BUDGET` 20 s).
- `modules/teams/src/session.rs`: `claims(token)` czyta `oid`, `name`, `upn`, `tid`, `exp` bez sprawdzania podpisu (token przyszedł z WAM). `Session::keep` trzyma token per zakres.
- `modules/teams/src/manifest.rs`: `login.microsoftonline.com` jest już w `hosts` (komentarz "1.3: authorization code redemption and token refresh") i w `sign_in_hosts`. Manifest się nie zmienia, więc przegląd hostów przy zatwierdzaniu pokazuje to samo. Nowy plik `teams.wasm` podmieniony ręcznie (wolumen serwera) i tak wymaga ponownego zatwierdzenia (zgoda wiąże skrót pliku i manifestu, `modules.rs`; spec wtyczki 5.4); do tego czasu nowy plik nie działa (po `rescan_modules` w działającym demonie poprzednia instancja dalej obsługuje konta; po restarcie demona moduł czeka nieaktywny). Tak ma być. Podpisane wydanie z aktualizatora zapisuje zgodę samo, gdy manifest różni się tylko wersją (`update/apply.rs`, `version_only_change`), jak dla każdego modułu.
- `crates/open-mess-host/src/wam.rs`: poza Windows `token_silent`, `token_interactive` i `accounts` zwracają `refused("Windows account sign-in exists only on Windows")`.
- `crates/open-mess-host/src/sign_in.rs`: `open_page` przekazuje prośbę do okna (Windows) albo do API administracyjnego (etap 1: `sign_in_request` z `open_url`, `sign_in_answer` z wklejonym adresem, sprawdzonym względem `redirect_prefix`). Limit tekstu prośby `MAX_PROMPT_TEXT` 8 KiB.
- `crates/open-mess-host/src/bindings.rs`: `secrets::Host` bez ograniczeń konta; `crates/open-mess-host/src/secrets.rs` liczy sekrety (`MAX_SECRETS` 64 na moduł) i usuwa je z kontem (`delete_account_secrets`). Na Linuksie magazynem jest `FileStore` z etapu 1.
- Testy: `crates/open-mess-host/tests/teams/` ładuje zbudowany `open_mess_teams.wasm` w prawdziwym hoście z fałszywym serwerem (`fake.rs`, wszystkie hosty z pętli zwrotnej) i fałszywym WAM (`Engine::with_fake_wam`).
- Moduł ma `wasi:random`; `api::random()` bierze 64 bity z `RandomState` (klucze SipHash z `wasi:random`, kolejne wartości z tych samych kluczy). Na identyfikatory wystarcza, na PKCE nie (4.3.1).

## 4. Projekt

### 4.1 Kontrakt (`wit/module.wit`)

W interfejsie `sign-in` nowa funkcja:

```wit
// Whether this host has Windows WAM. Without it `wam-token` always fails and a module signs in
// another way (spec 2026-11-20-teams-sign-in-without-wam).
wam-available: func() -> bool;
```

Nowa funkcja w istniejącym interfejsie nie psuje modułów zbudowanych wcześniej: importują one mniej funkcji, niż host definiuje (projekt, rozdz. 12: kontrakt hosta rozszerzalny bez psucia istniejących modułów). Sprawdzone ręcznie 2026-11-24: test-echo i moduł Teams zbudowane z `main` 83136f9 (WIT bez `wam-available`) ładują się w hoście z tą zmianą i wołają importy `sign-in`. Wersja pakietu zostaje `0.1.0`. Nowy wariant `sign-in-error` by je zepsuł (rejestr rebutali, B1).

W drugą stronę nie działa: moduł Teams z tej zmiany importuje `wam-available`, więc host bez tej funkcji (open_mess 0.2.0 i starsze) go nie załaduje (błąd ładowania, moduł nieaktywny, konta jak przy wyłączonym module). Moduły wychodzą razem z hostem (wydanie Windows, obraz z etapu 4 specu wtyczki), więc na serwerze kolejność to: najpierw `om` z tą zmianą, potem `teams.wasm`; plik podmieniony ręcznie na starszym hoście czeka na aktualizację demona.

### 4.2 Host (`crates/open-mess-host`)

- `wam-available` zwraca `cfg!(windows)`.
- Testy (`test-support`): z fałszywym WAM zwraca `true` jak dziś. Nowa opcja `Engine::without_wam()` (tylko `test-support`) daje `false` i usuwa fałszywy WAM, żeby testy przeszły drogą bez WAM na każdej platformie, także w CI Windows. Z nią `wam-token` odmawia (`refused`, jak poza Windows) i nigdy nie sięga do prawdziwego WAM na testowym Windows.
- `open_page` i kontrola hostów działają jak w etapie 1. Sekrety: jedna poprawka w 4.5.

### 4.3 Moduł Teams (`modules/teams/src/`)

Nowy plik `oauth.rs`: czyste funkcje (budowa adresu, PKCE, odczyt przekierowania, odczyt odpowiedzi tokenu, scalanie claims), testowane natywnie. `api.rs` dostaje źródło tokenów sesji.

#### 4.3.1 PKCE i `state`

- `code_verifier`: 32 bajty z `getrandom` (backend `wasi:random` na `wasm32-wasip2`), base64url bez dopełnienia = 43 znaki (RFC 7636 4.1).
- `code_challenge` = base64url(SHA-256(verifier)), metoda `S256`. SHA-256 z `sha2` (już w `Cargo.lock`, licencja MIT OR Apache-2.0).
- `state`: 16 bajtów z `getrandom`, base64url.
- Nowe zależności modułu: `getrandom` 0.3 i `sha2` 0.10, obie w `Cargo.lock`, MIT OR Apache-2.0. Nazwane w PR.
- Brak losowości (`getrandom` zwraca błąd) kończy logowanie błędem `platform`, nigdy wartością stałą.

#### 4.3.2 Logowanie (`sign_in`, gdy `wam-available` = false)

1. Adres: `https://login.microsoftonline.com/organizations/oauth2/v2.0/authorize` z parametrami (kodowanie `url::form_urlencoded`):
   `client_id=<CLIENT_ID>`, `response_type=code`, `response_mode=query`, `redirect_uri=<NATIVECLIENT>`, `scope=https://api.spaces.skype.com/.default offline_access openid profile`, `code_challenge`, `code_challenge_method=S256`, `state`, `prompt=select_account`, `claims={"access_token":{"xms_cc":{"values":["CP1"]}}}` (CAE jak oficjalny klient, docs 1.3).
2. `sign_in::open_page(adres, NATIVECLIENT)`. Odpowiedź to adres, który zaczyna się od `redirect_prefix` (host to sprawdza, etap 1).
3. Odczyt przekierowania (`oauth::redirect`):
   - `state` różny od wysłanego -> `auth-required("the sign-in answer is not for this sign-in")`;
   - `error` -> `auth-required` z kodem błędu i numerem `AADSTS` z `error_description`, bez reszty opisu (może nieść adres e-mail);
   - brak `code` -> `auth-required`.
4. Wymiana: `POST https://login.microsoftonline.com/organizations/oauth2/v2.0/token`, `Content-Type: application/x-www-form-urlencoded`, pola `client_id`, `grant_type=authorization_code`, `code`, `redirect_uri`, `code_verifier`, `scope` (jak w 1.), `claims` (jak w 1.). Bez nagłówka `Origin` (to nie rejestracja SPA, docs 1.3). Przez `exchange` bez tokenu (ponowienia 429/5xx z docs 8; wymiana kodu nie jest zapisem na platformie).
5. Odpowiedź (`oauth::token_response`): `access_token` i `refresh_token` wymagane, `expires_in` opcjonalne. Brak któregoś -> `incompatible`. Błąd 400/401 -> 4.3.5.
6. `claims(access_token)` -> konto; `establish` jak dziś (authsvc).
7. `secrets::set(oid, "refresh-token", refresh_token)`. Błąd zapisu kończy logowanie błędem `platform("cannot keep the sign-in: ...")`: bez tokenu odświeżania konto nie przetrwa restartu.
8. Sesja dostaje źródło `Refresh` (4.3.3) i token authsvc z czasem z `exp` (albo `expires_in`, gdy `exp` brak).

Czas użytkownika na stronie logowania nie liczy się do budżetu: `begin()` po `open_page`, jak po dialogu WAM.

Moduł obsługuje jedno wywołanie naraz (`actor.rs`), więc póki strona czeka na wklejony adres (najwyżej `SIGN_IN_BUDGET`, 10 min, albo do anulowania), narzędzia i odpytywanie innych kont tego modułu czekają. Tak jest dziś przy każdym logowaniu z prośbą (kod urządzenia, dialog WAM, strona w oknie Windows). Na serwerze z jednym kontem Teams to nic nie zmienia; logowanie drugiego konta albo ponowne logowanie to rzadka czynność właściciela przy panelu (rejestr rebutali, B7).

#### 4.3.3 Źródło tokenów sesji

`Session` dostaje pole `source: Source { Wam, Refresh }`. `renew` i `open` wybierają drogę:

- `open(account)` (restore, przeładowana instancja): z WAM (`wam-available` = true) -> WAM jak dziś, bez sięgania do sekretów; bez WAM -> sekret `refresh-token` konta i odświeżenie na `AUTHSVC_SCOPE`, źródło `Refresh`. Bez WAM i bez sekretu -> `auth-required("sign in again")`: rdzeń zapisuje wtedy stan `auth-required` (`restore_where`), a ponawianie kont `offline` (`.specs/2026-11-18-account-restore-retry.md`, PR #17, jeszcze nie w `main`) go nie bierze, więc nie ma pętli. Błąd sieci przy odświeżeniu to `platform` -> `offline`, dokładnie jak nieudany `restore` z WAM: z PR #17 konto wraca samo, bez niego wraca jak dziś (restart, `restore_after_crashes`, ponowne logowanie). Ten spec nie zmienia, co rdzeń robi z kontem `offline`.
- Droga WAM nie czyta ani nie usuwa żadnego sekretu, więc na Windows nic się nie zmienia (ani wywołania Menedżera poświadczeń, ani testy z fałszywym WAM).
- `renew(session, scope, claims)`: `Wam` -> jak dziś; `Refresh` -> 4.3.4.
- Sprawdzenie `oid` nowego tokenu z kontem sesji zostaje w obu drogach.

#### 4.3.4 Odświeżenie

- `POST .../organizations/oauth2/v2.0/token`, pola `client_id`, `grant_type=refresh_token`, `refresh_token`, `scope=<zakres> offline_access`, `claims`.
- `claims` = `{"access_token":{"xms_cc":{"values":["CP1"]}}}` scalone z claims wyzwania CAE, jeśli jest (`oauth::merge_claims`: obiekt `access_token` z wyzwania + `xms_cc`; wyzwanie, które nie jest obiektem JSON, idzie bez zmian, jak dziś do WAM).
- Odpowiedź z nowym `refresh_token` -> `secrets::set` (rotacja), ale tylko gdy w sekretach wciąż leży token, którym odświeżano. Konto usunięte w trakcie odświeżania (usuwanie skasowało sekret, także gdy moduł wyłączono i `sign_out` nie doszedł) nie dostaje sekretu z powrotem, a konto zalogowane ponownie w innej instancji zachowuje nowszy token. Zostaje okno między odczytem a zapisem: dwa lokalne wywołania hosta zamiast całej odpowiedzi Microsoftu (4.5, "Poza zakresem"). Nieudany zapis: ostrzeżenie w logu (bez wartości), token dostępu użyty; w sekretach zostaje stary token odświeżania albo, gdy magazyn zapisał mimo błędu (`FileStore`: plik na miejscu, synchronizacja katalogu się nie udała), już nowy. Oba działają.
- Odpowiedź bez `refresh_token` -> stary zostaje.
- Odbiorcy: każdy z tabeli 1.4, których moduł używa (`SCOPES` z `manifest.rs`, SharePoint organizacji). Spec wtyczki 3.3 potwierdził odświeżenie z tokenu wydanego dla `api.spaces.skype.com` na `ic3.teams.office.com`; pozostali odbiorcy - krok 4 (test na żywo).

#### 4.3.5 Błędy Microsoftu

Odpowiedź 400/401 z JSON `{"error": ..., "error_description": ..., "error_codes": [...]}`:
- `invalid_grant`, `interaction_required`, `consent_required`, `login_required` -> `auth-required("Microsoft asks to sign in again (AADSTS<kod>)")`; dalej jak każdy `auth-required` (`with_session` zgłasza `SessionState::AuthRequired`). Sekret zostaje: użytkownik loguje się ponownie i nowy go nadpisze.
- inny `error` -> `platform` z kodem i `AADSTS`.
- ciało nie jest takim JSON -> `platform("the sign-in service answered HTTP <status>")`.

Do komunikatów trafia tylko `error`, liczba z `AADSTS` i status; nigdy `error_description` w całości, token, kod ani adres.

#### 4.3.6 Wylogowanie

`sign_out` jak dziś. Sekret `refresh-token` usuwa rdzeń razem z kontem (`delete_account_secrets`). Bez WAM moduł dodatkowo woła `secrets::delete(account, "refresh-token")`, żeby nie zależeć od kolejności; z WAM nie (4.3.3).

### 4.4 Limity

- Adres autoryzacji ma ok. 700 znaków, poniżej `MAX_PROMPT_TEXT`.
- Jeden sekret na konto (`MAX_SECRETS` 64 na moduł).
- Ciało odpowiedzi tokenu: `MAX_BODY` hosta; moduł czyta tylko znane pola.

### 4.5 Sekret, który nie zostaje bez konta

Token odświeżania to pierwszy sekret, który żyje latami, więc dwie istniejące ścieżki nie mogą go zostawić poza zasięgiem usuwania konta:

1. **Rdzeń (`Accounts::sign_in`).** Moduł zapisuje sekret w trakcie `sign_in`, zanim rdzeń zapisze konto. Gdy `add` się nie uda (identyfikator, którego rdzeń nie przyjmuje, baza stanu odrzuca zapis), konto, którego open_mess nie ma (`get` daje `None`), idzie tą samą drogą co dziś konto spoza open_mess po odmowie: `sign_out` w module i `delete_account_secrets`. Konto, które open_mess ma, zostaje nietknięte. Konto w trakcie usuwania (`removing`) idzie tą samą drogą co konto, którego nie ma: jego usuwanie mogło już skasować sekrety, zanim moduł zapisał nowe, a przy module wyłączanym w tym czasie pomija `sign_out` i nie czeka na wywołanie `sign_in`. Podwójne `sign_out` i podwójne usunięcie sekretów nic nie psują.
2. **Host (`secrets::set`).** Magazyn może zapisać sekret i mimo to zgłosić błąd (`FileStore`: plik na miejscu, synchronizacja katalogu się nie udała). Dziś host wtedy zapomina nazwę nowego sekretu, a wartość zostaje w pliku poza `delete_account_secrets`. Po zmianie host najpierw usuwa sekret; nazwę zapomina tylko, gdy usunięcie się udało albo sekretu nie ma. Inaczej nazwa zostaje i usunięcie konta go znajdzie.
3. **`FileStore::write`.** Zapis idzie do `secrets.json.tmp`, potem `rename`. Gdy zapis, `sync_all` albo `rename` się nie uda, plik tymczasowy (pełna kopia sekretów, także nowego) jest usuwany, zanim błąd wróci; dziś zostaje do następnego zapisu.

Poza zakresem (B8, B10-B12): awarie, po których rdzeń nie wie, czy ma konto, albo nie może nic zmienić na dysku: proces ubity między zapisem sekretu a zapisem wiersza konta, wywołanie `sign_in` przerwane po zapisie sekretu (rdzeń nie dostał identyfikatora konta), baza stanu, z której nie da się nawet czytać (`get` też zawodzi; usunięcie sekretów konta, które open_mess może mieć, zniszczyłoby działające konto, więc rdzeń ich nie usuwa), system plików tylko do odczytu (ani `rename`, ani usunięcie pliku tymczasowego nie działa). 4.5 sprząta w zwykłym toku, nie po takich awariach. Tak samo jest dziś z każdym sekretem modułu (np. konto Windows wybrane w WAM). Ponowne zalogowanie tego samego konta nadpisuje ten sam klucz i dodaje wiersz, więc usunięcie konta znów go znajdzie. Do tego czasu token leży tylko w magazynie sekretów kontenera `open-mess`, tam gdzie wszystkie (spec wtyczki 8). Sprzątanie sekretów bez konta przy starcie to osobna zmiana rdzenia.

## 5. Kroki

1. WIT + host (4.1, 4.2) z testami. `cargo xtask ci`.
2. Moduł: `oauth.rs` z testami jednostkowymi, potem `api.rs` (4.3.2-4.3.6).
3. Testy integracyjne w `crates/open-mess-host/tests/teams/` (rozdz. 7, pkt 3-8): fałszywy serwer dostaje ścieżki `/organizations/oauth2/v2.0/token`, prośbę `open-page` test odbiera z `SignInPrompts` i odpowiada adresem przekierowania.
4. Test na żywo (9.5, z właścicielem konta przy komputerze, za jego zgodą): demon Linux w kontenerze, logowanie przez API administracyjne, `status`, lista rozmów, odświeżenie tokenu po restarcie demona, odbiorcy z 4.3.4. Tylko statusy i nazwy pól do `docs/protocols/teams.md` ("Live results", "Needs live capture" 2).
5. PR.

## 6. Dotknięte pliki

- `wit/module.wit`
- `crates/open-mess-host/src/{sign_in,bindings,runtime,lib,secrets}.rs`, `crates/open-mess-host/tests/sign_in.rs`
- `crates/open-mess-host/tests/teams/{main,no_wam}.rs`
- `crates/open-mess/src/{accounts,file_store}.rs`, `crates/open-mess/tests/{sign_in,file_store}.rs` (4.5)
- `modules/teams/Cargo.toml`, `modules/teams/src/{oauth,api,session,lib}.rs`
- `modules/test-echo/src/lib.rs`: narzędzie `wam-available`; `sign-out` zapisuje konto, które wylogowano (test 4.5)
- `docs/protocols/teams.md`, `Cargo.lock`

## 7. Kryteria akceptacji

Każde ma test, który bez zmiany nie przechodzi.

1. Host: `wam-available` = `false` poza Windows; `true` z fałszywym WAM; `false` z `Engine::without_wam()`, a `wam-token` wtedy `refused`.
2. `oauth.rs`:
   - wektor z RFC 7636 dodatek B (`dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk` -> `E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM`);
   - verifier: 43 znaki z alfabetu base64url, dwa kolejne różne;
   - adres autoryzacji: urząd, wszystkie parametry, `redirect_uri` dokładnie `nativeclient`;
   - przekierowanie: dobry `code`; zły `state`; `error` z `AADSTS` (opis z adresem e-mail nie trafia do błędu); brak `code`;
   - odpowiedź tokenu: pełna, bez `refresh_token` (logowanie: `incompatible`; odświeżenie: stary zostaje), bez `access_token`, nie-JSON;
   - `merge_claims`: bez wyzwania, z wyzwaniem-obiektem, z wyzwaniem nie-JSON.
3. Integracja, bez WAM: `sign_in` kończy się kontem z tokenu; fałszywy serwer dostał wymianę z `code_verifier` pasującym do `code_challenge` z adresu i `redirect_uri` `nativeclient`; sekret `refresh-token` zapisany.
4. Integracja: przeładowana instancja (`restore`) bierze token odświeżaniem z sekretu, bez prośby o stronę.
5. Integracja: token czatu (`CHAT_SCOPE`) odświeżaniem z `scope` czatu; zwrócony nowy `refresh_token` zastępuje stary w sekretach.
6. Integracja: 401 z wyzwaniem CAE na czacie -> odświeżenie z `claims` zawierającym wyzwanie i `xms_cc`, potem powtórka.
7. Integracja: `invalid_grant` przy odświeżeniu -> `auth-required` i zdarzenie `SessionState::AuthRequired`.
8. Integracja: zły `state` w odpowiedzi strony -> `auth-required`, brak wymiany kodu na serwerze.
9. Istniejące testy z WAM przechodzą bez zmian (droga WAM nietknięta). Integracja z WAM: sekret `refresh-token` konta nie jest ani użyty przy `restore`, ani usunięty przy `sign_out`.
10. `cargo xtask ci` zielone na Linuksie (CI także Windows).
11. Rdzeń: logowanie, którego konta `add` nie przyjął, i ponowne logowanie, które kończy się, gdy konto jest już `removing`, kończą się `sign_out` w module (4.5.1).
12. Host: `set`, który się nie udał, choć magazyn trzymał już wartość, nie zostawia sekretu (4.5.2). `FileStore`: zapis, który się nie udał, nie zostawia `secrets.json.tmp` (4.5.3; test tylko na Linuksie: plik tymczasowy jako dowiązanie do `/dev/full`).
13. Integracja: sekret usunięty, póki Microsoft odpowiada na odświeżenie, nie wraca z nowym tokenem (4.3.4).

## 8. Threat model

**Zaufane:** host i jego kontrola hostów (manifest, `sign_in_hosts`), magazyn sekretów (`FileStore` 0600 / Menedżer poświadczeń), `login.microsoftonline.com` po TLS.

**Niezaufane:**
- Wklejony adres przekierowania. Host sprawdza prefiks i HTTPS (etap 1). Moduł sprawdza `state` i czyta tylko `code`, `state`, `error`, `error_description` (z tego ostatniego tylko numer `AADSTS`). Podmiana `code` bez verifiera nic nie daje (PKCE).
- Odpowiedź serwera tokenów: tylko znane pola; JWT czytany bez sprawdzania podpisu, jak z WAM. Przyszedł po TLS od hosta z manifestu, w odpowiedzi na nasz kod i verifier; `oid` porównywany z kontem sesji przy każdym odświeżeniu.

**Sekrety:** token odświeżania żyje latami przy aktywnym użyciu i daje dostęp do Teams konta jak WAM. Leży w magazynie sekretów jak każdy sekret modułu (8.4); nigdy w logach, komunikatach błędów, `storage` ani testach (fikstury z syntetycznymi tokenami). `SecretString` dla tokenów w pamięci, jak dziś. Na serwerze ryzyko przyjął właściciel (spec wtyczki 2.3).

**Granica kontenera (spec wtyczki 8).** Tokeny Teams nie opuszczają kontenera `open-mess`: token odświeżania zna tylko moduł (import `secrets`), żadne narzędzie ani komenda API administracyjnego nie zwraca sekretów modułu, a `sign_in_result` i błędy narzędzi niosą tylko teksty z 4.3.5. `open_url` w `sign_in_request` niesie wyzwanie PKCE i `state`, nie verifier. Wklejony adres przekierowania niesie `code`, który bez verifiera z pamięci modułu nic nie daje.

**Przejęty panel Paperclip (spec wtyczki 2.4, 8: kontener Paperclip częściowo zaufany).** Panel pokazuje właścicielowi link do strony logowania i odbiera wklejony adres, więc przejęty panel może podsunąć własny link (własne wyzwanie PKCE i `state`), przechwycić `code` i wymienić go sam: token powstaje wtedy poza kontenerem `open-mess`, a właściciel widzi błąd logowania w open_mess. open_mess tego nie zablokuje: nie widzi, co otworzyła przeglądarka, a identyfikator klienta Teams jest publiczny i nie wymaga sekretu, więc ten sam atak działa bez open_mess, w dowolnej chwili, gdy właściciel zaloguje się z linku od panelu. Gwarancja ze specu wtyczki 8 dotyczy tokenów, które trzyma open_mess; tych open_mess nie wydaje. Właściciel sprawdza przed zalogowaniem, że link prowadzi do `login.microsoftonline.com`, a nieudane logowanie z poprawną zgodą u Microsoftu traktuje jako sygnał przejęcia panelu (B13).

**Magazyn, którego nie da się odczytać.** `secrets.get` zwraca `none` i dla braku sekretu, i dla magazynu, który nie odpowiada; bez WAM konto kończy się wtedy `auth-required`, nie `offline`. Ponowne zalogowanie i tak zapisze nowy sekret. Rozróżnienie wymagałoby zmiany typu istniejącego importu (moduły zbudowane wcześniej by się nie załadowały, B1).

**Głębokość:** bez szyfrowania sekretu w module (klucz leżałby obok, spec wtyczki 5.2). Bez wiązania tokenu z urządzeniem (wymagałoby PRT/brokera, którego tu nie ma).

## 9. Rejestr rebutali

- B1 (autor, R1): nowy wariant `sign-in-error::unavailable` zamiast funkcji. Odrzucone: zmienia typ istniejącego importu, więc moduły zbudowane na starym WIT nie załadują się w nowym hoście; nowa funkcja tego nie robi.
- B2 (autor, R1): rozpoznanie po tekście `refused("... only on Windows")`. Odrzucone: kruche, tekst nie jest kontraktem.
- B3 (autor, R1): urząd z `tid` konta (`/<tid>/oauth2/v2.0/token`) zamiast `organizations`. Odrzucone na teraz: `organizations` działa (spec wtyczki 3.3) i nie wymaga trzymania `tid`; wrócić, jeśli test na żywo pokaże inaczej.
- B4 (autor, R1): `api::random()` na PKCE. Odrzucone: kolejne wartości z tych samych kluczy SipHash, razem 128 bitów sekretu; verifier ma mieć 256 bitów z prawdziwego źródła.
- B5 (autor, R1): metoda `plain` bez `sha2`. Odrzucone: projekt 9.1 i oficjalny klient używają `S256`.
- B6 (Codex, R3, P2): po wymianie pliku `teams.wasm` na serwerze moduł czeka na ponowne zatwierdzenie, więc konta nie działają. Odrzucone jako błąd: to zamierzona kontrola 8.3 (zgoda wiąże skrót pliku) i spec wtyczki 5.4; zdanie w rozdz. 3, które mówiło, że zgoda zostaje, poprawione w R4.
- B7 (Codex, R3, P2): otwarta strona logowania drugiego konta blokuje na 10 min wywołania kont już zalogowanych. Odrzucone: tak działa host przy każdym logowaniu z prośbą od 8.3 R4 (kod urządzenia, WAM, strona w oknie); czas ogranicza `SIGN_IN_BUDGET` i anulowanie; wdrożenie Paperclip ma jedno konto, a logowanie robi właściciel przy panelu. Rozdzielenie logowania na dwa wywołania wymagałoby zmiany typu eksportu `sign-in` w WIT, czyli psułoby moduły zbudowane wcześniej (B1). Zapisane w 4.3.2.
- B8 (Codex, R3, P2): proces ubity między zapisem tokenu a zapisem wiersza konta zostawia token bez konta. Odrzucone na teraz: 4.5, akapit "Poza zakresem".
- B9 (Codex, R3, P2): "kod ponawiania jest już w `main`", a konto `offline` po awarii sieci przy starcie nie wraca samo. Odrzucone: `main` (83136f9) nie ma `retry_offline`, to PR #17; ten spec nie zmienia losu konta `offline` (to samo co nieudany `restore` z WAM), a z PR #17 konto wraca samo. Doprecyzowane w 4.3.3.
- B10 (Codex, R4, P2): wywołanie `sign_in` przerwane przez limit hosta po zapisie tokenu (wolny `fsync`) zostawia token bez konta. Odrzucone na teraz: ta sama klasa co B8 (rdzeń nie dostaje identyfikatora konta, więc nie ma czego sprzątać); rozwiązaniem jest sprzątanie sekretów bez konta przy starcie, osobna zmiana rdzenia (4.5).
- B11 (Codex, R4, P2): gdy baza stanu nie daje się nawet czytać, `get` przerywa sprzątanie po nieudanym `add`. Odrzucone: celowo. Bez odczytu rdzeń nie wie, czy konto już ma; usunięcie sekretów działającego konta jest gorsze niż token, który zostaje w magazynie kontenera (4.5).
- B12 (Codex, R4, P2): na systemie plików tylko do odczytu `rename` i usunięcie `secrets.json.tmp` zawodzą, więc kopia zostaje. Odrzucone: na takim systemie plików nic nie usunie pliku; następny udany zapis go nadpisuje, a plik leży w tym samym wolumenie co `secrets.json` (4.5).
- B13 (Codex, R5, P1): przejęty panel Paperclip podsuwa własny link logowania i wymienia przechwycony kod sam, więc token powstaje poza kontenerem `open-mess`. Odrzucone jako błąd tego specu: logowanie przez wklejenie adresu i panel jako część zaufania właściciela to zatwierdzone decyzje specu wtyczki (2.2, 2.4, 5.4, 8); atak nie potrzebuje open_mess (publiczny identyfikator klienta bez sekretu), a demon nie widzi przeglądarki, więc żadna kontrola po jego stronie go nie zamknie. Zapisane w threat modelu (8).
- B14 (Codex, R8, P2): "Dodaj konto" dla konta, które w tym samym czasie usuwa druga operacja: usuwanie kończy się przed `add`, kolejkowane `sign_out` kasuje nowy token, a `add` wstawia wiersz od nowa, więc konto jest `online` bez tokenu i pierwsze użycie daje `auth-required`. Odrzucone: to istniejący wyścig rdzenia między dodaniem a usunięciem tego samego konta (spec projektu 8.8), w którym właściciel ściga się sam ze sobą; skutek to jedno ponowne logowanie, bez utraty danych i bez sekretu poza kontem.
- B15 (Codex, R8, P2): witryna SharePoint z dodatkowym MFA (`interaction_required` przy odświeżeniu dla tej witryny) - ponowne logowanie prosi tylko o `api.spaces.skype.com` ze stałymi `claims`, więc tego wymogu nie spełni. Odrzucone na teraz: droga WAM robi dziś dokładnie to samo (logowanie interaktywne tylko na `AUTHSVC_SCOPE`); zasób z własnym wymogiem interakcji wymagałby logowania, które zna zasób i jego wyzwanie, w obu drogach. Odświeżenie na SharePoint nie jest jeszcze sprawdzone na żywo (status, "Needs live capture" 2).

## 10. Poza zakresem

- Strona logowania jako droga zapasowa na Windows po odmowie WAM. Uwaga na przyszłość: Menedżer poświadczeń ma limit 2560 bajtów na sekret, a token odświeżania Entra bywa dłuższy.
- Kod urządzenia (9.1 pkt 3).
- Etapy 3-4 specu wtyczki.

## Changelog rund

R1: pierwsza wersja.

R2: poprawiony wektor RFC 7636 w 7.2; test na żywo (krok 4) zrobiony - logowanie, odświeżenie na IC3, middle tier i `outlook.office.com`, odtworzenie po restarcie; MCPS, zasób nagrań i SharePoint nie sprawdzone na żywo.

R3 (przejęcie do repozytorium, review commitów Codex): `open` i `sign_out` z WAM nie sięgają do sekretów (4.3.3, 4.3.6; wcześniej `open` czytał sekret i na Windows, a testy z WAM i bez WAM ścigały się o domyślny magazyn procesu); `without_wam` odmawia też `wam-token` (4.2); nowy 4.5: sprzątanie sekretu po nieudanym `add` w rdzeniu i po zapisie, który magazyn zgłosił jako błąd; status bez "kroki 1-4 zrobione" (MCPS, nagrania i SharePoint nie sprawdzone na żywo); threat model: granica kontenera ze specu wtyczki 8 i magazyn, którego nie da się odczytać; kryteria 1, 9, 11, 12.

R4 (Codex R3: REJECT, GLM R3: APPROVE): poprawione zdanie o zgodzie na moduł w rozdz. 3 (B6); 4.3.2: wywołania modułu czekają, póki strona czeka (B7); 4.3.3: PR #17 nie jest w `main` (B9); 4.5.3: `FileStore` usuwa plik tymczasowy po nieudanym zapisie; 4.5: okno między zapisem sekretu a wierszem konta poza zakresem (B8); kryterium 12.

R5 (Codex R4: REJECT, GLM R4: APPROVE): 4.5 "Poza zakresem" obejmuje też przerwane `sign_in`, nieczytelną bazę stanu i system plików tylko do odczytu (B10-B12).

R6 (Codex R5: REJECT, GLM R5: APPROVE): threat model - przejęty panel Paperclip i podsunięty link logowania (B13).

R7 (Codex R6: REJECT, GLM R6: APPROVE, P3 o wykrywaniu ponownego użycia tokenu bez zmian): 4.3.4 - rotacja zapisuje nowy token tylko nad tym, którym odświeżano (konto usunięte w trakcie odświeżania, także przy wyłączanym module, nie dostaje sekretu z powrotem); kryterium 13.

R8 (Codex R7: REJECT, GLM R7: APPROVE, P3 o ponowieniu wymiany kodu po 5xx bez zmian - `invalid_grant` daje `auth-required` i nowe logowanie): 4.5.1 - logowanie, które kończy się przy koncie `removing`, sprząta sesję i sekrety jak dla konta, którego nie ma (usuwanie przy wyłączanym module pomija `sign_out`); kryterium 11.

R9 (Codex R8: REJECT, GLM R8: APPROVE bez nowych zastrzeżeń): rejestr rebutali B14 (wyścig "Dodaj konto" z usuwaniem tego samego konta) i B15 (dodatkowe MFA witryny SharePoint, jak przy WAM).

R10 (Codex R9: REJECT, GLM R9: APPROVE bez zastrzeżeń): 4.1 - nowy moduł Teams wymaga hosta z `wam-available` (na open_mess 0.2.0 się nie załaduje; kolejność wdrożenia); rozdz. 3 - zgoda przy ręcznej podmianie pliku a zgoda zapisywana przez aktualizator przy podpisanym wydaniu (Codex P3).

R11 (Codex R10: APPROVE, GLM R10: APPROVE): tylko sformułowania z dwóch P3 Codex (zgoda na nowy plik a `rescan_modules` w działającym demonie; co zostaje w sekretach po nieudanym zapisie rotacji), bez kolejnej rundy.
