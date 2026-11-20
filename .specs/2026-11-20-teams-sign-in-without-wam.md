# open_mess - logowanie Teams bez WAM (etap 2 wtyczki Paperclip)

Data: 2026-11-20
Status: R1, przejrzany przez autora (bez Codex i GLM: autor nie ma tych narzędzi; review właściciela w PR)
Powiązane: `.specs/2026-11-03-paperclip-plugin.md` (rozdz. 3.3, 6), `.specs/2026-10-26-open-mess-design.md` (8.3, 8.4, 9.1, 9.5), `docs/protocols/teams.md` (1.1, 1.3, 1.4, "Needs live capture" 2), `wit/module.wit`, `modules/teams/src/{api,session,manifest}.rs`, `crates/open-mess-host/src/{wam,sign_in,bindings}.rs`.

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
- `modules/teams/src/manifest.rs`: `login.microsoftonline.com` jest już w `hosts` (komentarz "1.3: authorization code redemption and token refresh") i w `sign_in_hosts`. Manifest się nie zmienia, więc skrót manifestu i zgoda użytkownika (8.3) zostają.
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

Nowa funkcja w istniejącym interfejsie nie psuje modułów zbudowanych wcześniej: importują one mniej funkcji, niż host definiuje (projekt, rozdz. 12: kontrakt hosta rozszerzalny bez psucia istniejących modułów). Wersja pakietu zostaje `0.1.0`. Nowy wariant `sign-in-error` by je zepsuł (rejestr rebutali, B1).

### 4.2 Host (`crates/open-mess-host`)

- `wam-available` zwraca `cfg!(windows)`.
- Testy (`test-support`): z fałszywym WAM zwraca `true` jak dziś. Nowa opcja `Engine::without_wam()` (tylko `test-support`) daje `false` i usuwa fałszywy WAM, żeby testy przeszły drogą bez WAM na każdej platformie, także w CI Windows.
- Bez innych zmian: `open_page`, sekrety i kontrola hostów działają jak w etapie 1.

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

#### 4.3.3 Źródło tokenów sesji

`Session` dostaje pole `source: Source { Wam, Refresh }`. `renew` i `open` wybierają drogę:

- `open(account)` (restore, przeładowana instancja): jest sekret `refresh-token` konta -> odświeżenie na `AUTHSVC_SCOPE`, źródło `Refresh`; nie ma -> WAM jak dziś. Bez WAM i bez sekretu -> `auth-required("sign in again")`.
- `renew(session, scope, claims)`: `Wam` -> jak dziś; `Refresh` -> 4.3.4.
- Sprawdzenie `oid` nowego tokenu z kontem sesji zostaje w obu drogach.

#### 4.3.4 Odświeżenie

- `POST .../organizations/oauth2/v2.0/token`, pola `client_id`, `grant_type=refresh_token`, `refresh_token`, `scope=<zakres> offline_access`, `claims`.
- `claims` = `{"access_token":{"xms_cc":{"values":["CP1"]}}}` scalone z claims wyzwania CAE, jeśli jest (`oauth::merge_claims`: obiekt `access_token` z wyzwania + `xms_cc`; wyzwanie, które nie jest obiektem JSON, idzie bez zmian, jak dziś do WAM).
- Odpowiedź z nowym `refresh_token` -> `secrets::set` (rotacja). Nieudany zapis: ostrzeżenie w logu (bez wartości), token dostępu użyty; stary token odświeżania zostaje w sekretach.
- Odpowiedź bez `refresh_token` -> stary zostaje.
- Odbiorcy: każdy z tabeli 1.4, których moduł używa (`SCOPES` z `manifest.rs`, SharePoint organizacji). Spec wtyczki 3.3 potwierdził odświeżenie z tokenu wydanego dla `api.spaces.skype.com` na `ic3.teams.office.com`; pozostali odbiorcy - krok 4 (test na żywo).

#### 4.3.5 Błędy Microsoftu

Odpowiedź 400/401 z JSON `{"error": ..., "error_description": ..., "error_codes": [...]}`:
- `invalid_grant`, `interaction_required`, `consent_required`, `login_required` -> `auth-required("Microsoft asks to sign in again (AADSTS<kod>)")`; dalej jak każdy `auth-required` (`with_session` zgłasza `SessionState::AuthRequired`). Sekret zostaje: użytkownik loguje się ponownie i nowy go nadpisze.
- inny `error` -> `platform` z kodem i `AADSTS`.
- ciało nie jest takim JSON -> `platform("the sign-in service answered HTTP <status>")`.

Do komunikatów trafia tylko `error`, liczba z `AADSTS` i status; nigdy `error_description` w całości, token, kod ani adres.

#### 4.3.6 Wylogowanie

`sign_out` jak dziś. Sekret `refresh-token` usuwa rdzeń razem z kontem (`delete_account_secrets`). Moduł dodatkowo woła `secrets::delete(account, "refresh-token")`, żeby nie zależeć od kolejności.

### 4.4 Limity

- Adres autoryzacji ma ok. 700 znaków, poniżej `MAX_PROMPT_TEXT`.
- Jeden sekret na konto (`MAX_SECRETS` 64 na moduł).
- Ciało odpowiedzi tokenu: `MAX_BODY` hosta; moduł czyta tylko znane pola.

## 5. Kroki

1. WIT + host (4.1, 4.2) z testami. `cargo xtask ci`.
2. Moduł: `oauth.rs` z testami jednostkowymi, potem `api.rs` (4.3.2-4.3.6).
3. Testy integracyjne w `crates/open-mess-host/tests/teams/` (rozdz. 7, pkt 3-8): fałszywy serwer dostaje ścieżki `/organizations/oauth2/v2.0/token`, prośbę `open-page` test odbiera z `SignInPrompts` i odpowiada adresem przekierowania.
4. Test na żywo (9.5, z właścicielem konta przy komputerze, za jego zgodą): demon Linux w kontenerze, logowanie przez API administracyjne, `status`, lista rozmów, odświeżenie tokenu po restarcie demona, odbiorcy z 4.3.4. Tylko statusy i nazwy pól do `docs/protocols/teams.md` ("Live results", "Needs live capture" 2).
5. PR.

## 6. Dotknięte pliki

- `wit/module.wit`
- `crates/open-mess-host/src/{sign_in,bindings,runtime,lib}.rs`
- `crates/open-mess-host/tests/teams/{main,fake}.rs`
- `modules/teams/Cargo.toml`, `modules/teams/src/{oauth,api,session,lib}.rs`
- `modules/test-*`: tylko jeśli bindingi tego wymagają (nowa funkcja hosta nie wymaga zmian w gościach)
- `docs/protocols/teams.md`, `Cargo.lock`

## 7. Kryteria akceptacji

Każde ma test, który bez zmiany nie przechodzi.

1. Host: `wam-available` = `false` poza Windows; `true` z fałszywym WAM; `false` z `Engine::without_wam()`.
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
9. Istniejące testy z WAM przechodzą bez zmian (droga WAM nietknięta).
10. `cargo xtask ci` zielone na Linuksie (CI także Windows).

## 8. Threat model

**Zaufane:** host i jego kontrola hostów (manifest, `sign_in_hosts`), magazyn sekretów (`FileStore` 0600 / Menedżer poświadczeń), `login.microsoftonline.com` po TLS.

**Niezaufane:**
- Wklejony adres przekierowania. Host sprawdza prefiks i HTTPS (etap 1). Moduł sprawdza `state` i czyta tylko `code`, `state`, `error`, `error_description` (z tego ostatniego tylko numer `AADSTS`). Podmiana `code` bez verifiera nic nie daje (PKCE).
- Odpowiedź serwera tokenów: tylko znane pola; JWT czytany bez sprawdzania podpisu, jak z WAM. Przyszedł po TLS od hosta z manifestu, w odpowiedzi na nasz kod i verifier; `oid` porównywany z kontem sesji przy każdym odświeżeniu.

**Sekrety:** token odświeżania żyje latami przy aktywnym użyciu i daje dostęp do Teams konta jak WAM. Leży w magazynie sekretów jak każdy sekret modułu (8.4); nigdy w logach, komunikatach błędów, `storage` ani testach (fikstury z syntetycznymi tokenami). `SecretString` dla tokenów w pamięci, jak dziś. Na serwerze ryzyko przyjął właściciel (spec wtyczki 2.3).

**Głębokość:** bez szyfrowania sekretu w module (klucz leżałby obok, spec wtyczki 5.2). Bez wiązania tokenu z urządzeniem (wymagałoby PRT/brokera, którego tu nie ma).

## 9. Rejestr rebutali

- B1 (autor, R1): nowy wariant `sign-in-error::unavailable` zamiast funkcji. Odrzucone: zmienia typ istniejącego importu, więc moduły zbudowane na starym WIT nie załadują się w nowym hoście; nowa funkcja tego nie robi.
- B2 (autor, R1): rozpoznanie po tekście `refused("... only on Windows")`. Odrzucone: kruche, tekst nie jest kontraktem.
- B3 (autor, R1): urząd z `tid` konta (`/<tid>/oauth2/v2.0/token`) zamiast `organizations`. Odrzucone na teraz: `organizations` działa (spec wtyczki 3.3) i nie wymaga trzymania `tid`; wrócić, jeśli test na żywo pokaże inaczej.
- B4 (autor, R1): `api::random()` na PKCE. Odrzucone: kolejne wartości z tych samych kluczy SipHash, razem 128 bitów sekretu; verifier ma mieć 256 bitów z prawdziwego źródła.
- B5 (autor, R1): metoda `plain` bez `sha2`. Odrzucone: projekt 9.1 i oficjalny klient używają `S256`.

## 10. Poza zakresem

- Strona logowania jako droga zapasowa na Windows po odmowie WAM. Uwaga na przyszłość: Menedżer poświadczeń ma limit 2560 bajtów na sekret, a token odświeżania Entra bywa dłuższy.
- Kod urządzenia (9.1 pkt 3).
- Etapy 3-4 specu wtyczki.

## Changelog rund

R1: pierwsza wersja.
