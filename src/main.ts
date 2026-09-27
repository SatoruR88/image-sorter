import { invoke, convertFileSrc } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";

interface SessionInfo {
  seq: number;
  root: string | null;
  categories: string[];
  index: number;
  remaining: number;
  current: string | null;
  upcoming: string[];
  undoable: number;
}

const $ = <T extends HTMLElement>(sel: string) => {
  const el = document.querySelector(sel) as T | null;
  if (!el) throw new Error(`missing element: ${sel}`);
  return el;
};

const rootPathEl = $("#root-path");
const progressEl = $("#progress");
const previewEl = $("#preview") as HTMLImageElement;
const messageEl = $("#message");
const brokenEl = $("#broken");
const brokenNameEl = $("#broken-name");
const startEl = $("#start");
const bottombarEl = $("#bottombar");
const categoriesEl = $("#categories");
const undoBtn = $("#undo") as HTMLButtonElement;
const deleteBtn = $("#delete") as HTMLButtonElement;
const addCategoryBtn = $("#add-category") as HTMLButtonElement;
const newCategoryWrap = $("#new-category");
const newCategoryInput = $("#new-category-name") as HTMLInputElement;
const toastEl = $("#toast");

let state: SessionInfo | null = null;
let toastTimer = 0;
let lastSeq = 0;

// Path the <img> is supposed to be showing, and the URL actually assigned —
// lets us ignore error events that belong to a src that was already replaced.
let previewPath: string | null = null;
let previewUrl = "";

// Retained preloads (≤2) — keeps in-flight fetches alive and dedupes refetches.
const preloadPool = new Map<string, HTMLImageElement>();

// Serialize backend operations so rapid keypresses execute in order.
let chain: Promise<void> = Promise.resolve();
function enqueue(op: () => Promise<SessionInfo>) {
  chain = chain.then(async () => {
    try {
      render(await op());
    } catch (e) {
      toast(String(e));
    }
  });
}

function toast(msg: string) {
  toastEl.textContent = msg;
  toastEl.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (toastEl.hidden = true), 3000);
}

function render(info: SessionInfo) {
  // Drop snapshots that predate what we already rendered (watcher emissions
  // race with invoke responses on separate IPC channels).
  if (info.seq <= lastSeq) return;
  lastSeq = info.seq;
  state = info;
  const hasFolder = info.root !== null;

  rootPathEl.textContent = info.root ?? "";
  progressEl.textContent = hasFolder
    ? info.remaining > 0
      ? `${info.index + 1} / ${info.remaining}`
      : "done"
    : "";

  startEl.hidden = hasFolder;
  bottombarEl.hidden = !hasFolder;

  // Categories
  categoriesEl.replaceChildren();
  info.categories.forEach((name, i) => {
    const btn = document.createElement("button");
    btn.className = "cat";
    if (i < 9) {
      const key = document.createElement("span");
      key.className = "key";
      key.textContent = String(i + 1);
      btn.append(key);
    }
    const label = document.createElement("span");
    label.className = "name";
    label.textContent = name;
    btn.append(label);
    btn.title = name;
    btn.addEventListener("click", () => classify(i));
    categoriesEl.append(btn);
  });

  undoBtn.disabled = info.undoable === 0;

  // Image — only touch <img> when the displayed path actually changed, and
  // keep the broken panel shown while the same broken path stays current.
  if (info.current !== previewPath) {
    previewPath = info.current;
    previewUrl = info.current ? convertFileSrc(info.current) : "";
    brokenEl.hidden = true;
    if (info.current) {
      previewEl.hidden = false;
      previewEl.src = previewUrl;
    } else {
      previewEl.hidden = true;
      previewEl.removeAttribute("src");
    }
  }
  if (info.current) {
    messageEl.hidden = true;
  } else {
    messageEl.hidden = !hasFolder;
    if (hasFolder) messageEl.textContent = "No unclassified images left.";
  }

  // Bounded preload of upcoming images.
  const wanted = new Set(info.upcoming);
  for (const p of [...preloadPool.keys()]) {
    if (!wanted.has(p)) preloadPool.delete(p);
  }
  for (const p of info.upcoming) {
    if (!preloadPool.has(p)) {
      const img = new Image();
      img.src = convertFileSrc(p);
      preloadPool.set(p, img);
    }
  }
}

previewEl.addEventListener("error", () => {
  // Ignore errors from a src that was already replaced by a newer render.
  if (!previewUrl || previewEl.src !== previewUrl || previewEl.hidden) return;
  previewEl.hidden = true;
  brokenNameEl.textContent = previewPath ?? "";
  brokenEl.hidden = false;
});

function classify(i: number) {
  if (!state || i >= state.categories.length || !state.current) return;
  const category = state.categories[i];
  const expected = state.current;
  enqueue(() => invoke("classify", { category, expected }));
}

let picking = false;
async function pickFolder() {
  if (picking) return;
  picking = true;
  try {
    const selected = await open({ directory: true });
    if (typeof selected === "string") {
      enqueue(() => invoke("open_folder", { path: selected }));
    }
  } catch (e) {
    toast(String(e));
  } finally {
    picking = false;
  }
}

function closeCategoryInput() {
  newCategoryWrap.hidden = true;
  newCategoryInput.value = "";
  newCategoryInput.blur();
}

function submitNewCategory() {
  const name = newCategoryInput.value.trim();
  closeCategoryInput();
  if (name) enqueue(() => invoke("create_category", { name }));
}

function deleteCurrent() {
  if (!state?.current) return;
  const expected = state.current;
  enqueue(() => invoke("delete_current", { expected }));
}

window.addEventListener("keydown", (e) => {
  // Escape closes the category input wherever focus is.
  if (e.key === "Escape" && !newCategoryWrap.hidden) {
    closeCategoryInput();
    return;
  }
  if (e.ctrlKey && e.key.toLowerCase() === "z") {
    e.preventDefault();
    if (state?.undoable) enqueue(() => invoke("undo"));
    return;
  }
  if (e.ctrlKey || e.altKey || e.metaKey) return;

  if (e.key >= "1" && e.key <= "9") {
    if (!e.repeat) classify(Number(e.key) - 1);
  } else if (e.key === "Delete") {
    if (!e.repeat) deleteCurrent();
  } else if (e.key === "ArrowRight") {
    enqueue(() => invoke("navigate", { delta: 1 }));
  } else if (e.key === "ArrowLeft") {
    enqueue(() => invoke("navigate", { delta: -1 }));
  }
});

$("#open-folder").addEventListener("click", pickFolder);
undoBtn.addEventListener("click", () => enqueue(() => invoke("undo")));
deleteBtn.addEventListener("click", deleteCurrent);
addCategoryBtn.addEventListener("click", () => {
  newCategoryWrap.hidden = !newCategoryWrap.hidden;
  if (!newCategoryWrap.hidden) {
    newCategoryInput.value = "";
    newCategoryInput.focus();
  }
});
newCategoryInput.addEventListener("keydown", (e) => {
  e.stopPropagation();
  if (e.key === "Enter") submitNewCategory();
  if (e.key === "Escape") closeCategoryInput();
});

// Filesystem watcher pushes updated sessions (e.g. folder added externally).
listen<SessionInfo>("session", (e) => render(e.payload)).catch(() => {});

enqueue(() => invoke("get_state"));
