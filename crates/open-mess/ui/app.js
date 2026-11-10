"use strict";

// The configuration window. Talks to the daemon only through its own Tauri commands. Text that may
// come from other people or from modules (names, titles, messages, manifests) goes into the page
// only through `el`, which sets textContent: never innerHTML.

/** The window's language, `pl` or `en`, as the daemon says before anything is drawn; Polish if it can't. */
let lang = "pl";
/** The language setting: `auto` (Windows's language), `pl` or `en`. */
let languageSetting = "auto";

/** The text in the window's language: every text of the window has both versions side by side. */
function tr(pl, en) {
  return lang === "en" ? en : pl;
}

/** The form for `n`: Polish forms are `{ one, few, many }`, English ones `{ one, other }`. */
function plural(n, pl, en) {
  const form = new Intl.PluralRules(lang).select(n);
  if (lang === "en") return form === "one" ? en.one : en.other;
  return pl[form] || pl.many;
}

/** The page's own texts in English: `data-en` and `data-en-aria-label` come only from index.html. */
function translatePage() {
  for (const node of document.querySelectorAll("[data-en]")) node.textContent = node.dataset.en;
  for (const node of document.querySelectorAll("[data-en-aria-label]")) node.setAttribute("aria-label", node.dataset.enAriaLabel);
}

/** A new element with `text` as its text (never parsed as HTML) and the given class. */
function el(tag, text, className) {
  const node = document.createElement(tag);
  if (text !== undefined && text !== null) node.textContent = String(text);
  if (className) node.className = className;
  return node;
}

/** A button that runs `action`, shows its error, and is disabled while it runs. */
function button(label, action, className) {
  const node = el("button", label, "action" + (className ? " " + className : ""));
  node.type = "button";
  node.addEventListener("click", async () => {
    node.disabled = true;
    try {
      await action();
      await renderFooter();
    } catch (err) {
      showError(err);
    } finally {
      node.disabled = false;
    }
  });
  return node;
}

/** A definition list of [label, value] pairs; values are text, or nodes. */
function details(pairs) {
  const list = el("dl");
  for (const [label, value] of pairs) {
    list.append(el("dt", label));
    const dd = el("dd");
    if (value instanceof Node) dd.append(value);
    else dd.textContent = String(value);
    list.append(dd);
  }
  return list;
}

function call(command, args) {
  return window.__TAURI_INTERNALS__.invoke(command, args || {});
}

function showError(err) {
  const node = document.getElementById("error");
  node.textContent = tr("Błąd: ", "Error: ") + String(err);
  node.hidden = false;
}

function clearError() {
  document.getElementById("error").hidden = true;
}

function content(section) {
  return document.querySelector("#" + section + " .content");
}

function accountStates() {
  return {
    online: [tr("połączone", "connected"), "ok"],
    offline: [tr("rozłączone", "disconnected"), "bad"],
    "auth-required": [tr("wymaga ponownego logowania", "needs signing in again"), "warn"],
    removing: [tr("usuwanie", "removing"), "warn"],
  };
}

function stateDot(state) {
  const [label, kind] = accountStates()[state] || [state, ""];
  const node = el("span", null, "row");
  node.append(el("span", null, "dot " + kind), el("span", label));
  return node;
}

/** The module signing in from this window, while it does. */
let signingIn = null;

/**
 * Signs in an account of `module`, or `account` again (the module's prompts appear in the sign-in
 * dialog meanwhile).
 */
async function signIn(module, account) {
  signingIn = module;
  renderAccounts().catch(showError);
  try {
    await call("sign_in_start", { module, account: account || null });
  } finally {
    signingIn = null;
    await renderAccounts();
  }
}

async function renderAccounts() {
  const accounts = await call("accounts");
  const modules = (await call("modules")).filter((module) => module.status === "active");
  const root = content("accounts");
  root.replaceChildren();
  const top = el("div", null, "row");
  if (signingIn) {
    top.append(el("span", tr("Trwa logowanie w module ", "Signing in with module ") + signingIn + "...", "muted"));
  } else if (modules.length === 0) {
    top.append(el("span", tr("Żeby dodać konto, zatwierdź i włącz moduł w sekcji Moduły.", "To add an account, approve and turn on a module in the Modules section."), "muted"));
  } else {
    const pick = el("select");
    for (const module of modules) {
      const option = el("option", module.name + (module.platform ? " (" + module.platform + ")" : ""));
      option.value = module.name;
      pick.append(option);
    }
    top.append(button(tr("Dodaj konto", "Add account"), () => signIn(pick.value), "primary"));
    if (modules.length > 1) top.append(pick);
  }
  root.append(top);
  if (accounts.length === 0) root.append(el("p", tr("Nie ma jeszcze żadnego konta.", "No accounts yet."), "muted"));
  for (const account of accounts) {
    const card = el("div", null, "card row");
    const who = el("div", null, "grow");
    who.append(el("strong", account.display_name), el("div", (account.platform || account.module) + " - " + account.id, "muted"));
    card.append(who, stateDot(account.state));
    if (!signingIn && account.state !== "removing" && modules.some((module) => module.name === account.module)) {
      card.append(button(tr("Zaloguj ponownie", "Sign in again"), () => signIn(account.module, account.id)));
    }
    // Not while a sign-in runs: it might be signing in this very account.
    if (!signingIn) {
      card.append(
        button(tr("Usuń", "Remove"), async () => {
          const sure = confirm(
            tr("Usunąć konto ", "Remove the account ") +
              account.display_name +
              tr("?\n\nZnikną jego sekrety, uprawnienia, akcje czekające na zatwierdzenie i pobrane załączniki.", "?\n\nIts secrets, permissions, actions waiting for approval and downloaded attachments go with it."),
          );
          if (!sure) return;
          await call("remove_account", { id: account.id });
          await renderAccounts();
        }, "danger"),
      );
    }
    root.append(card);
  }
}

function list(items, empty) {
  if (!items || items.length === 0) return el("span", empty || tr("brak", "none"), "muted");
  const node = el("ul");
  for (const item of items) node.append(el("li", item));
  return node;
}

async function renderModules() {
  const modules = await call("modules");
  const status = await call("status");
  const root = content("modules");
  root.replaceChildren();
  const top = el("div", null, "row");
  top.append(
    button(tr("Odśwież folder modułów", "Refresh the modules folder"), async () => {
      await call("rescan_modules");
      await renderModules();
    }),
    el("span", tr("Moduły to pliki .wasm w ", "Modules are .wasm files in ") + status.data_dir + "\\modules", "muted"),
  );
  root.append(top);
  root.append(
    el(
      "p",
      tr("Zatwierdzenie modułu to zaufanie do jego kodu, jak instalacja aplikacji: moduł z kontem i dostępem do hostów platformy może zrobić na tym koncie wszystko. Przy aktywnym proxy systemowym ochrona sieci lokalnej zależy od proxy.", "Approving a module means trusting its code, like installing an app: a module with an account and access to the platform's hosts can do anything on that account. With a system proxy on, the protection of the local network depends on the proxy."),
      "muted",
    ),
  );
  if (modules.length === 0) root.append(el("p", tr("W folderze nie ma modułów.", "There are no modules in the folder."), "muted"));
  for (const module of modules) {
    const card = el("div", null, "card");
    const head = el("div", null, "row");
    const title = el("div", null, "grow");
    title.append(el("strong", module.name + (module.version ? " " + module.version : "")), el("div", module.detail, "muted"));
    head.append(title);
    if (module.status === "awaiting-approval" || module.status === "replacement") {
      head.append(
        button(tr("Zatwierdź", "Approve"), async () => {
          let question = tr("Zatwierdzić moduł ", "Approve the module ") + module.name + tr(" z pliku ", " from the file ") + module.file + "?";
          if (module.takes_over) {
            question += tr("\n\nNowy plik przejmie konta i sekrety poprzedniej wersji.", "\n\nThe new file takes over the accounts and secrets of the previous version.");
          }
          if (!confirm(question)) return;
          await call("approve_module", { name: module.name, file_sha256: module.file_sha256, manifest_sha256: module.manifest_sha256 });
          await renderModules();
        }, "primary"),
      );
    }
    if (module.status === "active") {
      head.append(
        button(tr("Wyłącz", "Turn off"), async () => {
          await call("set_module_enabled", { name: module.name, enabled: false });
          await renderModules();
        }),
      );
    }
    if (module.status === "disabled" || module.status === "crashed") {
      head.append(
        button(module.status === "disabled" ? tr("Włącz", "Turn on") : tr("Uruchom ponownie", "Restart"), async () => {
          await call("set_module_enabled", { name: module.name, enabled: true });
          await renderModules();
        }),
      );
    }
    card.append(head);
    if (module.takes_over) {
      card.append(
        el(
          "p",
          tr("Po zatwierdzeniu ten plik przejmie konta i sekrety poprzedniej wersji modułu.", "Once approved, this file takes over the accounts and secrets of the module's previous version."),
          "error",
        ),
      );
    }
    if (module.changes.length > 0) {
      card.append(el("h2", tr("Zmiany względem działającej wersji", "Changes from the running version")), list(module.changes));
    }
    const wam = module.wam ? module.wam.client_id + " (" + module.wam.scopes.join(", ") + ")" : tr("brak", "none");
    const tools = module.tools.map((tool) => tool.name + (tool.writes ? tr(" (pisze)", " (writes)") : tr(" (czyta)", " (reads)")));
    card.append(
      details([
        [tr("Plik", "File"), module.file],
        [tr("Platforma", "Platform"), module.platform || "-"],
        [tr("Hosty", "Hosts"), list(module.hosts)],
        [tr("Hosty logowania", "Sign-in hosts"), list(module.sign_in_hosts)],
        [
          tr("Połączenie mediów (bez dźwięku i obrazu, poza proxy)", "Media connection (no sound or video, not through a proxy)"),
          module.media.length > 0 ? list(module.media) : tr("brak", "none"),
        ],
        [tr("Konto Windows (WAM)", "Windows account (WAM)"), wam],
        [tr("Własne narzędzia", "Own tools"), list(tools)],
        [tr("SHA-256 pliku", "File SHA-256"), el("code", module.file_sha256 || "-")],
        [tr("SHA-256 manifestu", "Manifest SHA-256"), el("code", module.manifest_sha256 || "-")],
      ]),
    );
    root.append(card);
  }
}

async function renderIntegrations() {
  const status = await call("status");
  const root = content("integrations");
  root.replaceChildren();
  root.append(el("h2", tr("Claude Code i Codex (MCP)", "Claude Code and Codex (MCP)")));
  root.append(
    el(
      "p",
      tr("Polecenia zawierają token lokalnego API: każdy, kto go zna, może używać open_mess jak agent.", "The commands contain the local API token: anyone who knows it can use open_mess as an agent."),
      "muted",
    ),
  );
  const shown = el("div");
  shown.append(
    button(tr("Pokaż polecenie", "Show the command"), async () => {
      const commands = await call("mcp_command");
      shown.replaceChildren();
      for (const [label, command] of [["Claude Code", commands.claude], [tr("Codex (do pliku config.toml)", "Codex (for the config.toml file)"), commands.codex]]) {
        shown.append(el("h2", label), el("pre", command, "content-text"), button(tr("Kopiuj", "Copy"), () => navigator.clipboard.writeText(command)));
      }
      shown.append(
        el(
          "p",
          tr("Codex: wklej to do pliku %USERPROFILE%\\.codex\\config.toml (albo config.toml w folderze z CODEX_HOME). Nie podawaj tokenu w zmiennej środowiskowej (setx, $env:): Codex przekazuje zmienne do poleceń, które uruchamia, a każdy program je dziedziczy.", "Codex: paste this into %USERPROFILE%\\.codex\\config.toml (or config.toml in the CODEX_HOME folder). Don't put the token in an environment variable (setx, $env:): Codex hands variables on to the commands it runs, and every program inherits them."),
          "muted",
        ),
      );
    }),
  );
  root.append(shown);
  root.append(el("h2", tr("Wiersz poleceń", "Command line")));
  root.append(
    details([
      [tr("Ścieżka do om", "Path to om"), el("code", status.cli)],
      [tr("Adres MCP", "MCP address"), el("code", "http://127.0.0.1:" + status.port + "/mcp")],
    ]),
  );
  root.append(
    el(
      "p",
      tr("Bez tokenu w zmiennej OPEN_MESS_TOKEN polecenie om czyta go samo z Menedżera poświadczeń.", "Without the token in OPEN_MESS_TOKEN, the om command reads it from Credential Manager itself."),
      "muted",
    ),
  );
}

function duration(seconds) {
  const h = Math.floor(seconds / 3600);
  const m = Math.floor((seconds % 3600) / 60);
  return (h > 0 ? h + " h " : "") + m + " min";
}

/** The language of the window, the tray and the notifications; the window reloads in it. */
function languagePicker() {
  const options = [["auto", tr("Automatycznie (język Windows)", "Automatic (Windows language)")], ["pl", "Polski"], ["en", "English"]];
  const pick = select(options, languageSetting);
  pick.classList.add("saves");
  pick.addEventListener("change", async () => {
    pick.disabled = true;
    try {
      await call("set_language", { setting: pick.value });
      location.reload();
    } catch (err) {
      pick.value = languageSetting;
      pick.disabled = updateHeld;
      showError(err);
    }
  });
  return field(tr("Język:", "Language:"), pick);
}

async function renderDaemon() {
  const status = await call("status");
  const root = content("daemon");
  root.replaceChildren();
  root.append(
    details([
      [tr("Wersja", "Version"), status.version],
      [tr("Działa od", "Running for"), duration(status.uptime_seconds)],
      [tr("Port lokalnego API", "Local API port"), status.port],
      [tr("Folder danych", "Data folder"), el("code", status.data_dir)],
      [tr("Konta", "Accounts"), status.accounts_online + tr(" z ", " of ") + status.accounts + tr(" połączonych", " connected")],
      [tr("Moduły", "Modules"), status.modules_active + tr(" z ", " of ") + status.modules + tr(" działa", " running")],
      [tr("Akcje do zatwierdzenia", "Actions waiting for approval"), status.waiting],
    ]),
  );
  root.append(languagePicker());
  await renderUpdates(root);
  root.append(el("h2", tr("Ostatnie błędy", "Recent errors")));
  root.append(list(status.problems, tr("Brak błędów modułów. (Dziennik demona pojawi się w następnej wersji.)", "No module errors. (The daemon's log comes in a later version.)")));
}

function updateStates() {
  return {
    idle: tr("nie ma nowszej wersji", "no newer version"),
    checking: tr("sprawdzanie...", "checking..."),
    downloading: tr("pobieranie i sprawdzanie podpisu...", "downloading and checking the signature..."),
    ready: tr("gotowa do instalacji", "ready to install"),
  };
}

function updatePhases() {
  return {
    a: tr("instalacja, faza A: agenci i wyzwalacze kończą pracę (najdłużej 30 minut)", "installing, phase A: agents and triggers finish their work (30 minutes at most)"),
    b: tr("instalacja, faza B: kończą się trwające wywołania narzędzi, nowe czekają", "installing, phase B: tool calls in progress finish, new ones wait"),
    launching: tr("uruchamianie pomocnika aktualizacji", "starting the update helper"),
  };
}

function updateStateText(update) {
  if (update.probation) return tr("aktualizacja w toku: nowa wersja jest sprawdzana", "update in progress: the new version is being checked");
  const state = update.state;
  if (state.kind === "error") return tr("błąd: ", "error: ") + state.detail;
  if (state.kind === "phase") return updatePhases()[state.detail] || state.detail;
  return updateStates()[state.kind] || state.kind;
}

/** The Aktualizacje block of Demon i logi (spec 2026-10-27-self-update, 6 and 7). */
async function renderUpdates(root) {
  const update = await call("update_status").catch(() => null);
  if (!update) return;
  root.append(el("h2", tr("Aktualizacje", "Updates")));
  let found = tr("brak", "none");
  if (update.found) {
    found = update.found.version;
    if (update.found.rolled_back) {
      found += tr(" (wycofane: jej instalacja została cofnięta, więc nie instaluje się sama)", " (rolled back: its installation was undone, so it does not install itself)");
    }
  }
  root.append(
    details([
      [tr("Bieżąca wersja", "Current version"), update.current],
      [tr("Znaleziona wersja", "Found version"), found],
      [tr("Stan", "State"), updateStateText(update)],
      [tr("Ostatni wynik", "Last result"), update.last_result || tr("brak", "none")],
    ]),
  );
  const row = el("div", null, "row");
  const phase = update.state.kind === "phase";
  if (!phase) {
    row.append(
      button(tr("Sprawdź teraz", "Check now"), async () => {
        await call("check_update");
        watchUpdate();
      }),
    );
  }
  if (update.state.kind === "ready" && !update.not_installable) {
    const install = button(tr("Zainstaluj", "Install"), async () => {
      const sure = confirm(
        tr("Zainstalować open_mess ", "Install open_mess ") +
          update.found.version +
          tr("?\n\nDemon poczeka, aż agenci skończą pracę, i uruchomi się ponownie w nowej wersji.", "?\n\nThe daemon waits for the agents to finish their work and starts again in the new version."),
      );
      if (!sure) return;
      await call("install_update");
      await renderDaemon();
    }, "primary saves");
    row.append(install);
  }
  if (phase && update.state.detail !== "launching") {
    row.append(
      button(tr("Anuluj", "Cancel"), async () => {
        await call("cancel_update");
        await renderDaemon();
      }, "danger"),
    );
  }
  root.append(row);
  const box = el("input", null, "saves");
  box.type = "checkbox";
  box.checked = update.auto_install;
  box.addEventListener("change", async () => {
    box.disabled = true;
    try {
      await call("set_auto_install", { on: box.checked });
    } catch (err) {
      box.checked = !box.checked;
      showError(err);
    } finally {
      box.disabled = updateHeld;
    }
  });
  const auto = el("label", null, "row");
  auto.append(box, el("span", tr("Instaluj aktualizacje automatycznie", "Install updates automatically")));
  root.append(auto);
  if (update.not_installable) {
    root.append(el("p", update.not_installable, "muted"));
  } else {
    root.append(
      el(
        "p",
        tr("Sama instaluje się tylko wtedy, gdy nic nie czeka na zatwierdzenie, żaden agent nie pracuje i żadna reguła nie doręcza.", "An update installs itself only when nothing waits for approval, no agent is working and no rule is delivering."),
        "muted",
      ),
    );
  }
  holdSaves(update.probation);
}

/** Whether saves are refused: an update's new version is being checked (spec 7.2). */
let updateHeld = false;
const SAVING_SECTIONS = ["accounts", "permissions", "triggers", "waiting", "modules"];

/** Greys out what saves, and says why; the daemon refuses the saves anyway. */
function holdSaves(held) {
  updateHeld = held;
  for (const section of SAVING_SECTIONS) content(section).inert = held;
  for (const node of document.querySelectorAll("#daemon .saves")) node.disabled = held;
  document.getElementById("update-held").hidden = !held;
}

let updateTimer = null;

/** Looks at the update again in a moment: the daemon checks and installs in the background. */
function watchUpdate() {
  clearTimeout(updateTimer);
  updateTimer = setTimeout(() => {
    renderFooter().catch(showError);
    if (current === "daemon") renderDaemon().catch(showError);
  }, 1500);
}

async function renderFooter() {
  const status = await call("status");
  const connected = plural(
    status.accounts,
    { one: "konto połączone", few: "konta połączone", many: "kont połączonych" },
    { one: "account connected", other: "accounts connected" },
  );
  document.getElementById("daemon-state").textContent =
    tr("Demon działa - port ", "Daemon running - port ") + status.port + "\n" + status.accounts_online + "/" + status.accounts + " " + connected;
  const badge = document.getElementById("waiting-count");
  badge.textContent = String(status.waiting);
  badge.hidden = status.waiting === 0;
  const update = await call("update_status").catch(() => null);
  holdSaves(Boolean(update && update.probation));
  if (update && (update.probation || ["checking", "downloading", "phase"].includes(update.state.kind))) watchUpdate();
}

/** The write permission's labels; autonomous_rule() in commands.rs quotes two of them. */
function writeOptions() {
  return [
    ["never", tr("nigdy", "never")],
    ["confirm", tr("z potwierdzeniem", "with confirmation")],
    ["autonomous", tr("samodzielnie", "autonomously")],
  ];
}

/**
 * The read box and write list for one account (conversation null) or conversation, saving both
 * on every change; a change that fails is put back. Both are disabled while a save runs, so two
 * quick changes can't be saved in the wrong order and leave the agent more than the window shows.
 */
function permissionControls(account, conversation, current) {
  const box = el("input");
  box.type = "checkbox";
  box.checked = current.read;
  const read = el("label", null, "row");
  read.append(box, el("span", tr("czytanie", "reading")));
  const write = el("select");
  for (const [value, label] of writeOptions()) {
    const option = el("option", label);
    option.value = value;
    write.append(option);
  }
  write.value = current.write;
  const save = async () => {
    if (write.value === "autonomous" && current.write !== "autonomous") {
      const sure = confirm(tr("Samodzielnie: agent wyśle, poprawi i zareaguje bez pytania. Ustawić?", "Autonomously: the agent sends, edits and reacts without asking. Set it?"));
      if (!sure) {
        write.value = current.write;
        return;
      }
    }
    box.disabled = write.disabled = true;
    try {
      await call("set_permissions", { account, conversation, read: box.checked, write: write.value });
      current = { read: box.checked, write: write.value };
    } catch (err) {
      box.checked = current.read;
      write.value = current.write;
      showError(err);
    } finally {
      box.disabled = write.disabled = false;
    }
  };
  box.addEventListener("change", save);
  write.addEventListener("change", save);
  const writeLabel = el("label", null, "row");
  writeLabel.append(el("span", tr("pisanie:", "writing:")), write);
  return [read, writeLabel];
}

async function renderPermissions() {
  const accounts = await call("accounts");
  const root = content("permissions");
  root.replaceChildren();
  root.append(
    el(
      "p",
      tr("Te same uprawnienia dotyczą każdego agenta i polecenia om. Ustawienie rozmowy ma pierwszeństwo przed ustawieniem konta. Z potwierdzeniem: każde wysłanie, edycja i reakcja czeka na Twoją decyzję w powiadomieniu albo w sekcji Do zatwierdzenia. Narzędzia piszące modułów (np. dołączenie do spotkania dla napisów) czekają na nią zawsze, także przy zapisie samodzielnym.", "The same permissions apply to every agent and to the om command. A conversation's setting comes before the account's. With confirmation: every send, edit and reaction waits for your decision in a notification or in the Waiting for approval section. A module's write tools (such as joining a meeting for its captions) always wait for it, even when writing on its own is allowed."),
      "muted",
    ),
  );
  if (accounts.length === 0) root.append(el("p", tr("Nie ma jeszcze żadnego konta.", "No accounts yet."), "muted"));
  for (const account of accounts) {
    const permissions = await call("permissions", { account: account.id });
    const card = el("div", null, "card");
    card.append(el("strong", account.display_name), el("div", account.id, "muted"));
    const own = el("div", null, "row");
    own.append(el("span", tr("Całe konto:", "Whole account:")), ...permissionControls(account.id, null, permissions));
    card.append(own, el("h2", tr("Rozmowy z własnymi uprawnieniami", "Conversations with their own permissions")));
    const table = el("table");
    for (const item of permissions.overrides) {
      const row = el("tr");
      const id = el("td");
      id.append(el("code", item.conversation));
      const controls = el("td", null, "row");
      controls.append(...permissionControls(account.id, item.conversation, item));
      const remove = el("td");
      remove.append(
        button(tr("Usuń", "Remove"), async () => {
          await call("clear_permission_override", { account: account.id, conversation: item.conversation });
          await renderPermissions();
        }, "danger"),
      );
      row.append(id, controls, remove);
      table.append(row);
    }
    if (permissions.overrides.length === 0) {
      card.append(el("p", tr("Brak - wszystkie rozmowy mają ustawienia konta.", "None - every conversation has the account's settings."), "muted"));
    } else card.append(table);
    const add = el("div", null, "row");
    const id = el("input");
    id.type = "text";
    id.placeholder = tr("identyfikator rozmowy", "conversation id");
    add.append(
      id,
      button(tr("Dodaj rozmowę", "Add conversation"), async () => {
        if (!id.value.trim()) return;
        // A new conversation starts with the account's permissions as they are now (they may have
        // changed since this section was drawn); change them in its row.
        const now = await call("permissions", { account: account.id });
        await call("set_permissions", { account: account.id, conversation: id.value, read: now.read, write: now.write });
        await renderPermissions();
      }),
    );
    card.append(add);
    root.append(card);
  }
  // The local recordings: one read permission, apart from the accounts' (spec
  // 2026-10-28-local-transcription, 4.5); only its read box is shown.
  const local = await call("permissions", { account: "local" });
  const card = el("div", null, "card");
  card.append(
    el("strong", tr("Nagrania lokalne", "Local recordings")),
    el(
      "div",
      tr("Transkrypcje rozmów nagranych na tym komputerze. Niezależne od uprawnień kont: agent z tym prawem czyta każde nagranie.", "Transcripts of the calls recorded on this computer. Apart from the accounts' permissions: an agent with this right reads every recording."),
      "muted",
    ),
  );
  const row = el("div", null, "row");
  row.append(permissionControls("local", null, local)[0]);
  card.append(row);
  root.append(card);
}

function kinds() {
  return { send: tr("Wysłanie", "Send"), edit: tr("Edycja", "Edit"), react: tr("Reakcja", "Reaction") };
}

function outcomes() {
  return {
    done: tr("Wykonano.", "Done."),
    rejected: tr("Odrzucono.", "Rejected."),
    expired: tr("Wygasło - nic nie wysłano.", "Expired - nothing was sent."),
    voided: tr("Uczestnicy, tytuł rozmowy, wiadomość albo prawo zapisu zmieniły się od pokazania - nic nie wysłano.", "The participants, the conversation's title, the message or the write permission changed since it was shown - nothing was sent."),
    uncertain: tr("Mogło dotrzeć - sprawdź rozmowę, zanim spróbujesz ponownie.", "It may have arrived - check the conversation before you try again."),
    failed: tr("Nie udało się - nic nie wysłano.", "It failed - nothing was sent."),
  };
}

/** The cards on screen by action, so a new or decided action leaves the others (and where they are scrolled) alone. */
const waitingCards = new Map();

/** One waiting action with everything it would do, in full; long content scrolls. */
function waitingCard(preview) {
  const card = el("div", null, "card");
  card.append(el("strong", kinds()[preview.kind] || preview.kind));
  const participants = preview.participants.length > 0 ? preview.participants.join(", ") : tr("brak (kanał)", "none (channel)");
  // A module tool's approval is for the conversation: who is in it now is shown, not approved.
  const tool = preview.description !== null;
  card.append(
    details([
      [tr("Konto", "Account"), preview.account],
      [tr("Rozmowa", "Conversation"), preview.conversation],
      [tool ? tr("Teraz w rozmowie", "In it now") : tr("Uczestnicy", "Participants"), participants],
    ]),
  );
  const block = (label, text) => card.append(el("h2", label), el("div", text, "content-text"));
  if (tool) {
    block(tr("Co zrobi narzędzie modułu", "What the module's tool does"), preview.description);
    if (preview.arguments.length > 0) block(tr("Argumenty", "Arguments"), preview.arguments.join("\n"));
    card.append(el("p", tr("Zgoda dotyczy rozmowy, nie tylko osób, które są w niej teraz.", "The approval is for the conversation, not only for the people in it now."), "muted"));
  } else if (preview.kind === "send") {
    block(tr("Treść", "Text"), preview.text);
    if (preview.target_text !== null) block(tr("W odpowiedzi na", "In reply to"), preview.target_text);
  } else if (preview.kind === "edit") {
    block(tr("Przed", "Before"), preview.target_text);
    block(tr("Po", "After"), preview.text);
  } else {
    block(tr("Reakcja", "Reaction"), preview.reaction);
    block(tr("Pod wiadomością", "Under the message"), preview.target_text);
  }
  const decide = (approve) => async () => {
    const outcome = await call("decide", { action: preview.action, approve });
    document.getElementById("waiting-outcome").textContent = outcomes()[outcome] || outcome;
    await renderWaiting();
  };
  const row = el("div", null, "row");
  const approve = button(tr("Zatwierdź", "Approve"), decide(true), "primary");
  if (preview.target_cut) {
    // Nothing is approved that is not on screen in full.
    approve.disabled = true;
    card.append(
      el(
        "p",
        tr("Wiadomość, której dotyczy akcja, jest za długa, by pokazać ją w całości - akcję można tylko odrzucić.", "The message this action is about is too long to show in full - the action can only be rejected."),
        "error",
      ),
    );
  }
  row.append(approve, button(tr("Odrzuć", "Reject"), decide(false), "danger"));
  card.append(row);
  return card;
}

/** One refresh at a time, in order, so an older answer never undoes a newer one. */
let waitingRefresh = Promise.resolve();

function renderWaiting() {
  waitingRefresh = waitingRefresh.then(refreshWaiting, refreshWaiting);
  return waitingRefresh;
}

async function refreshWaiting() {
  const waiting = await call("waiting");
  const root = content("waiting");
  if (!document.getElementById("waiting-outcome")) {
    const outcome = el("p", null, "muted");
    outcome.id = "waiting-outcome";
    root.append(outcome, el("p", tr("Nic nie czeka na zatwierdzenie.", "Nothing is waiting for approval."), "muted empty"));
  }
  const ids = new Set(waiting.map((preview) => preview.action));
  for (const [action, card] of waitingCards) {
    if (!ids.has(action)) {
      card.remove();
      waitingCards.delete(action);
    }
  }
  for (const preview of waiting) {
    if (!waitingCards.has(preview.action)) {
      const card = waitingCard(preview);
      waitingCards.set(preview.action, card);
      root.append(card);
    }
  }
  root.querySelector(".empty").hidden = waiting.length > 0;
}

/** The module's sign-in prompt in a dialog over the window; it disappears once nothing waits. */
async function refreshSignIn() {
  const prompt = await call("sign_in_request");
  const overlay = document.getElementById("sign-in");
  const dialog = overlay.querySelector(".dialog");
  dialog.replaceChildren();
  overlay.hidden = !prompt;
  if (!prompt) return;
  dialog.append(el("h1", tr("Logowanie - ", "Sign in - ") + prompt.module));
  if (prompt.kind === "device-code") {
    dialog.append(
      el("p", tr("Otwórz stronę logowania w przeglądarce i wpisz ten kod:", "Open the sign-in page in your browser and enter this code:")),
      el("div", prompt.code, "code"),
      el("p", prompt.url, "muted"),
    );
    dialog.append(button(tr("Otwórz stronę w przeglądarce", "Open the page in the browser"), () => call("sign_in_open_link", { request_id: prompt.id })));
  } else if (prompt.kind === "qr") {
    dialog.append(el("p", tr("Zeskanuj ten kod telefonem:", "Scan this code with your phone:")));
    if (prompt.qr_svg) {
      const image = el("img");
      image.alt = tr("kod QR", "QR code");
      // An image, so nothing in it runs.
      image.src = "data:image/svg+xml;charset=utf-8," + encodeURIComponent(prompt.qr_svg);
      dialog.append(image);
    } else {
      dialog.append(el("p", tr("Kod jest za długi, żeby go pokazać.", "The code is too long to show."), "error"));
    }
  } else if (prompt.kind === "page") {
    dialog.append(
      el(
        "p",
        tr("Zaloguj się w osobnym oknie strony logowania (", "Sign in in the sign-in page's own window (") +
          prompt.url +
          tr("). Hasło wpisujesz tylko tam.", "). You enter your password only there."),
      ),
    );
  } else if (prompt.kind === "ask-host") {
    dialog.append(el("p", tr("Strona logowania przechodzi na ", "The sign-in page goes on to ") + prompt.url + tr(". Zezwolić?", ". Allow it?")));
    dialog.append(
      el(
        "p",
        tr("Zgoda doda ten host do zatwierdzonych hostów logowania modułu (np. firmowy serwer logowania). Odmowa przerwie logowanie.", "Allowing adds this host to the module's approved sign-in hosts (a company sign-in server, say). Refusing ends the sign-in."),
        "muted",
      ),
    );
    const row = el("div", null, "row");
    row.append(
      button(tr("Zezwól", "Allow"), () => call("sign_in_answer", { request_id: prompt.id, reply: { done: "allow" } }), "primary"),
      button(tr("Nie zezwalaj", "Don't allow"), () => call("sign_in_answer", { request_id: prompt.id, reply: "cancelled" }), "danger"),
    );
    dialog.append(row);
    return;
  } else if (prompt.kind === "wam-pick") {
    const answer = (done) => () => call("sign_in_answer", { request_id: prompt.id, reply: { done } });
    dialog.append(el("p", tr("Wybierz konto Windows, którym moduł ma się zalogować:", "Choose the Windows account the module signs in with:")));
    if (prompt.accounts.length === 0) {
      dialog.append(el("p", tr("Windows nie zna jeszcze żadnego konta służbowego dla tego modułu.", "Windows knows no work account for this module yet."), "muted"));
    }
    // The same name in several organizations (as a guest, say) shows the organization's id.
    const total = new Map();
    for (const account of prompt.accounts) total.set(account.user_name, (total.get(account.user_name) || 0) + 1);
    for (const account of prompt.accounts) {
      const label = account.user_name + (total.get(account.user_name) > 1 ? tr(" (organizacja ", " (organization ") + (account.tenant || "?") + ")" : "");
      dialog.append(button(label, answer(account.id), "choice"));
    }
    dialog.append(button(tr("Inne konto Windows...", "Another Windows account..."), answer(null), "choice"));
  }
  const row = el("div", null, "row");
  row.append(button(tr("Anuluj", "Cancel"), () => call("sign_in_answer", { request_id: prompt.id, reply: "cancelled" }), "danger"));
  dialog.append(row);
}

/** One refresh at a time, in order, so an older answer never undoes a newer one. */
let signInRefresh = Promise.resolve();

function renderSignIn() {
  signInRefresh = signInRefresh.then(refreshSignIn, refreshSignIn);
  return signInRefresh;
}

// Sent by the daemon whenever the sign-in prompt changes.
window.addEventListener("sign-in-request", () => renderSignIn().catch(showError));

function conditions() {
  return [
    ["direct", tr("wiadomość prywatna (DM)", "a direct message (DM)")],
    ["mention", tr("wzmianka o mnie", "a mention of me")],
    ["conversation", tr("nowa wiadomość w wybranej rozmowie", "a new message in a chosen conversation")],
    ["from", tr("wiadomość od wybranych osób", "a message from chosen people")],
  ];
}

function drops() {
  return [
    ["dropped_rate", tr("ponad limit na godzinę", "over the hourly limit")],
    ["dropped_quiet_hours", tr("w ciszy nocnej", "in quiet hours")],
    ["dropped_queue", tr("wypchnięte z kolejki", "pushed out of the queue")],
    ["dropped_no_session", tr("bez sesji i programu", "no session and no program")],
  ];
}

function conditionText(condition) {
  const [, label] = conditions().find(([type]) => type === condition.type) || [condition.type, condition.type];
  if (condition.type === "conversation") return label + ": " + condition.id;
  if (condition.type === "from") return label + ": " + condition.senders.join(", ");
  return label;
}

function spawnText(spawn) {
  return [spawn.program, ...spawn.args].join(" ") + (spawn.working_dir ? tr(" (w ", " (in ") + spawn.working_dir + ")" : "");
}

function channelText() {
  return tr("wpis do otwartej sesji Claude Code (kanał)", "a message to an open Claude Code session (channel)");
}

function actionText(action) {
  if (action.type === "spawn") return tr("uruchom program: ", "start a program: ") + spawnText(action);
  const fallback = action.fallback_spawn ? tr("; bez sesji uruchom: ", "; without a session start: ") + spawnText(action.fallback_spawn) : "";
  return channelText() + fallback;
}

/** A labelled form field. */
function field(label, input) {
  const node = el("label", null, "row");
  node.append(el("span", label), input);
  return node;
}

function input(type, value) {
  const node = el("input");
  node.type = type;
  if (value !== undefined && value !== null) node.value = String(value);
  return node;
}

function select(options, value) {
  const node = el("select");
  for (const [key, label] of options) {
    const option = el("option", label);
    option.value = key;
    node.append(option);
  }
  if (value !== undefined) node.value = value;
  return node;
}

/** The program, arguments and folder of a spawn action; `read()` gives the spawn or null. */
function spawnFields(spawn) {
  const program = input("text", spawn ? spawn.program : "");
  program.readOnly = true;
  program.placeholder = tr("wybierz program", "choose a program");
  const pick = button(tr("Wybierz program...", "Choose a program..."), async () => {
    const picked = await call("pick_program");
    if (picked) program.value = picked;
  });
  const args = el("textarea");
  args.rows = 3;
  args.value = spawn ? spawn.args.join("\n") : "";
  const dir = input("text", spawn ? spawn.working_dir : "");
  dir.placeholder = tr("pełna ścieżka, opcjonalnie", "full path, optional");
  const node = el("div");
  const programRow = el("div", null, "row");
  const clear = button(tr("Bez programu", "No program"), () => {
    program.value = "";
  });
  programRow.append(el("span", "Program:"), program, pick, clear);
  node.append(programRow, field(tr("Argumenty (stałe, po jednym w wierszu):", "Arguments (fixed, one per line):"), args), field(tr("Folder roboczy:", "Working folder:"), dir));
  node.append(
    el(
      "p",
      tr("Treść wiadomości nigdy nie trafia do argumentów: program dostaje ją na wejściu jako dane niezaufane.", "Message text never goes into the arguments: the program gets it on its input as untrusted data."),
      "muted",
    ),
  );
  const read = () => {
    if (!program.value) return null;
    return { program: program.value, args: args.value.split("\n").filter((line) => line !== ""), working_dir: dir.value.trim() || null };
  };
  return { node, read };
}

/** The form that adds a rule (`existing` null) or changes one. */
function ruleForm(accounts, existing, done) {
  const rule = existing || {
    // A new rule's id (1-64 characters); randomUUID needs a secure context, which the page is.
    id: crypto.randomUUID ? crypto.randomUUID() : "r" + Date.now() + Math.random().toString(16).slice(2),
    account: accounts[0].id,
    enabled: true,
    condition: { type: "direct" },
    action: { type: "channel", fallback_spawn: null },
    quiet_hours: null,
    max_per_hour: 6,
    quiet_ms: 60000,
    max_delay_ms: 300000,
  };
  const card = el("div", null, "card");
  card.append(el("h2", existing ? tr("Zmień regułę", "Change the rule") : tr("Nowa reguła", "New rule")));
  const account = select(accounts.map((item) => [item.id, item.display_name + " (" + item.id + ")"]), rule.account);
  const condition = select(conditions(), rule.condition.type);
  const conditionValue = input("text", rule.condition.id || (rule.condition.senders || []).join(", "));
  const conditionRow = field("", conditionValue);
  const showCondition = () => {
    conditionRow.hidden = condition.value !== "conversation" && condition.value !== "from";
    conditionRow.firstChild.textContent =
      condition.value === "conversation" ? tr("Identyfikator rozmowy:", "Conversation id:") : tr("Identyfikatory osób (po przecinku):", "People's ids (comma-separated):");
  };
  condition.addEventListener("change", showCondition);
  showCondition();

  const action = select([["channel", channelText()], ["spawn", tr("uruchom program", "start a program")]], rule.action.type);
  const spawn = spawnFields(rule.action.type === "spawn" ? rule.action : rule.action.fallback_spawn);
  const fallbackNote = el(
    "p",
    tr("Gdy nie ma otwartej sesji z kanałami, reguła uruchomi program poniżej (zostaw pusty, żeby nic nie robić).", "When no session with channels is open, the rule starts the program below (leave it empty to do nothing)."),
    "muted",
  );
  const showAction = () => {
    fallbackNote.hidden = action.value !== "channel";
  };
  action.addEventListener("change", showAction);
  showAction();

  const quiet = input("checkbox");
  quiet.checked = !!rule.quiet_hours;
  const quietFrom = input("time", rule.quiet_hours ? rule.quiet_hours[0].slice(0, 5) : "22:00");
  const quietTo = input("time", rule.quiet_hours ? rule.quiet_hours[1].slice(0, 5) : "07:00");
  const quietRow = el("div", null, "row");
  quietRow.append(quiet, el("span", tr("Cisza nocna od", "Quiet hours from")), quietFrom, el("span", tr("do", "to")), quietTo);
  const perHour = input("number", rule.max_per_hour);
  const quietSeconds = input("number", rule.quiet_ms / 1000);
  const delaySeconds = input("number", rule.max_delay_ms / 1000);
  const enabled = input("checkbox");
  enabled.checked = rule.enabled;

  card.append(
    field(tr("Konto:", "Account:"), account),
    field(tr("Gdy przyjdzie:", "When there is:"), condition),
    conditionRow,
    field(tr("Akcja:", "Action:"), action),
    fallbackNote,
    spawn.node,
    quietRow,
    field(tr("Najwyżej pobudek na godzinę:", "Wake-ups per hour, at most:"), perHour),
    field(tr("Wyślij po tylu sekundach ciszy w rozmowie:", "Send after this many seconds of quiet in the conversation:"), quietSeconds),
    field(tr("...ale najpóźniej po tylu sekundach:", "...but at the latest after this many seconds:"), delaySeconds),
    field(tr("Włączona:", "On:"), enabled),
  );
  const row = el("div", null, "row");
  row.append(
    button(tr("Zapisz", "Save"), async () => {
      if (quiet.checked && (!quietFrom.value || !quietTo.value)) throw tr("podaj obie godziny ciszy nocnej", "enter both quiet hours");
      const values = conditionValue.value.split(",").map((value) => value.trim()).filter((value) => value !== "");
      const conditionDto =
        condition.value === "conversation"
          ? { type: "conversation", id: conditionValue.value.trim() }
          : condition.value === "from"
            ? { type: "from", senders: values }
            : { type: condition.value };
      const program = spawn.read();
      const actionDto = action.value === "spawn" ? Object.assign({ type: "spawn" }, program || { program: "", args: [], working_dir: null }) : { type: "channel", fallback_spawn: program };
      const warning = await call("put_rule", {
        rule: {
          id: rule.id,
          account: account.value,
          enabled: enabled.checked,
          condition: conditionDto,
          action: actionDto,
          quiet_hours: quiet.checked ? [quietFrom.value + ":00", quietTo.value + ":00"] : null,
          max_per_hour: Number(perHour.value),
          quiet_ms: Math.round(Number(quietSeconds.value) * 1000),
          max_delay_ms: Math.round(Number(delaySeconds.value) * 1000),
        },
      });
      if (warning) alert(warning);
      await done();
    }, "primary"),
    button(tr("Anuluj", "Cancel"), done),
  );
  card.append(row);
  return card;
}

async function renderTriggers() {
  const [rules, accounts] = await Promise.all([call("rules"), call("accounts")]);
  const root = content("triggers");
  root.replaceChildren();
  root.append(
    el(
      "p",
      tr("Reguła budzi agenta, gdy ktoś napisze do Ciebie: wpisem do otwartej sesji Claude Code albo uruchomieniem programu. Twoje własne wiadomości nigdy jej nie uruchamiają, a rozmowy, których agent nie może czytać, są pomijane. Treść od innych osób agent dostaje jako dane niezaufane.", "A rule wakes an agent when someone writes to you: with a message to an open Claude Code session or by starting a program. Your own messages never set it off, and conversations the agent may not read are skipped. The agent gets what other people wrote as untrusted data."),
      "muted",
    ),
  );
  const editor = el("div");
  const close = async () => renderTriggers();
  if (accounts.length === 0) {
    root.append(el("p", tr("Reguły należą do kont: najpierw dodaj konto.", "Rules belong to accounts: add an account first."), "muted"));
  } else {
    root.append(button(tr("Dodaj regułę", "Add rule"), () => editor.replaceChildren(ruleForm(accounts, null, close)), "primary"));
  }
  root.append(editor);
  if (rules.length === 0) root.append(el("p", tr("Nie ma jeszcze żadnej reguły.", "No rules yet."), "muted"));
  for (const rule of rules) {
    const card = el("div", null, "card");
    const head = el("div", null, "row");
    const title = el("div", null, "grow");
    const owner = accounts.find((account) => account.id === rule.account);
    title.append(
      el("strong", conditionText(rule.condition)),
      el("div", (owner ? owner.display_name + " - " : "") + rule.account + (rule.enabled ? "" : tr(" (wyłączona)", " (off)")), "muted"),
    );
    head.append(
      title,
      button(tr("Zmień", "Change"), () => editor.replaceChildren(ruleForm(accounts, rule, close))),
      button(tr("Usuń", "Remove"), async () => {
        if (!confirm(tr("Usunąć regułę?", "Remove the rule?"))) return;
        await call("delete_rule", { id: rule.id });
        await renderTriggers();
      }, "danger"),
    );
    card.append(head);
    const quiet = rule.quiet_hours ? rule.quiet_hours[0].slice(0, 5) + "-" + rule.quiet_hours[1].slice(0, 5) : tr("brak", "none");
    const counters = rule.counters;
    const dropped = drops()
      .filter(([key]) => counters[key] > 0)
      .map(([key, label]) => label + ": " + counters[key]);
    const limits =
      rule.max_per_hour +
      tr(" na godzinę; po ", " per hour; after ") +
      rule.quiet_ms / 1000 +
      tr(" s ciszy, najpóźniej po ", " s of quiet, at the latest after ") +
      rule.max_delay_ms / 1000 +
      " s";
    card.append(
      details([
        [tr("Akcja", "Action"), actionText(rule.action)],
        [tr("Cisza nocna", "Quiet hours"), quiet],
        [tr("Limity", "Limits"), limits],
        [tr("Od startu demona", "Since the daemon started"), tr("dostarczone: ", "delivered: ") + counters.delivered + (dropped.length ? tr("; pominięte - ", "; dropped - ") + dropped.join(", ") : "")],
      ]),
    );
    if (counters.failed_to_start > 0) {
      const times = plural(counters.failed_to_start, { one: "raz", few: "razy", many: "razy" }, { one: "time", other: "times" });
      card.append(
        el(
          "p",
          tr("Program nie uruchomił się ", "The program failed to start ") +
            counters.failed_to_start +
            " " +
            times +
            tr(" - sprawdź, czy nadal jest pod tą ścieżką, i wybierz go ponownie.", " - check that it is still at that path, and choose it again."),
          "error",
        ),
      );
    }
    root.append(card);
  }

  root.append(el("h2", tr("Sesja Claude Code z kanałami", "Claude Code session with channels")));
  root.append(
    el(
      "p",
      tr("Wpis do sesji działa w sesjach Claude Code uruchomionych z kanałem open_mess. Polecenie om mcp-stdio należy tylko do takich sesji: nie dodawaj go do zwykłej konfiguracji MCP (tam open_mess działa przez adres z sekcji Integracje).", "A message to a session works in Claude Code sessions started with the open_mess channel. The om mcp-stdio command belongs only in such sessions: don't add it to your usual MCP configuration (there open_mess works through the address in the Integrations section)."),
      "muted",
    ),
  );
  const setup = el("div");
  setup.append(
    button(tr("Pokaż konfigurację kanału", "Show the channel configuration"), async () => {
      const channel = await call("channel_setup");
      setup.replaceChildren(
        el("p", tr("Konfiguracja MCP (zapisana w ", "MCP configuration (saved in ") + channel.file + "):", "muted"),
        el("pre", channel.json, "content-text"),
        el("p", tr("Uruchom Claude Code tak:", "Start Claude Code like this:"), "muted"),
        el("pre", channel.command, "content-text"),
        button(tr("Kopiuj polecenie", "Copy the command"), () => navigator.clipboard.writeText(channel.command)),
        el("p", tr("Otwarte sesje z kanałem: ", "Open sessions with the channel: ") + channel.sessions + ".", "muted"),
      );
    }),
  );
  root.append(setup);
}

function megabytes(bytes) {
  return Math.round(bytes / (1024 * 1024)).toLocaleString(locale()) + " MB";
}

/** The locale dates and numbers are written in. */
function locale() {
  return tr("pl-PL", "en-GB");
}

function recordingStates() {
  return {
    recording: tr("nagrywanie", "recording"),
    waiting: tr("czeka na transkrypcję", "waiting for transcription"),
    transcribing: tr("transkrypcja", "transcribing"),
    done: tr("gotowe", "done"),
    failed: tr("nieudane", "failed"),
  };
}

/** A reason the daemon gives in English (a recording's, the queue's, the recorder's), as shown. */
function recordingReason(reason) {
  const texts = {
    interrupted: tr("przerwane", "interrupted"),
    expired: tr("dźwięk wygasł", "expired"),
    damaged: tr("uszkodzone", "damaged"),
    failed: tr("błąd", "failed"),
    "capture failed": tr("błąd przechwytu", "capture failed"),
    "write failed": tr("błąd zapisu", "write failed"),
    "detect failed": tr("utrata wykrywania", "detect failed"),
    "too long": tr("za długie", "too long"),
    "low disk": tr("mało miejsca na dysku", "low disk"),
    recording: tr("trwa nagrywanie", "a call is being recorded"),
    "no model": tr("brak modelu", "no model"),
    "model damaged": tr("model uszkodzony", "model damaged"),
    "low memory": tr("za mało pamięci", "low memory"),
    "cannot open the call's devices": tr("nie da się otworzyć urządzeń rozmowy", "cannot open the call's devices"),
    "no tray icon": tr("brak ikony w zasobniku", "no tray icon"),
    "audio limit": tr("limit dźwięku", "audio limit"),
    "text limit": tr("limit transkrypcji", "text limit"),
    "update in progress": tr("trwa aktualizacja", "update in progress"),
    "cannot create the recording": tr("błąd tworzenia nagrania", "cannot create the recording"),
    stopped: tr("zatrzymane", "stopped"),
    auto_off: tr("wyłączony zapis", "switched off"),
    e2ee: tr("spotkanie szyfrowane", "end-to-end encrypted"),
    lobby: tr("poczekalnia", "lobby"),
    policy: tr("polityka bez napisów", "no captions by policy"),
    media_failed: tr("błąd połączenia", "connection failed"),
    proxy: tr("serwer pośredniczący", "proxy"),
    protocol: tr("błąd protokołu", "protocol error"),
    refused: tr("odmowa Teams", "refused by Teams"),
    connection_limit: tr("limit rozmów", "call limit"),
    "module failed": tr("błąd modułu", "module failed"),
    late_start: tr("późny start", "late start"),
    mixed: tr("różne", "mixed"),
    captions_off: tr("napisy wyłączone", "captions off"),
    lost: tr("utracone", "lost"),
    rejoin: tr("ponowne dołączanie", "rejoining"),
  };
  return texts[reason] || reason;
}

/** The recording's source, as the list shows it. */
function recordingSources() {
  return { audio: tr("dźwięk", "audio"), captions: tr("napisy", "captions") };
}

function speakers() {
  return { me: tr("ja", "me"), others: tr("rozmówcy", "others") };
}

function recordingLanguages() {
  return [
    ["auto", tr("wykryj sam", "detect")],
    ["pl", tr("polski", "Polish")],
    ["en", tr("angielski", "English")],
    ["de", tr("niemiecki", "German")],
    ["uk", tr("ukraiński", "Ukrainian")],
    ["fr", tr("francuski", "French")],
    ["es", tr("hiszpański", "Spanish")],
  ];
}

/** HH:MM:SS of `ms`. */
function clock(ms) {
  const seconds = Math.floor(ms / 1000);
  return [Math.floor(seconds / 3600), Math.floor(seconds / 60) % 60, seconds % 60].map((part) => String(part).padStart(2, "0")).join(":");
}

/** The recording whose transcript the show button shows, and its pieces. */
let shownRecording = null;

// Call recording (spec 2026-10-28-local-transcription, 4.6): the settings, the model and the
// recordings. A transcript is other people's speech: it goes in through `el` only.
async function renderRecording() {
  const [model, recording] = await Promise.all([call("model_status"), call("recordings")]);
  const root = content("recording");
  root.replaceChildren();
  renderCaptionsKept(root, recording);
  renderRecordingSettings(root, recording);
  renderCaptionsSettings(root, recording);
  root.append(
    el(
      "p",
      tr("Rozmowy są zamieniane na tekst na tym komputerze, modelem Whisper (large-v3-turbo, licencja MIT). Model pobiera się raz, z Hugging Face; każdy plik jest sprawdzany sumą kontrolną.", "Calls are turned into text on this computer, with the Whisper model (large-v3-turbo, MIT licence). The model is downloaded once, from Hugging Face; every file is checked against its checksum."),
      "muted",
    ),
  );
  const download = model.download;
  let state = model.installed ? tr("pobrany", "downloaded") : tr("nie pobrany", "not downloaded");
  if (model.newer) state += tr(" - dostępny nowszy model", " - a newer model is available");
  if (download.state === "running") state = tr("pobieranie: ", "downloading: ") + megabytes(download.done) + tr(" z ", " of ") + megabytes(download.total);
  if (download.state === "failed") state = tr("pobieranie nie udało się: ", "the download failed: ") + download.reason;
  root.append(details([[tr("Model", "Model"), state]]));
  const buttons = el("div", null, "row");
  if (download.state !== "running" && (!model.installed || model.newer)) {
    buttons.append(
      button(tr("Pobierz model (", "Download the model (") + megabytes(model.size) + ")", async () => {
        await call("download_model");
        await renderRecording();
      }),
    );
  }
  if (model.installed && download.state !== "running") {
    buttons.append(
      button(tr("Usuń model", "Delete the model"), async () => {
        if (!confirm(tr("Usunąć model? Bez niego rozmowy nie będą zamieniane na tekst.", "Delete the model? Without it calls are not turned into text."))) return;
        await call("delete_model");
        await renderRecording();
      }, "danger"),
    );
  }
  root.append(buttons);
  renderRecordings(root, recording);
  // Progress while the section is open.
  const busy = recording.active || recording.recordings.some((item) => ["transcribing", "waiting", "recording"].includes(item.state));
  if (download.state === "running" || busy) {
    setTimeout(() => {
      if (current === "recording") renderRecording().catch(showError);
    }, 2000);
  }
}

function renderRecordingSettings(root, recording) {
  const auto = el("input");
  auto.type = "checkbox";
  auto.checked = recording.auto;
  const autoLabel = el("label", null, "row");
  autoLabel.append(auto, el("span", tr("Nagrywaj rozmowy Teams automatycznie", "Record Teams calls automatically")));
  root.append(
    autoLabel,
    el(
      "p",
      tr("Gdy jest włączone, każda rozmowa Teams jest nagrywana bez pytania: wszystko, co gra urządzenie rozmowy (także powiadomienia i inne aplikacje) przez cały czas rozmowy, oraz Twój mikrofon, tylko gdy jest włączony w Teams. Nagrania leżą w %LOCALAPPDATA%\\open_mess\\recordings na tym komputerze; dźwięk znika po transkrypcji, tekst zostaje, dopóki go nie usuniesz. Uczestników informujesz Ty.", "When on, every Teams call is recorded without asking: everything the call's device plays (notifications and other apps too) for the whole call, and your microphone only while it is on in Teams. Recordings stay in %LOCALAPPDATA%\\open_mess\\recordings on this computer; the audio goes once transcribed, the text stays until you delete it. Telling the participants is up to you."),
      "muted",
    ),
  );
  const microphone = el("input");
  microphone.type = "checkbox";
  microphone.checked = recording.microphone;
  const microphoneLabel = el("label", null, "row");
  microphoneLabel.append(microphone, el("span", tr("Nagrywaj mój mikrofon", "Record my microphone")));
  const languages = recordingLanguages();
  const known = languages.some(([code]) => code === recording.language);
  const language = select(known ? languages : [...languages, [recording.language, recording.language]], recording.language);
  const languageLabel = el("label", null, "row");
  languageLabel.append(el("span", tr("Język rozmów", "Language of the calls")), language);
  root.append(
    microphoneLabel,
    languageLabel,
    button(tr("Zapisz ustawienia", "Save settings"), async () => {
      if (auto.checked && !recording.auto && !confirm(tr("Nagrywać automatycznie każdą rozmowę Teams na tym komputerze?", "Record every Teams call on this computer automatically?"))) return;
      await call("set_recording_settings", { auto: auto.checked, microphone: microphone.checked, language: language.value });
      await renderRecording();
    }),
  );
  if (recording.active) {
    const row = el("div", null, "row");
    row.append(
      el("strong", tr("Trwa nagrywanie rozmowy.", "A call is being recorded.")),
      button(tr("Zatrzymaj", "Stop"), async () => {
        await call("stop_recording");
        await renderRecording();
      }),
    );
    root.append(row);
  }
  const status = recording.status || {};
  if (status.detect === "unavailable") root.append(el("p", tr("Nie można wykryć rozmów Teams.", "Teams calls cannot be detected."), "muted"));
  if (status.reason) root.append(el("p", tr("Czeka: ", "Waiting: ") + recordingReason(status.reason), "muted"));
}

// The calls whose captions are kept now, on top of the section (spec 2026-11-04-captions-archive, 4.7).
function renderCaptionsKept(root, recording) {
  const kept = recording.recordings.filter((item) => item.source === "captions" && item.state === "recording");
  if (kept.length === 0) return;
  const row = el("div", null, "row");
  row.append(
    el("strong", tr("Zapisuję napisy rozmów: ", "Keeping the captions of calls: ") + kept.length),
    button(tr("Zatrzymaj wszystkie", "Stop all"), async () => {
      await call("stop_captions", { id: null });
      await renderRecording();
    }),
  );
  root.append(row);
}

// The captions archive's switch and language (spec 2026-11-04-captions-archive, 4.1).
function renderCaptionsSettings(root, recording) {
  if (recording.captions === null) return;
  root.append(el("h2", tr("Napisy rozmów Teams", "Teams calls' captions")));
  const auto = el("input");
  auto.type = "checkbox";
  auto.checked = recording.captions_auto;
  const autoLabel = el("label", null, "row");
  autoLabel.append(auto, el("span", tr("Zapisuj napisy rozmów Teams automatycznie", "Keep the captions of Teams calls automatically")));
  const language = el("input");
  language.value = recording.captions_language;
  const languageLabel = el("label", null, "row");
  languageLabel.append(el("span", tr("Język napisów, gdy rozmowa nie ma żadnego", "Captions language when a call has none")), language);
  root.append(
    autoLabel,
    el(
      "p",
      tr("Gdy jest włączone, open_mess dołącza do każdej rozmowy Teams, na której jesteś, jako drugie urządzenie Twojego konta, bez dźwięku i obrazu; włącza napisy, gdy nikt ich nie ma; zapisuje tekst z imionami mówców na tym komputerze do czasu, aż go usuniesz. Uczestników informujesz Ty. Zatrzymanie zapisu kończy obecność open_mess w rozmowie.", "When on, open_mess joins every Teams call you are in as a second device of your account, without audio or video; turns captions on when no one has; keeps the text with the speakers' names on this computer until you delete it. Telling the participants is up to you. Stopping a call's captions ends open_mess's presence in it."),
      "muted",
    ),
    languageLabel,
    button(tr("Zapisz ustawienia napisów", "Save captions settings"), async () => {
      if (auto.checked && !recording.captions_auto && !confirm(tr("Zapisywać napisy każdej rozmowy Teams, na której jesteś?", "Keep the captions of every Teams call you are in?"))) return;
      await call("set_captions_settings", { auto: auto.checked, language: language.value });
      await renderRecording();
    }),
  );
  const status = recording.captions || {};
  if (status.reason) root.append(el("p", tr("Napisy czekają: ", "Captions wait: ") + recordingReason(status.reason), "muted"));
  if (status.state_lost) root.append(el("p", tr("Stan rozmów przepadł: rozmowy trwające przy starcie nie są zapisywane.", "The calls' state was lost: calls going on at the start are not kept."), "muted"));
  if (status.notices_failed) root.append(el("p", tr("Nieudane powiadomienia: ", "Notices that failed: ") + status.notices_failed, "muted"));
}

function renderRecordings(root, recording) {
  root.append(el("h2", tr("Nagrania", "Recordings")));
  if (recording.recordings.length === 0) {
    root.append(el("p", tr("Brak nagrań.", "No recordings."), "muted"));
    return;
  }
  const table = el("table");
  const head = el("tr");
  for (const label of [tr("Data", "Date"), tr("Długość", "Length"), tr("Źródło", "Source"), tr("Stan", "State"), ""]) head.append(el("th", label));
  table.append(head);
  const states = recordingStates();
  const sources = recordingSources();
  for (const item of recording.recordings) {
    const row = el("tr");
    const length = item.end_ms ? clock(item.end_ms - item.start_ms) : "";
    let state = states[item.state] || item.state;
    if (item.progress !== null && item.progress !== undefined) state += " " + item.progress + "%";
    if (item.coverage !== null && item.coverage !== undefined) state += tr(", pokrycie ", ", coverage ") + item.coverage + "%";
    if (item.reason) state += " (" + recordingReason(item.reason) + ")";
    const actions = el("td", null, "row");
    if (item.source === "captions") {
      actions.append(
        button(tr("Pokaż", "Show"), async () => {
          shownRecording = { id: item.id, captions: await call("show_captions", { id: item.id }) };
          await renderRecording();
        }),
      );
      if (item.state === "recording") {
        actions.append(
          button(tr("Zatrzymaj", "Stop"), async () => {
            await call("stop_captions", { id: item.id });
            await renderRecording();
          }),
        );
      }
    }
    if (item.source !== "captions" && item.state === "done") {
      actions.append(
        button(tr("Pokaż", "Show"), async () => {
          shownRecording = { id: item.id, cues: await call("show_transcript", { id: item.id }) };
          await renderRecording();
        }),
      );
    }
    if (item.state === "failed" && item.audio) {
      actions.append(
        button(tr("Ponów", "Retry"), async () => {
          await call("retry_recording", { id: item.id });
          await renderRecording();
        }),
      );
    }
    actions.append(
      button(
        tr("Usuń", "Delete"),
        async () => {
          if (!confirm(tr("Usunąć to nagranie i jego tekst?", "Delete this recording and its text?"))) return;
          await call("delete_recording", { id: item.id });
          if (shownRecording && shownRecording.id === item.id) shownRecording = null;
          await renderRecording();
        },
        "danger",
      ),
    );
    row.append(el("td", new Date(item.start_ms).toLocaleString(locale())), el("td", length), el("td", sources[item.source] || item.source), el("td", state), actions);
    table.append(row);
  }
  root.append(
    table,
    button(
      tr("Usuń wszystkie", "Delete all"),
      async () => {
        if (!confirm(tr("Usunąć wszystkie nagrania i ich teksty?", "Delete all recordings and their texts?"))) return;
        shownRecording = null;
        await call("delete_all_recordings");
        await renderRecording();
      },
      "danger",
    ),
  );
  if (shownRecording && recording.recordings.some((item) => item.id === shownRecording.id)) {
    root.append(el("h2", tr("Transkrypcja", "Transcript")));
    const text = el("div", null, "transcript");
    const names = speakers();
    for (const cue of shownRecording.cues || []) {
      text.append(el("p", "[" + clock(cue.start_ms) + "] " + (names[cue.speaker] || cue.speaker) + ": " + cue.text));
    }
    // A captions recording: the speakers' names as Teams gave them, and the times nothing was heard.
    const captions = shownRecording.captions;
    if (captions) {
      for (const gap of captions.gaps) {
        const to = gap.to === null ? tr("trwa", "lasting") : clock(gap.to);
        root.append(el("p", tr("Luka ", "Gap ") + clock(gap.from) + " - " + to + " (" + recordingReason(gap.reason) + ")", "muted"));
      }
      if (captions.damaged) root.append(el("p", tr("Uszkodzone linie: ", "Damaged lines: ") + captions.damaged, "muted"));
      for (const cue of captions.cues) text.append(el("p", "[" + clock(cue.at_ms) + "] " + cue.speaker + ": " + cue.text));
      if (captions.more) text.append(el("p", tr("Dalszą część czyta agent.", "An agent reads the rest."), "muted"));
    }
    root.append(text, button(tr("Zamknij", "Close"), async () => {
      shownRecording = null;
      await renderRecording();
    }));
  }
}

const RENDER = {
  accounts: renderAccounts,
  triggers: renderTriggers,
  permissions: renderPermissions,
  waiting: renderWaiting,
  modules: renderModules,
  recording: renderRecording,
  integrations: renderIntegrations,
  daemon: renderDaemon,
};

// Sent by the daemon whenever the waiting actions change.
window.addEventListener("pending-changed", () => {
  if (current === "waiting") renderWaiting().catch(showError);
  renderFooter().catch(showError);
});

// Sent by the daemon when the language changed (the setting, or Windows's): the page starts again in it.
window.addEventListener("language-changed", () => location.reload());

let current = null;

async function show(section) {
  // The token is shown only in the view where it was asked for.
  if (current === "integrations" && section !== "integrations") content("integrations").replaceChildren();
  current = section;
  clearError();
  for (const node of document.querySelectorAll("#menu button")) {
    node.classList.toggle("active", node.dataset.section === section);
  }
  for (const node of document.querySelectorAll("main section")) {
    node.hidden = node.id !== section;
  }
  try {
    if (RENDER[section]) await RENDER[section]();
    await renderFooter();
  } catch (err) {
    showError(err);
  }
}

/**
 * The language first, then the sections: none is drawn before the language is known. The first is
 * the actions waiting for approval, otherwise the accounts, unless the user picked one meanwhile.
 */
async function start() {
  let failed = null;
  try {
    const language = await call("language");
    lang = language.lang === "en" ? "en" : "pl";
    languageSetting = language.setting;
  } catch (err) {
    // The window works in Polish, as it did before it had a language, and says why.
    failed = err;
  }
  document.documentElement.lang = lang;
  if (lang === "en") translatePage();
  for (const node of document.querySelectorAll("#menu button")) {
    node.addEventListener("click", () => show(node.dataset.section));
  }
  // Asked for by the notice that several actions wait, on an open window.
  window.addEventListener("show-waiting", () => show("waiting"));
  const waiting = await call("waiting").catch(() => []);
  if (current === null) await show(waiting.length > 0 ? "waiting" : "accounts");
  renderSignIn().catch(showError);
  if (failed) showError(failed);
}

start().catch(showError);
