"use strict";

// The configuration window. Talks to the daemon only through its own Tauri commands. Text that may
// come from other people or from modules (names, titles, messages, manifests) goes into the page
// only through `el`, which sets textContent: never innerHTML.

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
  node.textContent = "Błąd: " + String(err);
  node.hidden = false;
}

function clearError() {
  document.getElementById("error").hidden = true;
}

function content(section) {
  return document.querySelector("#" + section + " .content");
}

const ACCOUNT_STATES = {
  online: ["połączone", "ok"],
  offline: ["rozłączone", "bad"],
  "auth-required": ["wymaga ponownego logowania", "warn"],
  removing: ["usuwanie", "warn"],
};

function stateDot(state) {
  const [label, kind] = ACCOUNT_STATES[state] || [state, ""];
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
    top.append(el("span", "Trwa logowanie w module " + signingIn + "...", "muted"));
  } else if (modules.length === 0) {
    top.append(el("span", "Żeby dodać konto, zatwierdź i włącz moduł w sekcji Moduły.", "muted"));
  } else {
    const pick = el("select");
    for (const module of modules) {
      const option = el("option", module.name + (module.platform ? " (" + module.platform + ")" : ""));
      option.value = module.name;
      pick.append(option);
    }
    top.append(button("Dodaj konto", () => signIn(pick.value), "primary"));
    if (modules.length > 1) top.append(pick);
  }
  root.append(top);
  if (accounts.length === 0) root.append(el("p", "Nie ma jeszcze żadnego konta.", "muted"));
  for (const account of accounts) {
    const card = el("div", null, "card row");
    const who = el("div", null, "grow");
    who.append(el("strong", account.display_name), el("div", (account.platform || account.module) + " - " + account.id, "muted"));
    card.append(who, stateDot(account.state));
    if (!signingIn && account.state !== "removing" && modules.some((module) => module.name === account.module)) {
      card.append(button("Zaloguj ponownie", () => signIn(account.module, account.id)));
    }
    // Not while a sign-in runs: it might be signing in this very account.
    if (!signingIn) {
      card.append(
        button("Usuń", async () => {
          const sure = confirm(
            "Usunąć konto " + account.display_name + "?\n\nZnikną jego sekrety, uprawnienia, akcje czekające na zatwierdzenie i pobrane załączniki.",
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
  if (!items || items.length === 0) return el("span", empty || "brak", "muted");
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
    button("Odśwież folder modułów", async () => {
      await call("rescan_modules");
      await renderModules();
    }),
    el("span", "Moduły to pliki .wasm w " + status.data_dir + "\\modules", "muted"),
  );
  root.append(top);
  root.append(
    el(
      "p",
      "Zatwierdzenie modułu to zaufanie do jego kodu, jak instalacja aplikacji: moduł z kontem i dostępem do hostów platformy może zrobić na tym koncie wszystko. Przy aktywnym proxy systemowym ochrona sieci lokalnej zależy od proxy.",
      "muted",
    ),
  );
  if (modules.length === 0) root.append(el("p", "W folderze nie ma modułów.", "muted"));
  for (const module of modules) {
    const card = el("div", null, "card");
    const head = el("div", null, "row");
    const title = el("div", null, "grow");
    title.append(el("strong", module.name + (module.version ? " " + module.version : "")), el("div", module.detail, "muted"));
    head.append(title);
    if (module.status === "awaiting-approval" || module.status === "replacement") {
      head.append(
        button("Zatwierdź", async () => {
          let question = "Zatwierdzić moduł " + module.name + " z pliku " + module.file + "?";
          if (module.takes_over) question += "\n\nNowy plik przejmie konta i sekrety poprzedniej wersji.";
          if (!confirm(question)) return;
          await call("approve_module", { name: module.name, file_sha256: module.file_sha256, manifest_sha256: module.manifest_sha256 });
          await renderModules();
        }, "primary"),
      );
    }
    if (module.status === "active") {
      head.append(
        button("Wyłącz", async () => {
          await call("set_module_enabled", { name: module.name, enabled: false });
          await renderModules();
        }),
      );
    }
    if (module.status === "disabled" || module.status === "crashed") {
      head.append(
        button(module.status === "disabled" ? "Włącz" : "Uruchom ponownie", async () => {
          await call("set_module_enabled", { name: module.name, enabled: true });
          await renderModules();
        }),
      );
    }
    card.append(head);
    if (module.takes_over) {
      card.append(el("p", "Po zatwierdzeniu ten plik przejmie konta i sekrety poprzedniej wersji modułu.", "error"));
    }
    if (module.changes.length > 0) {
      card.append(el("h2", "Zmiany względem działającej wersji"), list(module.changes));
    }
    const wam = module.wam ? module.wam.client_id + " (" + module.wam.scopes.join(", ") + ")" : "brak";
    const tools = module.tools.map((tool) => tool.name + (tool.writes ? " (pisze)" : " (czyta)"));
    card.append(
      details([
        ["Plik", module.file],
        ["Platforma", module.platform || "-"],
        ["Hosty", list(module.hosts)],
        ["Hosty logowania", list(module.sign_in_hosts)],
        ["Konto Windows (WAM)", wam],
        ["Własne narzędzia", list(tools)],
        ["SHA-256 pliku", el("code", module.file_sha256 || "-")],
        ["SHA-256 manifestu", el("code", module.manifest_sha256 || "-")],
      ]),
    );
    root.append(card);
  }
}

async function renderIntegrations() {
  const status = await call("status");
  const root = content("integrations");
  root.replaceChildren();
  root.append(el("h2", "Claude Code i Codex (MCP)"));
  root.append(el("p", "Polecenia zawierają token lokalnego API: każdy, kto go zna, może używać open_mess jak agent.", "muted"));
  const shown = el("div");
  shown.append(
    button("Pokaż polecenie", async () => {
      const commands = await call("mcp_command");
      shown.replaceChildren();
      for (const [label, command] of [["Claude Code", commands.claude], ["Codex (do pliku %USERPROFILE%\\.codex\\config.toml)", commands.codex]]) {
        shown.append(el("h2", label), el("pre", command, "content-text"), button("Kopiuj", () => navigator.clipboard.writeText(command)));
      }
      shown.append(
        el(
          "p",
          "Codex dostaje token z pliku config.toml, tak jak Claude Code ze swojej konfiguracji. Nie zapisuj go w zmiennych środowiskowych (setx): dziedziczy je każdy uruchomiony program, także powłoki agentów.",
          "muted",
        ),
      );
    }),
  );
  root.append(shown);
  root.append(el("h2", "Wiersz poleceń"));
  root.append(details([["Ścieżka do om", el("code", status.cli)], ["Adres MCP", el("code", "http://127.0.0.1:" + status.port + "/mcp")]]));
  root.append(el("p", "Bez tokenu w zmiennej OPEN_MESS_TOKEN polecenie om czyta go samo z Menedżera poświadczeń.", "muted"));
}

function duration(seconds) {
  const h = Math.floor(seconds / 3600);
  const m = Math.floor((seconds % 3600) / 60);
  return (h > 0 ? h + " h " : "") + m + " min";
}

async function renderDaemon() {
  const status = await call("status");
  const root = content("daemon");
  root.replaceChildren();
  root.append(
    details([
      ["Wersja", status.version],
      ["Działa od", duration(status.uptime_seconds)],
      ["Port lokalnego API", status.port],
      ["Folder danych", el("code", status.data_dir)],
      ["Konta", status.accounts_online + " z " + status.accounts + " połączonych"],
      ["Moduły", status.modules_active + " z " + status.modules + " działa"],
      ["Akcje do zatwierdzenia", status.waiting],
    ]),
  );
  root.append(el("h2", "Ostatnie błędy"));
  root.append(list(status.problems, "Brak błędów modułów. (Dziennik demona pojawi się w następnej wersji.)"));
}

async function renderFooter() {
  const status = await call("status");
  document.getElementById("daemon-state").textContent =
    "Demon działa - port " + status.port + "\n" + status.accounts_online + "/" + status.accounts + " kont połączonych";
  const badge = document.getElementById("waiting-count");
  badge.textContent = String(status.waiting);
  badge.hidden = status.waiting === 0;
}

const WRITE_OPTIONS = [
  ["never", "nigdy"],
  ["confirm", "z potwierdzeniem"],
  ["autonomous", "samodzielnie"],
];

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
  read.append(box, el("span", "czytanie"));
  const write = el("select");
  for (const [value, label] of WRITE_OPTIONS) {
    const option = el("option", label);
    option.value = value;
    write.append(option);
  }
  write.value = current.write;
  const save = async () => {
    if (write.value === "autonomous" && current.write !== "autonomous") {
      const sure = confirm("Samodzielnie: agent wyśle, poprawi i zareaguje bez pytania. Ustawić?");
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
  writeLabel.append(el("span", "pisanie:"), write);
  return [read, writeLabel];
}

async function renderPermissions() {
  const accounts = await call("accounts");
  const root = content("permissions");
  root.replaceChildren();
  root.append(
    el(
      "p",
      "Te same uprawnienia dotyczą każdego agenta i polecenia om. Ustawienie rozmowy ma pierwszeństwo przed ustawieniem konta. Z potwierdzeniem: każde wysłanie, edycja i reakcja czeka na Twoją decyzję w powiadomieniu albo w sekcji Do zatwierdzenia.",
      "muted",
    ),
  );
  if (accounts.length === 0) root.append(el("p", "Nie ma jeszcze żadnego konta.", "muted"));
  for (const account of accounts) {
    const permissions = await call("permissions", { account: account.id });
    const card = el("div", null, "card");
    card.append(el("strong", account.display_name), el("div", account.id, "muted"));
    const own = el("div", null, "row");
    own.append(el("span", "Całe konto:"), ...permissionControls(account.id, null, permissions));
    card.append(own, el("h2", "Rozmowy z własnymi uprawnieniami"));
    const table = el("table");
    for (const item of permissions.overrides) {
      const row = el("tr");
      const id = el("td");
      id.append(el("code", item.conversation));
      const controls = el("td", null, "row");
      controls.append(...permissionControls(account.id, item.conversation, item));
      const remove = el("td");
      remove.append(
        button("Usuń", async () => {
          await call("clear_permission_override", { account: account.id, conversation: item.conversation });
          await renderPermissions();
        }, "danger"),
      );
      row.append(id, controls, remove);
      table.append(row);
    }
    if (permissions.overrides.length === 0) card.append(el("p", "Brak - wszystkie rozmowy mają ustawienia konta.", "muted"));
    else card.append(table);
    const add = el("div", null, "row");
    const id = el("input");
    id.type = "text";
    id.placeholder = "identyfikator rozmowy";
    add.append(
      id,
      button("Dodaj rozmowę", async () => {
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
}

const KINDS = { send: "Wysłanie", edit: "Edycja", react: "Reakcja" };

const OUTCOMES = {
  done: "Wykonano.",
  rejected: "Odrzucono.",
  expired: "Wygasło - nic nie wysłano.",
  voided: "Uczestnicy albo wiadomość zmienili się od pokazania - nic nie wysłano.",
  uncertain: "Mogło dotrzeć - sprawdź rozmowę, zanim spróbujesz ponownie.",
  failed: "Nie udało się - nic nie wysłano.",
};

/** The cards on screen by action, so a new or decided action leaves the others (and where they are scrolled) alone. */
const waitingCards = new Map();

/** One waiting action with everything it would do, in full; long content scrolls. */
function waitingCard(preview) {
  const card = el("div", null, "card");
  card.append(el("strong", KINDS[preview.kind] || preview.kind));
  const participants = preview.participants.length > 0 ? preview.participants.join(", ") : "brak (kanał)";
  card.append(details([["Konto", preview.account], ["Rozmowa", preview.conversation], ["Uczestnicy", participants]]));
  const block = (label, text) => card.append(el("h2", label), el("div", text, "content-text"));
  if (preview.kind === "send") {
    block("Treść", preview.text);
    if (preview.target_text !== null) block("W odpowiedzi na", preview.target_text);
  } else if (preview.kind === "edit") {
    block("Przed", preview.target_text);
    block("Po", preview.text);
  } else {
    block("Reakcja", preview.reaction);
    block("Pod wiadomością", preview.target_text);
  }
  const decide = (approve) => async () => {
    const outcome = await call("decide", { action: preview.action, approve });
    document.getElementById("waiting-outcome").textContent = OUTCOMES[outcome] || outcome;
    await renderWaiting();
  };
  const row = el("div", null, "row");
  const approve = button("Zatwierdź", decide(true), "primary");
  if (preview.target_cut) {
    // Nothing is approved that is not on screen in full.
    approve.disabled = true;
    card.append(el("p", "Wiadomość, której dotyczy akcja, jest za długa, by pokazać ją w całości - akcję można tylko odrzucić.", "error"));
  }
  row.append(approve, button("Odrzuć", decide(false), "danger"));
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
    root.append(outcome, el("p", "Nic nie czeka na zatwierdzenie.", "muted empty"));
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
  dialog.append(el("h1", "Logowanie - " + prompt.module));
  if (prompt.kind === "device-code") {
    dialog.append(el("p", "Otwórz stronę logowania w przeglądarce i wpisz ten kod:"), el("div", prompt.code, "code"), el("p", prompt.url, "muted"));
    dialog.append(button("Otwórz stronę w przeglądarce", () => call("sign_in_open_link", { request_id: prompt.id })));
  } else if (prompt.kind === "qr") {
    dialog.append(el("p", "Zeskanuj ten kod telefonem:"));
    if (prompt.qr_svg) {
      const image = el("img");
      image.alt = "kod QR";
      // An image, so nothing in it runs.
      image.src = "data:image/svg+xml;charset=utf-8," + encodeURIComponent(prompt.qr_svg);
      dialog.append(image);
    } else {
      dialog.append(el("p", "Kod jest za długi, żeby go pokazać.", "error"));
    }
  } else if (prompt.kind === "page") {
    dialog.append(el("p", "Zaloguj się w osobnym oknie strony logowania (" + prompt.url + "). Hasło wpisujesz tylko tam."));
  } else if (prompt.kind === "ask-host") {
    dialog.append(el("p", "Strona logowania przechodzi na " + prompt.url + ". Zezwolić?"));
    dialog.append(
      el("p", "Zgoda doda ten host do zatwierdzonych hostów logowania modułu (np. firmowy serwer logowania). Odmowa przerwie logowanie.", "muted"),
    );
    const row = el("div", null, "row");
    row.append(
      button("Zezwól", () => call("sign_in_answer", { request_id: prompt.id, reply: { done: "allow" } }), "primary"),
      button("Nie zezwalaj", () => call("sign_in_answer", { request_id: prompt.id, reply: "cancelled" }), "danger"),
    );
    dialog.append(row);
    return;
  } else if (prompt.kind === "wam-pick") {
    const answer = (done) => () => call("sign_in_answer", { request_id: prompt.id, reply: { done } });
    dialog.append(el("p", "Wybierz konto Windows, którym moduł ma się zalogować:"));
    if (prompt.accounts.length === 0) dialog.append(el("p", "Windows nie zna jeszcze żadnego konta służbowego dla tego modułu.", "muted"));
    // The same name in several organizations (as a guest, say) shows the organization's id.
    const total = new Map();
    for (const account of prompt.accounts) total.set(account.user_name, (total.get(account.user_name) || 0) + 1);
    for (const account of prompt.accounts) {
      const label = account.user_name + (total.get(account.user_name) > 1 ? " (organizacja " + (account.tenant || "?") + ")" : "");
      dialog.append(button(label, answer(account.id), "choice"));
    }
    dialog.append(button("Inne konto Windows...", answer(null), "choice"));
  }
  const row = el("div", null, "row");
  row.append(button("Anuluj", () => call("sign_in_answer", { request_id: prompt.id, reply: "cancelled" }), "danger"));
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

const CONDITIONS = [
  ["direct", "wiadomość prywatna (DM)"],
  ["mention", "wzmianka o mnie"],
  ["conversation", "nowa wiadomość w wybranej rozmowie"],
  ["from", "wiadomość od wybranych osób"],
];

const DROPS = [
  ["dropped_rate", "ponad limit na godzinę"],
  ["dropped_quiet_hours", "w ciszy nocnej"],
  ["dropped_queue", "wypchnięte z kolejki"],
  ["dropped_no_session", "bez sesji i programu"],
];

function conditionText(condition) {
  const [, label] = CONDITIONS.find(([type]) => type === condition.type) || [condition.type, condition.type];
  if (condition.type === "conversation") return label + ": " + condition.id;
  if (condition.type === "from") return label + ": " + condition.senders.join(", ");
  return label;
}

function spawnText(spawn) {
  return [spawn.program, ...spawn.args].join(" ") + (spawn.working_dir ? " (w " + spawn.working_dir + ")" : "");
}

function actionText(action) {
  if (action.type === "spawn") return "uruchom program: " + spawnText(action);
  const fallback = action.fallback_spawn ? "; bez sesji uruchom: " + spawnText(action.fallback_spawn) : "";
  return "wpis do otwartej sesji Claude Code (kanał)" + fallback;
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
  program.placeholder = "wybierz program";
  const pick = button("Wybierz program...", async () => {
    const picked = await call("pick_program");
    if (picked) program.value = picked;
  });
  const args = el("textarea");
  args.rows = 3;
  args.value = spawn ? spawn.args.join("\n") : "";
  const dir = input("text", spawn ? spawn.working_dir : "");
  dir.placeholder = "pełna ścieżka, opcjonalnie";
  const node = el("div");
  const programRow = el("div", null, "row");
  const clear = button("Bez programu", () => {
    program.value = "";
  });
  programRow.append(el("span", "Program:"), program, pick, clear);
  node.append(programRow, field("Argumenty (stałe, po jednym w wierszu):", args), field("Folder roboczy:", dir));
  node.append(el("p", "Treść wiadomości nigdy nie trafia do argumentów: program dostaje ją na wejściu jako dane niezaufane.", "muted"));
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
  card.append(el("h2", existing ? "Zmień regułę" : "Nowa reguła"));
  const account = select(accounts.map((item) => [item.id, item.display_name + " (" + item.id + ")"]), rule.account);
  const condition = select(CONDITIONS, rule.condition.type);
  const conditionValue = input("text", rule.condition.id || (rule.condition.senders || []).join(", "));
  const conditionRow = field("", conditionValue);
  const showCondition = () => {
    conditionRow.hidden = condition.value !== "conversation" && condition.value !== "from";
    conditionRow.firstChild.textContent =
      condition.value === "conversation" ? "Identyfikator rozmowy:" : "Identyfikatory osób (po przecinku):";
  };
  condition.addEventListener("change", showCondition);
  showCondition();

  const action = select([["channel", "wpis do otwartej sesji Claude Code (kanał)"], ["spawn", "uruchom program"]], rule.action.type);
  const spawn = spawnFields(rule.action.type === "spawn" ? rule.action : rule.action.fallback_spawn);
  const fallbackNote = el("p", "Gdy nie ma otwartej sesji z kanałami, reguła uruchomi program poniżej (zostaw pusty, żeby nic nie robić).", "muted");
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
  quietRow.append(quiet, el("span", "Cisza nocna od"), quietFrom, el("span", "do"), quietTo);
  const perHour = input("number", rule.max_per_hour);
  const quietSeconds = input("number", rule.quiet_ms / 1000);
  const delaySeconds = input("number", rule.max_delay_ms / 1000);
  const enabled = input("checkbox");
  enabled.checked = rule.enabled;

  card.append(
    field("Konto:", account),
    field("Gdy przyjdzie:", condition),
    conditionRow,
    field("Akcja:", action),
    fallbackNote,
    spawn.node,
    quietRow,
    field("Najwyżej pobudek na godzinę:", perHour),
    field("Wyślij po tylu sekundach ciszy w rozmowie:", quietSeconds),
    field("...ale najpóźniej po tylu sekundach:", delaySeconds),
    field("Włączona:", enabled),
  );
  const row = el("div", null, "row");
  row.append(
    button("Zapisz", async () => {
      if (quiet.checked && (!quietFrom.value || !quietTo.value)) throw "podaj obie godziny ciszy nocnej";
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
    button("Anuluj", done),
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
      "Reguła budzi agenta, gdy ktoś napisze do Ciebie: wpisem do otwartej sesji Claude Code albo uruchomieniem programu. Twoje własne wiadomości nigdy jej nie uruchamiają, a rozmowy, których agent nie może czytać, są pomijane. Treść od innych osób agent dostaje jako dane niezaufane.",
      "muted",
    ),
  );
  const editor = el("div");
  const close = async () => renderTriggers();
  if (accounts.length === 0) {
    root.append(el("p", "Reguły należą do kont: najpierw dodaj konto.", "muted"));
  } else {
    root.append(button("Dodaj regułę", () => editor.replaceChildren(ruleForm(accounts, null, close)), "primary"));
  }
  root.append(editor);
  if (rules.length === 0) root.append(el("p", "Nie ma jeszcze żadnej reguły.", "muted"));
  for (const rule of rules) {
    const card = el("div", null, "card");
    const head = el("div", null, "row");
    const title = el("div", null, "grow");
    const owner = accounts.find((account) => account.id === rule.account);
    title.append(
      el("strong", conditionText(rule.condition)),
      el("div", (owner ? owner.display_name + " - " : "") + rule.account + (rule.enabled ? "" : " (wyłączona)"), "muted"),
    );
    head.append(
      title,
      button("Zmień", () => editor.replaceChildren(ruleForm(accounts, rule, close))),
      button("Usuń", async () => {
        if (!confirm("Usunąć regułę?")) return;
        await call("delete_rule", { id: rule.id });
        await renderTriggers();
      }, "danger"),
    );
    card.append(head);
    const quiet = rule.quiet_hours ? rule.quiet_hours[0].slice(0, 5) + "-" + rule.quiet_hours[1].slice(0, 5) : "brak";
    const counters = rule.counters;
    const dropped = DROPS.filter(([key]) => counters[key] > 0).map(([key, label]) => label + ": " + counters[key]);
    card.append(
      details([
        ["Akcja", actionText(rule.action)],
        ["Cisza nocna", quiet],
        ["Limity", rule.max_per_hour + " na godzinę; po " + rule.quiet_ms / 1000 + " s ciszy, najpóźniej po " + rule.max_delay_ms / 1000 + " s"],
        ["Od startu demona", "dostarczone: " + counters.delivered + (dropped.length ? "; pominięte - " + dropped.join(", ") : "")],
      ]),
    );
    if (counters.failed_to_start > 0) {
      card.append(
        el("p", "Program nie uruchomił się " + counters.failed_to_start + " raz(y) - sprawdź, czy nadal jest pod tą ścieżką, i wybierz go ponownie.", "error"),
      );
    }
    root.append(card);
  }

  root.append(el("h2", "Sesja Claude Code z kanałami"));
  root.append(
    el(
      "p",
      "Wpis do sesji działa w sesjach Claude Code uruchomionych z kanałem open_mess. Polecenie om mcp-stdio należy tylko do takich sesji: nie dodawaj go do zwykłej konfiguracji MCP (tam open_mess działa przez adres z sekcji Integracje).",
      "muted",
    ),
  );
  const setup = el("div");
  setup.append(
    button("Pokaż konfigurację kanału", async () => {
      const channel = await call("channel_setup");
      setup.replaceChildren(
        el("p", "Konfiguracja MCP (zapisana w " + channel.file + "):", "muted"),
        el("pre", channel.json, "content-text"),
        el("p", "Uruchom Claude Code tak:", "muted"),
        el("pre", channel.command, "content-text"),
        button("Kopiuj polecenie", () => navigator.clipboard.writeText(channel.command)),
        el("p", "Otwarte sesje z kanałem: " + channel.sessions + ".", "muted"),
      );
    }),
  );
  root.append(setup);
}

const RENDER = {
  accounts: renderAccounts,
  triggers: renderTriggers,
  permissions: renderPermissions,
  waiting: renderWaiting,
  modules: renderModules,
  integrations: renderIntegrations,
  daemon: renderDaemon,
};

// Sent by the daemon whenever the waiting actions change.
window.addEventListener("pending-changed", () => {
  if (current === "waiting") renderWaiting().catch(showError);
  renderFooter().catch(showError);
});

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

for (const node of document.querySelectorAll("#menu button")) {
  node.addEventListener("click", () => show(node.dataset.section));
}

show("accounts");
renderSignIn().catch(showError);
