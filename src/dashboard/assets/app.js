"use strict";

const $ = (selector, root = document) => root.querySelector(selector);

/** Creates an element; strings become text nodes, so data never becomes markup. */
function el(tag, attrs = {}, ...children) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(attrs)) {
    if (value === undefined || value === null || value === false) continue;
    if (key === "class") node.className = value;
    else if (key.startsWith("on")) node.addEventListener(key.slice(2), value);
    else node.setAttribute(key, value === true ? "" : value);
  }
  for (const child of children.flat()) {
    if (child !== null && child !== undefined && child !== false) node.append(child);
  }
  return node;
}

async function api(path, options = {}) {
  const response = await fetch(path, {
    ...options,
    headers: { "content-type": "application/json", ...(options.headers || {}) },
  });
  if (response.status === 401) {
    location.href = "login";
    throw new Error("Signed out");
  }
  const body = await response.json().catch(() => ({}));
  if (!response.ok) throw new Error(body.error || `HTTP ${response.status}`);
  return body;
}

/** Every setting the server understands; each provider shows the ones it supports. */
const SETTINGS = {
  n: { label: "Images", type: "number", min: 1, max: 10, placeholder: "1" },
  size: { label: "Size", suggestions: ["auto", "1024x1024", "1536x1024", "1024x1536"], placeholder: "auto" },
  aspect_ratio: { label: "Aspect ratio", suggestions: ["1:1", "16:9", "9:16", "4:3", "3:4", "3:2", "2:3", "21:9"], placeholder: "auto" },
  resolution: { label: "Resolution", options: ["1k", "2k", "4k"] },
  quality: { label: "Quality", options: ["low", "medium", "high", "auto"] },
  background: { label: "Background", options: ["auto", "opaque", "transparent"] },
  output_format: { label: "Format", options: ["png", "jpeg", "webp"] },
  output_compression: { label: "Compression", type: "number", min: 0, max: 100, placeholder: "0-100" },
  seed: { label: "Seed", type: "number", min: 0, placeholder: "random" },
  negative_prompt: { label: "Negative prompt", wide: true, placeholder: "text, watermark" },
};
const NUMERIC = new Set(["n", "output_compression", "seed"]);

const state = {
  providers: [],
  inputs: [],
  polls: new Map(),
};

/* Views */

function route() {
  const view = location.hash === "#providers" ? "providers" : "create";
  $("#view-create").hidden = view !== "create";
  $("#view-providers").hidden = view !== "providers";
  for (const link of document.querySelectorAll("nav a")) {
    if (link.dataset.view === view) link.setAttribute("aria-current", "page");
    else link.removeAttribute("aria-current");
  }
}

/* Providers */

async function loadProviders() {
  const data = await api("api/providers");
  state.providers = data.providers;
  $("#storage").textContent = data.storage;
  $("#sign-out").hidden = !data.sign_out;
  renderProviderSelect();
  renderProviderList();
}

function selectedProvider() {
  return state.providers.find((p) => p.name === $("#generate").provider.value);
}

function renderProviderSelect() {
  const select = $("#generate").provider;
  const previous = select.value;
  select.replaceChildren(
    ...state.providers.map((p) =>
      el("option", { value: p.name, disabled: !p.ready }, p.ready ? p.name : `${p.name} (not connected)`),
    ),
  );
  const keep = state.providers.find((p) => p.name === previous && p.ready);
  const fallback = state.providers.find((p) => p.default) || state.providers.find((p) => p.ready);
  if (keep || fallback) select.value = (keep || fallback).name;

  const anyReady = state.providers.some((p) => p.ready);
  $("#submit").disabled = !anyReady;
  if (anyReady) {
    hideNote();
  } else {
    showNote("No provider is connected yet. Connect one under Providers.");
  }
  renderProviderOptions();
}

function renderProviderOptions() {
  const provider = selectedProvider();
  const form = $("#generate");
  $("#models").replaceChildren(...(provider ? provider.models : []).map((m) => el("option", { value: m })));
  form.model.placeholder = provider ? provider.default_model : "";

  const container = $("#settings");
  const values = Object.fromEntries([...container.querySelectorAll("[name]")].map((f) => [f.name, f.value]));
  const supported = provider ? provider.settings : [];
  container.replaceChildren(
    ...supported.filter((name) => SETTINGS[name]).map((name) => settingField(name, values[name] || "")),
  );

  const limit = provider ? provider.max_input_images : 0;
  $("#input-limit").textContent = limit ? `Up to ${limit}` : "";
  renderInputs();
}

function settingField(name, value) {
  const spec = SETTINGS[name];
  let control;
  if (spec.options) {
    control = el("select", { name }, el("option", { value: "" }, "Default"), ...spec.options.map((o) => el("option", { value: o }, o)));
  } else {
    const list = spec.suggestions ? `list-${name}` : undefined;
    control = el("input", {
      name,
      type: spec.type || "text",
      min: spec.min,
      max: spec.max,
      placeholder: spec.placeholder,
      list,
      autocomplete: "off",
    });
    if (list) control = [control, el("datalist", { id: list }, ...spec.suggestions.map((s) => el("option", { value: s })))];
  }
  const field = el("label", { class: spec.wide ? "field wide" : "field" }, el("span", {}, spec.label), control);
  const input = field.querySelector("[name]");
  input.value = value;
  return field;
}

function renderProviderList() {
  $("#provider-list").replaceChildren(...state.providers.map(providerRow));
}

function providerRow(p) {
  const row = el(
    "li",
    { class: "provider" },
    el(
      "div",
      { class: "provider-name" },
      p.name,
      el("span", { class: "tag mono" }, `${p.type} · ${p.default_model}`),
    ),
    el(
      "div",
      { class: "provider-status" },
      el("span", { class: p.ready ? "dot on" : "dot" }),
      statusText(p),
    ),
  );

  const actions = el("div", { class: "provider-actions" });
  if (p.login && !p.ready && !state.polls.has(p.login)) {
    actions.append(el("button", { class: "secondary", type: "button", onclick: () => startLogin(p.login, row) }, "Connect"));
  }
  if (p.login && p.auth_method === "login") {
    actions.append(el("button", { class: "ghost", type: "button", onclick: () => logout(p.login) }, "Disconnect"));
  }
  if (actions.childElementCount) row.append(actions);

  const pending = state.polls.get(p.login);
  if (pending && !p.ready) row.append(loginBox(pending));
  return row;
}

function statusText(p) {
  if (p.ready) return `Connected · ${p.auth}`;
  if (p.login) return "Not connected";
  return p.auth.replace(/^not connected: /, "Not connected · ");
}

function loginBox(prompt) {
  return el(
    "div",
    { class: "provider-login" },
    "Open ",
    el("a", { href: prompt.verification_uri, target: "_blank", rel: "noreferrer" }, prompt.verification_uri),
    " and enter ",
    el("code", {}, prompt.user_code),
    ". Waiting for approval…",
  );
}

async function startLogin(account, row) {
  const button = row.querySelector(".secondary");
  button.disabled = true;
  try {
    const prompt = await api(`api/login/${account}`, { method: "POST" });
    state.polls.set(account, prompt);
    renderProviderList();
    window.open(prompt.verification_uri, "_blank", "noreferrer");
    pollLogin(account);
  } catch (error) {
    button.disabled = false;
    row.append(el("div", { class: "provider-login note error" }, error.message));
  }
}

async function pollLogin(account) {
  while (state.polls.has(account)) {
    await new Promise((resolve) => setTimeout(resolve, 3000));
    const status = await api(`api/login/${account}`).catch(() => ({ state: "pending" }));
    if (status.state === "pending") continue;
    state.polls.delete(account);
    await loadProviders();
    if (status.state === "failed") {
      const row = [...document.querySelectorAll(".provider")].find((r) => r.querySelector(".secondary"));
      if (row) row.append(el("div", { class: "provider-login note error" }, status.error));
    }
  }
}

/** Picks up logins started before the page was (re)loaded. */
async function resumeLogins() {
  for (const p of state.providers.filter((p) => p.login && !p.ready && !state.polls.has(p.login))) {
    const status = await api(`api/login/${p.login}`).catch(() => ({}));
    if (status.state !== "pending") continue;
    state.polls.set(p.login, status.prompt);
    renderProviderList();
    pollLogin(p.login);
  }
}

async function logout(account) {
  await api(`api/login/${account}`, { method: "DELETE" });
  await loadProviders();
}

/* Reference images */

function renderInputs() {
  const provider = selectedProvider();
  const limit = provider ? provider.max_input_images : 0;
  $("#input-list").replaceChildren(
    ...state.inputs.map((input, index) =>
      el(
        "div",
        { class: "input-thumb", title: index < limit ? input.name : `${input.name} (over this provider's limit)` },
        el("img", { src: input.dataUri, alt: input.name, style: index < limit ? undefined : "opacity: 0.35" }),
        el(
          "button",
          {
            type: "button",
            "aria-label": `Remove ${input.name}`,
            onclick: () => {
              state.inputs.splice(index, 1);
              renderInputs();
            },
          },
          "×",
        ),
      ),
    ),
  );
}

function readFile(file) {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve({ name: file.name, dataUri: reader.result });
    reader.onerror = () => reject(reader.error);
    reader.readAsDataURL(file);
  });
}

/* Generate */

function showNote(text, isError = false, link) {
  const note = $("#form-note");
  note.className = isError ? "note error" : "note";
  note.replaceChildren(text);
  if (link) note.append(" ", el("a", { href: link.href }, link.text));
  note.hidden = false;
}

function hideNote() {
  $("#form-note").hidden = true;
}

function collectArgs() {
  const form = $("#generate");
  const provider = selectedProvider();
  const args = {
    prompt: form.prompt.value.trim(),
    provider: provider && provider.name,
  };
  if (form.model.value.trim()) args.model = form.model.value.trim();
  for (const field of $("#settings").querySelectorAll("[name]")) {
    const value = field.value.trim();
    if (value) args[field.name] = NUMERIC.has(field.name) ? Number(value) : value;
  }
  const extra = form.extra.value.trim();
  if (extra) {
    let parsed;
    try {
      parsed = JSON.parse(extra);
    } catch {
      throw new Error("Extra must be valid JSON.");
    }
    if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) throw new Error("Extra must be a JSON object.");
    args.extra = parsed;
  }
  const limit = provider ? provider.max_input_images : 0;
  args.input_images = state.inputs.slice(0, limit).map((i) => i.dataUri);
  return args;
}

function setBusy(busy) {
  const button = $("#submit");
  button.disabled = busy;
  $(".spinner", button).hidden = !busy;
  clearInterval(state.timer);
  if (!busy) {
    $(".label", button).textContent = "Generate";
    return;
  }
  const started = Date.now();
  const tick = () => {
    $(".label", button).textContent = `Generating… ${Math.round((Date.now() - started) / 1000)}s`;
  };
  tick();
  state.timer = setInterval(tick, 1000);
}

function showResult(urls, meta) {
  const result = $("#result");
  result.className = urls.length > 1 ? "result multi" : "result";
  result.replaceChildren(
    ...urls.map((url) => el("a", { href: url, target: "_blank" }, el("img", { src: url, alt: "" }))),
  );
  $("#result-meta").textContent = meta;
}

async function generate(event) {
  event.preventDefault();
  let args;
  try {
    args = collectArgs();
  } catch (error) {
    showNote(error.message, true);
    return;
  }
  if (!args.prompt) {
    showNote("Write a prompt first.", true);
    $("#generate").prompt.focus();
    return;
  }
  hideNote();
  setBusy(true);
  try {
    const outcome = await api("api/generate", { method: "POST", body: JSON.stringify(args) });
    const count = outcome.images.length;
    showResult(outcome.urls, `${outcome.provider} · ${outcome.model} · ${count} image${count === 1 ? "" : "s"}`);
    if (outcome.ignored_settings && outcome.ignored_settings.length) {
      showNote(`Not supported by ${outcome.provider}, so not applied: ${outcome.ignored_settings.join(", ")}`);
    }
    const revised = outcome.images.map((i) => i.revised_prompt).filter(Boolean);
    if (revised.length) $("#result-meta").textContent += ` · revised: ${revised[0]}`;
    await loadGallery();
  } catch (error) {
    showNote(error.message, true);
  } finally {
    setBusy(false);
  }
}

/* Gallery */

async function loadGallery() {
  const images = await api("api/images");
  $("#gallery-empty").hidden = images.length > 0;
  $("#gallery").replaceChildren(
    ...images.map((image) =>
      el(
        "button",
        { type: "button", title: image.name, onclick: () => showResult([image.url], image.name) },
        el("img", { src: image.url, alt: image.name, loading: "lazy" }),
      ),
    ),
  );
}

/* Start */

window.addEventListener("hashchange", route);
document.addEventListener("DOMContentLoaded", () => {
  route();
  $("#generate").addEventListener("submit", generate);
  $("#generate").provider.addEventListener("change", renderProviderOptions);
  $("#generate").prompt.addEventListener("keydown", (event) => {
    if (event.key === "Enter" && (event.metaKey || event.ctrlKey)) $("#generate").requestSubmit();
  });
  $("#file").addEventListener("change", async (event) => {
    const files = [...event.target.files];
    event.target.value = "";
    state.inputs.push(...(await Promise.all(files.map(readFile))));
    renderInputs();
  });
  loadProviders()
    .then(resumeLogins)
    .catch((error) => showNote(error.message, true));
  loadGallery().catch(() => {});
});
