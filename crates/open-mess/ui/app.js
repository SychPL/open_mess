"use strict";

// The configuration window. Talks to the daemon only through its own Tauri commands. Text that may
// come from other people (names, titles, messages) goes into the page only through `el`, which sets
// textContent: never innerHTML.

/** A new element with `text` as its text (never parsed as HTML) and the given class. */
function el(tag, text, className) {
  const node = document.createElement(tag);
  if (text !== undefined && text !== null) node.textContent = String(text);
  if (className) node.className = className;
  return node;
}

function show(section) {
  for (const button of document.querySelectorAll("#menu button")) {
    button.classList.toggle("active", button.dataset.section === section);
  }
  for (const node of document.querySelectorAll("main section")) {
    node.hidden = node.id !== section;
  }
}

for (const button of document.querySelectorAll("#menu button")) {
  button.addEventListener("click", () => show(button.dataset.section));
}

show("accounts");
