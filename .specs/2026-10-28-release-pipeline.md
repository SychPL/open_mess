# open_mess - wydania z pipeline'u GitHub Actions

Data: 2026-10-28
Status: do review (Codex, GLM)
Powiązane: `.specs/2026-10-27-self-update.md` (4 - wydanie, 8 - threat model), `xtask/src/main.rs`, `.github/workflows/ci.yml`, `CONTRIBUTING.md` ("Releases").

## 1. Cel

Właściciel wydaje nową wersję z GitHuba, bez lokalnego budowania i bez wpisywania hasła w konsoli: uruchamia workflow "Release" z numerem wersji, zatwierdza to jedno uruchomienie w GitHubie, a workflow robi to samo, co dziś `cargo xtask release <wersja>` (commit wersji, tag, build, podpis, wydanie). Lokalne `cargo xtask release` zostaje bez zmian jako droga zapasowa.

## 2. Decyzje właściciela (2026-10-28)

- Klucz prywatny wydań (`release.key`, zaszyfrowany) i jego hasło trafiają do sekretów **środowiska** GitHub `release` w repozytorium `SychPL/open_mess`, nie do sekretów repozytorium.
- Każde uruchomienie, które sięga po te sekrety, wymaga zatwierdzenia przez właściciela (wymagany recenzent środowiska: `SychPL`).
- Właściciel świadomie przyjmuje, że klucz opuszcza jego komputer (sekcja 7). Lokalna kopia `%USERPROFILE%\.open_mess\release.key` zostaje jako kopia zapasowa.

## 3. Kontekst w repo

- `xtask/src/main.rs`, `release_steps`: kroki 1-6 speca samoaktualizacji. Krok 1 (warunki) sprawdza czyste drzewo na `main` równym `origin/main`, brak tagu, zielone `cargo xtask ci` i plik `.reviews/<sha7>.processed` dla każdego commita od poprzedniego tagu `v*` (`unprocessed_commits`). Klucz prywatny czyta `minisign::SecretKey::from_file(&key_path, None)`, które pyta o hasło na konsoli; `secret_key_path()` bierze ścieżkę z `OPEN_MESS_RELEASE_KEY` albo `%USERPROFILE%\.open_mess\release.key`. Krok 6: `git push --atomic origin main <tag>`, potem `gh release create`.
- `.reviews/` jest lokalny i ignorowany przez git (AGENTS.md): werdykty Codex/GLM istnieją tylko na komputerze właściciela. Runner GitHuba ich nie ma, więc lokalny warunek recenzji nie da się tam spełnić.
- Każda zmiana wchodzi na `main` przez pull request, scalany squashem (CONTRIBUTING.md); szablon PR wymaga rozstrzygniętych recenzji commitów. Wyjątek: commit `Release <wersja>` z samego `xtask release`, który zmienia tylko wersję i `Cargo.lock` (sprawdzane na diffie).
- `.github/workflows/ci.yml`: bramki na `windows-latest` i `ubuntu-latest` (`dtolnay/rust-toolchain@stable` z `wasm32-wasip2`, `Swatinem/rust-cache@v2`, `cargo xtask ci`), akcje przypięte tagami.
- `om.exe` jest programem Windows, więc wydanie buduje się na `windows-latest`.
- Ochrona gałęzi `main` nie jest jeszcze włączona.

## 4. Zmiany w `xtask`

1. **Hasło ze zmiennej środowiskowej.** Gdy `OPEN_MESS_RELEASE_KEY_PASSWORD` jest ustawione (i niepuste), `release` odblokowuje klucz tym hasłem (`SecretKey::from_file(&key_path, Some(hasło))`) zamiast pytać na konsoli. Hasło nigdy nie trafia na wyjście ani do komunikatu błędu. Zły klucz albo złe hasło nadal kończy wydanie przed commitem i tagiem (dzisiejsze `check_key_pair`).
2. **Tryb pipeline'u: `cargo xtask release <wersja> --pipeline`.**
   - Działa tylko, gdy `GITHUB_ACTIONS=true`; poza GitHub Actions odmawia ("--pipeline only runs in GitHub Actions"), żeby lokalnie nie dało się przypadkiem ominąć warunku recenzji.
   - Wymaga `OPEN_MESS_RELEASE_KEY_PASSWORD` (bez niego odmawia, bo nie ma konsoli).
   - Zastępuje warunek `.reviews` warunkiem "każdy commit od poprzedniego tagu `v*` wszedł przez scalony pull request": dla każdego commita z `git rev-list <poprzedni-tag>..HEAD` pyta `gh api repos/SychPL/open_mess/commits/<sha>/pulls` i wymaga PR z bazą `main`, `merged_at` różnym od null i `merge_commit_sha` równym temu commitowi. Najwyżej 500 commitów w zakresie (więcej = odmowa) i najwyżej 64 KiB odpowiedzi na commit.
   - Brak poprzedniego tagu `v*` w trybie pipeline'u = odmowa ("the first release is local"). Pierwsze wydanie (0.1.0, 2026-10-28) właściciel robi lokalnie, z pełnym warunkiem `.reviews` dla całej historii, w której są też commity sprzed pull requestów; pipeline zaczyna od drugiego.
   - Wymaga `HEAD` równego `GITHUB_SHA`: wydanie dotyczy dokładnie commita, dla którego uruchomiono (i zatwierdzono) przebieg; gdy `main` w międzyczasie się przesunął, dzisiejszy warunek `main` == `origin/main` i ten odmawiają.
   - Publikacja przez szkic, w obu trybach: po `git push --atomic` `xtask` zakłada wydanie jawnie jako szkic (`gh release create v<wersja> --draft ...` z plikami), a przy przejściowym błędzie ponawia do 3 razy (30 s przerwy): gdy szkic z tym tagiem już istnieje, ponowienie to `gh release upload v<wersja> <pliki> --clobber`. Potem sprawdza `gh release view v<wersja> --json isDraft,assets`: dokładnie pliki tego wydania (lista, którą `xtask` sam zapisał: moduły, `om.exe`, manifest i podpis), z rozmiarami jak na dysku; `xtask` czyści `target/release-dist` przed zapisem, więc pliki z poprzednich wydań nie trafiają ani do wysyłki, ani do sprawdzenia. Dopiero komplet = `gh release edit v<wersja> --draft=false` i ponowne sprawdzenie, że wydanie jest opublikowane. Szkic nie jest widoczny w `releases/latest`, a demon pomija szkice (spec samoaktualizacji, 5), więc niekompletne wydanie nigdy nie trafia do instalacji, a `--clobber` działa tylko na szkicu. Gdy `git push --atomic` zwróci błąd, `xtask` sprawdza `git ls-remote origin refs/tags/v<wersja>`: tag na origin wskazujący commit wydania znaczy, że push doszedł (połączenie urwało się dopiero przy potwierdzeniu), więc stan to "po pushu" i publikacja idzie dalej. Polecenia odzyskania po pushu zaczynają się od `gh release view v<wersja> --json isDraft,assets` i każą: przy opublikowanym wydaniu z kompletem plików nic nie robić; przy szkicu `upload --clobber`, sprawdzenie, `edit --draft=false`; przy braku wydania `create --draft`, sprawdzenie, publikacja. `--clobber` nigdy nie dotyka opublikowanego wydania. Gdy się nie uda, wydanie zostaje szkicem, `xtask` wypisuje te polecenia (w `--pipeline` tylko dla etapu po pushu: wcześniejsze etapy nie zostawiają niczego na origin, a polecenia cofające lokalny commit nie mają sensu poza znikającym runnerem), kończy się błędem, a workflow zachowuje pliki (sekcja 5, krok 6).
   - Decyzja dla jednego commita to czysta funkcja `merged_through_pr(sha, odpowiedź_json) -> bool`, testowana na przykładowych odpowiedziach.
   - Lista commitów z numerami PR trafia do `GITHUB_STEP_SUMMARY`, żeby przebieg wydania miał zapis tego, co weszło.
   - Wszystko inne bez zmian: czyste drzewo, `main` równe `origin/main`, brak tagu, `cargo xtask ci`, commit wersji, tag, build, podpis, `git push --atomic`, `gh release create`.
3. Lokalne `cargo xtask release <wersja>` bez `--pipeline` działa jak dziś poza punktem 4 (warunek `.reviews`, hasło z konsoli albo ze zmiennej).
4. **Klucz, któremu ufają instalacje.** `check_key_pair` porównuje klucz prywatny z `keys/release.pub` **poprzedniego tagu `v*`** (`git show <tag>:keys/release.pub`), a nie z drzewa roboczego, bo to ten klucz jest wkompilowany w wersję, z której użytkownicy się aktualizują; ten sam klucz publiczny (jedna wartość w kodzie) idzie do `minisign::sign(Some(&klucz), ...)` przy podpisie manifestu, który dziś dostaje klucz z drzewa i sam sprawdza parę. Bez poprzedniego tagu (pierwsze wydanie) - klucz z drzewa. Poprzedni tag rozwiązywany jest dopiero po `git fetch origin main --tags`, więc kolejność kroku 1 zmienia się na: fetch, potem poprzedni tag, klucz i hasło, potem pozostałe warunki; nadal wszystko przed commitem i tagiem. Dzięki temu rotacja działa: wydanie, które wnosi nowy `keys/release.pub`, podpisuje się jeszcze starym kluczem, a następne już nowym. Dotyczy obu trybów (lokalnego i `--pipeline`).
5. **Wersja rośnie.** W obu trybach wersja musi być ściśle wyższa (porównanie liczbowe `x.y.z`) od wersji poprzedniego tagu `v*`, inaczej odmowa przed commitem i tagiem ("1.2.3 is not above v1.2.4"): działający open_mess i tak odrzuciłby wydanie równe albo niższe (spec samoaktualizacji, 5).

## 5. Workflow `.github/workflows/release.yml`

- Wyzwalacz: tylko `workflow_dispatch` z wejściem `version` (tekst). Bez `push`, `pull_request`, `schedule`.
- Jedno zadanie `release`: `runs-on: windows-latest`, `environment: release`, `if: ${{ !cancelled() && github.ref == 'refs/heads/main' }}` (jawnie, żeby anulowanie przebiegu zawsze anulowało zadanie), `concurrency: { group: release, cancel-in-progress: false }`, `timeout-minutes: 90`.
- `permissions: contents: write` (push commitu i tagu, wydanie) i `pull-requests: read` (kontrola "commit wszedł przez scalony PR", `commits/<sha>/pulls`); nic więcej.
- Kroki:
  1. `actions/checkout` z `fetch-depth: 0` i `fetch-tags: true` (potrzebne `git describe` i zakres commitów), potem `git checkout -B main "$env:GITHUB_SHA"`, żeby istniała lokalna gałąź `main` niezależnie od tego, czy checkout zostawił odłączony `HEAD` (`xtask` sprawdza nazwę gałęzi i pushuje `main`).
  2. Toolchain jak w `ci.yml` (`stable`, `rustfmt`, `clippy`, `wasm32-wasip2`). **Bez** `rust-cache`: wydanie nie bierze artefaktów z cache'u, który mogły zapisać inne przebiegi.
  3. Tożsamość git: `github-actions[bot]` (`41898094+github-actions[bot]@users.noreply.github.com`) dla commitu `Release <wersja>`.
  4. Zapis klucza: sekret `RELEASE_KEY_FILE` (zawartość pliku `release.key`) do `$RUNNER_TEMP\release.key` bajt w bajt (np. `[IO.File]::WriteAllText` z UTF-8 bez BOM, bez dodawania nowej linii), bo `SecretKey::from_file` odrzuca zniekształcony plik; ścieżka tego pliku trafia do zmiennej `OPEN_MESS_RELEASE_KEY`, którą `xtask` już czyta jako ścieżkę.
  5. Wydanie: `env:` kroku ustawia `VERSION: ${{ inputs.version }}`, `OPEN_MESS_RELEASE_KEY_PASSWORD: ${{ secrets.RELEASE_KEY_PASSWORD }}` i `GH_TOKEN: ${{ github.token }}`, a skrypt kroku to `cargo xtask release "$env:VERSION" --pipeline`. Wejście nigdy nie jest wstawiane w tekst skryptu (bez wstrzyknięcia w powłokę); `xtask` i tak odrzuca wszystko poza `x.y.z`.
  Krok wydania ma własne `timeout-minutes: 75`, żeby po jego przekroczeniu zostało miejsce w limicie zadania (90) na kroki 6 i 7.
  6. `if: failure() || cancelled()`: `actions/upload-artifact` z `target/release-dist` jako `release-dist-<wersja>` (retencja 14 dni, `if-no-files-found: ignore`, bo przy błędzie przed buildem folderu jeszcze nie ma), żeby po nieudanej publikacji pliki z jednorazowego runnera zostały do odzyskania (`gh run download <przebieg> -n release-dist-<wersja> -D target/release-dist` w czystym klonie na tagu odtwarza ścieżkę, której używają polecenia odzyskania wypisane przez `xtask`; podsumowanie przebiegu podaje to polecenie z numerem przebiegu). Folder nie zawiera klucza (ten leży w `$RUNNER_TEMP`); pliki i tak są publiczną treścią wydania, a ich integralność chroni podpis.
  7. `if: always()`: usunięcie `$RUNNER_TEMP\release.key`.
- Akcje w tym workflow przypięte do pełnych SHA commitów (z komentarzem z wersją), bo to zadanie widzi klucz.

## 6. Konfiguracja GitHuba (raz)

- Środowisko `release`: wymagany recenzent `SychPL`; zasady wdrożeń: tylko gałąź `main`; "zapobiegaj samodzielnemu zatwierdzeniu" wyłączone (właściciel sam uruchamia i sam zatwierdza). Zakłada je Claude przez `gh api` po zgodzie właściciela.
- Sekrety środowiska `release`: `RELEASE_KEY_FILE` (zawartość `release.key`) i `RELEASE_KEY_PASSWORD`. Ustawia je **sam właściciel** (`gh secret set ... --env release`); Claude nie przesyła klucza ani hasła.
- Repozytorium dopuszcza tylko scalanie squashem (ustawienie repo), żeby każdy commit na `main` był commitem scalenia swojego PR (warunek z 4.2); robi to Claude przez `gh api` razem ze środowiskiem.
- Gdy później wejdzie ochrona `main` wymagająca PR: push `Release <wersja>` z `GITHUB_TOKEN` zostanie odrzucony. Ochrona musi wtedy dopuścić ten push (np. reguła z wyjątkiem dla GitHub Actions) albo wydanie zostaje lokalne; CONTRIBUTING to opisuje.

## 7. Threat model

- **Zaufane:** konto GitHub właściciela (z 2FA) i jego zatwierdzenie w środowisku; runnery hostowane przez GitHub (świeża maszyna na każdy przebieg); zależności Rust na poziomie, który już dziś mają (`Cargo.lock`, `cargo-deny`) - złośliwa zależność i tak mogłaby wbudować się w `om.exe`, więc dostęp do klucza w tym samym przebiegu nie dodaje nowej klasy ataku; akcje przypięte do SHA.
- **Kod scalony na `main`** jest zaufany tak samo jak dziś: ten sam przebieg, który go buduje, widzi klucz (rebutal Codex R6).
- **Niezaufane:** pull requesty (także z forków) i ich zmiany w workflow - sekrety środowiska są dostępne tylko dla przebiegów z `main` (zasada wdrożeń) i tylko po zatwierdzeniu; wejście `version` - walidowane przez `xtask`, nigdy nie wstawiane w skrypt powłoki.
- **Co się zmienia względem speca samoaktualizacji:** dziś podpis chroni też przed przejęciem konta GitHub (bez dysku właściciela nie ma podpisu). Po tej zmianie przejęcie konta właściciela (albo prawa do zatwierdzania w środowisku) wystarczy do wydania podpisanej wersji. To świadoma decyzja właściciela (sekcja 2). Ograniczenia: tylko `main`, tylko po zatwierdzeniu, każdy przebieg zostaje w historii Actions.
- **Głębokość:** bez HSM, OIDC ani podpisu bez klucza (np. Sigstore); bez osobnego klucza dla CI. Wyciek klucza: rotacja to nowa para i wydanie podpisane **starym** kluczem, które wnosi nowy `keys/release.pub` (stare instalacje ufają tylko staremu); procedura w CONTRIBUTING. Ograniczenie: demon sprawdza tylko najnowsze wydanie, więc instalacja, która była wyłączona przez cały czas, gdy wydanie przejściowe było najnowsze, nie zaktualizuje się już sama (następne wydania podpisuje nowy klucz). Taką instalację użytkownik aktualizuje ręcznie (`scripts/install.ps1` z wydania); CONTRIBUTING każe przy rotacji zostawić wydanie przejściowe jako najnowsze możliwie długo i opisać to w jego notatce.
- Hasło i klucz nie trafiają do logów: GitHub maskuje sekrety, `xtask` ich nie wypisuje, plik klucza żyje w `$RUNNER_TEMP` i jest usuwany.

## 8. Dotknięte pliki

- `xtask/src/main.rs`: hasło ze zmiennej, `--pipeline`, `merged_through_pr`, podsumowanie kroku, testy.
- `.github/workflows/release.yml`: nowy.
- `CONTRIBUTING.md` ("Releases"): wydanie z pipeline'u, konfiguracja środowiska, rotacja klucza, ochrona `main`.

## 9. Kryteria akceptacji

1. Uruchomienie "Release" z `main` z wersją `x.y.z` czeka na zatwierdzenie; po zatwierdzeniu powstaje commit `Release x.y.z` i tag `vx.y.z` na `main` oraz wydanie z `om.exe`, `teams.wasm`, `manifest.json`, `manifest.json.minisig`; podpis przechodzi weryfikację kluczem z `keys/release.pub`, a działający open_mess znajduje to wydanie ("Sprawdź teraz").
2. Uruchomienie z innej gałęzi nie dostaje sekretów i nic nie publikuje.
3. `cargo xtask release 1.2.3 --pipeline` poza GitHub Actions odmawia, zanim czegokolwiek dotknie.
4. Commit od poprzedniego tagu bez scalonego PR (np. push prosto na `main`) = odmowa przed commitem i tagiem, z sha tego commita w komunikacie.
5. Złe hasło w sekrecie = odmowa przed commitem i tagiem; hasło nie pojawia się w logu.
5a. Pierwsza próba założenia szkicu albo wgrania plików zawodzi (np. przerwany upload): kolejna próba kończy wydanie; gdy zawiodą wszystkie, wydanie zostaje szkicem (niewidocznym dla instalacji), a przebieg zostawia artefakt `release-dist-<wersja>` i log z poleceniami odzyskania.
5b. `--pipeline` bez wcześniejszego tagu `v*` odmawia przed commitem i tagiem.
5c. Wydanie jest publikowane (`--draft=false`) dopiero po sprawdzeniu kompletu plików; w żadnym momencie `releases/latest` nie wskazuje wydania bez kompletu plików.
5d. Rotacja: drzewo z nowym `keys/release.pub` i klucz prywatny pasujący do `keys/release.pub` poprzedniego tagu = wydanie przechodzi; klucz pasujący tylko do nowego = odmowa przed commitem.
5e. `HEAD` różny od `GITHUB_SHA` w `--pipeline` = odmowa przed commitem.
5f. Wersja równa albo niższa od poprzedniego tagu = odmowa przed commitem, w obu trybach.
6. Lokalne `cargo xtask release` bez `--pipeline` zachowuje się jak przed zmianą poza zmianami opisanymi wprost jako wspólne dla obu trybów (hasło ze zmiennej w 4.1, klucz poprzedniego tagu w 4.4, rosnąca wersja w 4.5, publikacja przez szkic i ustalenie pushu w 4.2): nadal warunek `.reviews` i hasło z konsoli, gdy zmiennej nie ma.
7. Testy `xtask`: `merged_through_pr` (scalony PR z tym `merge_commit_sha`; PR niescalony; PR do innej bazy; inny `merge_commit_sha`; pusta lista; zepsuty JSON), wybór klucza zaufanego (z poprzedniego tagu albo, bez tagu, z drzewa) i odmowa klucza pasującego tylko do nowego `keys/release.pub`, `version_above` (wyższa, równa, niższa, porównanie liczbowe `0.10.0` > `0.9.9`), odmowa `--pipeline` bez `GITHUB_ACTIONS`, odmowa `--pipeline` bez hasła.

## 10. Poza zakresem

- Wydania wyzwalane tagiem albo harmonogramem; notatki wydania generowane z PR; podpis bez klucza.
- Przeniesienie lokalnego warunku `.reviews` do repozytorium.

## 11. Rejestr rebutali

- **Codex R1 P1 (pierwsze wydanie bez tagu odrzucane przez `--pipeline`)**: zamierzone, nie poprawiane. Pierwsze wydanie obejmuje całą historię, w tym commity z czasu przed pull requestami, których bramka PR nie przepuści; jego warunek recenzji da się sprawdzić tylko lokalnie (`.reviews`). Właściciel robi 0.1.0 lokalnie (2026-10-28), więc pipeline zawsze ma poprzedni tag. Spec mówi to wprost (4.2).
- **Codex R9 P2 (anulowanie po pushu nie zachowa artefaktu, bo `!cancelled()` anuluje zadanie)**: odrzucone. Warunek zadania decyduje tylko o tym, czy zadanie zostanie anulowane; w anulowanym zadaniu przerwany jest bieżący krok, a kroki z `if: failure() || cancelled()` i `if: always()` nadal się wykonują (na tym GitHub opiera kroki sprzątające), więc krok 6 zachowuje `target/release-dist`, a krok 7 usuwa klucz. Commit i tag na origin bez wydania odzyskuje się z artefaktu poleceniami z 4.2.
- **Codex R6 P2 (złośliwy scalony PR zmienia `build.rs` i kradnie klucz z runnera)**: odrzucone. Kod na `main` jest zaufany z założenia: wchodzi tylko przez PR scalany przez właściciela po recenzjach Codex i GLM, a wydanie służy właśnie temu, by ten kod podpisać i rozesłać. Złośliwy scalony kod kompromituje wszystkie instalacje przez samo podpisane wydanie, z kluczem czy bez; to samo dotyczy zmiany w `xtask`. Prawdziwa izolacja klucza od kodu projektu wymagałaby osobnego zadania podpisującego bez żadnego kodu z repo (gotowy podpisywacz, przekazanie commitu przez artefakt), co jest nieproporcjonalne dla projektu jednego właściciela. Sekcja 7 mówi to wprost.
- **Codex R3 P2 (instalacja wyłączona przez całe życie wydania przejściowego nie przejdzie rotacji sama)**: przyjęte jako ograniczenie, nie poprawiane w tym specu. Usunięcie go wymaga zmiany w demonie (szukanie starszych wydań albo łańcuch zaufania kluczy), czyli zmiany kontraktu samoaktualizacji, a rotacja to procedura awaryjna po wycieku klucza. Spec opisuje ograniczenie i ręczną ścieżkę (sekcja 7).

## Changelog rund

R1: zakres odpowiedzialności przy braku tagu opisany wprost (rebutal Codex P1); po udanym pushu `gh release create` z ponowieniami i `upload --clobber`, a nieudany przebieg zostawia pliki jako artefakt (Codex P2, GLM P2); wersja tylko przez `env:` w kroku (GLM P3); sekrety przemianowane na `RELEASE_KEY_FILE` i `RELEASE_KEY_PASSWORD`, żeby nie myliły się ze zmienną ścieżki `OPEN_MESS_RELEASE_KEY` (GLM P3).
R2: lokalna gałąź `main` na `GITHUB_SHA` po checkout i warunek `HEAD` == `GITHUB_SHA` (GLM P1); po publikacji sprawdzenie, że wydanie nie jest szkicem i ma komplet plików, ze zdjęciem szkicu (Codex P2, GLM P2); klucz prywatny porównywany z `keys/release.pub` poprzedniego tagu, więc rotacja działa (Codex P2); artefakt także przy anulowaniu i limicie czasu kroku (GLM P3).
R3: GLM zatwierdził. Ograniczenie rotacji dla instalacji, które pominą wydanie przejściowe, opisane w threat modelu i rejestrze rebutali (Codex P2).
R4: `pull-requests: read` w uprawnieniach workflow (Codex P1); klucz poprzedniego tagu wprost także w `minisign::sign` (GLM P2); `if-no-files-found: ignore` dla artefaktu (GLM P3).
R5: GLM zatwierdził. Wersja musi rosnąć względem poprzedniego tagu (Codex P2); repo tylko ze scalaniem squashem (GLM P3).
R6: GLM zatwierdził. Polecenie pobrania artefaktu odtwarzające `target/release-dist` (Codex P2); poprzedni tag rozwiązywany po fetch (GLM P3); testy klucza zaufanego i rosnącej wersji (GLM P3); rebutal: klucz widoczny dla kodu scalonego na `main` (Codex P2).
R7: GLM zatwierdził. Jawne `!cancelled()` w warunku zadania (Codex P2); w `--pipeline` bez poleceń odzyskania dla etapów przed pushem (GLM P3).
R8: wydanie zawsze zakładane jako szkic i publikowane dopiero po sprawdzeniu kompletu plików, także w trybie lokalnym; `--clobber` tylko na szkicu (Codex P2, GLM P2).
R9: GLM zatwierdził. Polecenia odzyskania zaczynają od stanu wydania i nigdy nie robią `--clobber` na opublikowanym (Codex P2); push z urwanym potwierdzeniem rozpoznawany przez `ls-remote` tagu (Codex P2); kryterium 6 zgodne z ciałem speca (GLM P3); rebutal: artefakt przy anulowaniu (Codex P2).
R10: GLM zatwierdził. Sprawdzenie kompletu tylko dla plików tego wydania i czyszczenie `target/release-dist` przed zapisem (Codex P2); zapis klucza bajt w bajt (GLM P3). Limit 10 rund wyczerpany; poprawka R10 bez ponownej recenzji, decyzja o wdrożeniu należy do właściciela (AGENTS.md).
