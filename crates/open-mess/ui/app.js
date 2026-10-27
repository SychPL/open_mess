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

async function renderAccounts() {
  const accounts = await call("accounts");
  const root = content("accounts");
  root.replaceChildren();
  const add = el("button", "Dodaj konto", "action primary");
  add.disabled = true;
  add.title = "Logowanie kont pojawi się w następnej wersji";
  root.append(add);
  if (accounts.length === 0) root.append(el("p", "Nie ma jeszcze żadnego konta.", "muted"));
  for (const account of accounts) {
    const card = el("div", null, "card row");
    const who = el("div", null, "grow");
    who.append(el("strong", account.display_name), el("div", (account.platform || account.module) + " - " + account.id, "muted"));
    card.append(who, stateDot(account.state));
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
      for (const [label, command] of [["Claude Code", commands.claude], ["Codex", commands.codex]]) {
        shown.append(el("h2", label), el("pre", command, "content-text"), button("Kopiuj", () => navigator.clipboard.writeText(command)));
      }
      shown.append(el("p", "setx zapisuje token dla nowych okien terminala: uruchom Codex w nowym oknie.", "muted"));
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
 * on every change; a change that fails is put back.
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
    try {
      await call("set_permissions", { account, conversation, read: box.checked, write: write.value });
      current = { read: box.checked, write: write.value };
    } catch (err) {
      box.checked = current.read;
      write.value = current.write;
      showError(err);
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
        // A new conversation starts with the account's permissions; change them in its row.
        await call("set_permissions", { account: account.id, conversation: id.value, read: permissions.read, write: permissions.write });
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
  row.append(button("Zatwierdź", decide(true), "primary"), button("Odrzuć", decide(false), "danger"));
  card.append(row);
  return card;
}

async function renderWaiting() {
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

const RENDER = {
  accounts: renderAccounts,
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
