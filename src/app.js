const { invoke } = window.__TAURI__.core;

const $ = (id) => document.getElementById(id);

let state = null; // last snapshot from the backend
let draft = null; // proxy currently shown in the form
let busy = false;
let lastError = null;

// -------------------------------------------------------------------- helpers

function toast(message, kind = "") {
  const el = document.createElement("div");
  el.className = `toast ${kind}`;
  el.textContent = message;
  $("toasts").appendChild(el);
  setTimeout(() => el.remove(), 4200);
}

function result(id, message, kind = "") {
  const el = $(id);
  el.textContent = message;
  el.className = `result ${kind}`;
}

/// Runs a command and keeps its failure in `lastError` (commands that return
/// `()` resolve to `null` on success, so success cannot be inferred from the
/// value alone).
async function call(command, args) {
  busy = true;
  lastError = null;
  try {
    return await invoke(command, args);
  } catch (e) {
    lastError = typeof e === "string" ? e : String(e);
    toast(lastError, "err");
    return undefined;
  } finally {
    busy = false;
  }
}

const failed = () => lastError !== null;

// --------------------------------------------------------------- branding

const SITE = "https://peditx.ir";

/// The opener plugin is what actually hands the URL to the default browser;
/// without it a plain <a> would navigate this webview away from the app.
function openSite(e) {
  e.preventDefault();
  const opener = window.__TAURI__ && window.__TAURI__.opener;
  if (opener) opener.openUrl(SITE).catch((err) => toast(String(err), "err"));
  else window.open(SITE, "_blank", "noopener");
}

$("site-top").onclick = openSite;
$("site-foot").onclick = openSite;

function emptyDraft() {
  return {
    id: 0,
    name: "",
    kind: "http",
    host: "",
    port: 0,
    username: "",
    password: "",
  };
}

function kindLabel(kind) {
  return { http: "HTTP/HTTPS", socks5: "SOCKS5", socks4: "SOCKS4" }[kind] || kind;
}

// ------------------------------------------------------------------ rendering

function renderList() {
  const list = $("proxy-list");
  list.innerHTML = "";
  const active = state.settings.active_id;
  $("rail-count").textContent = state.proxies.length;

  for (const proxy of state.proxies) {
    const li = document.createElement("li");
    li.className = "item" + (draft && draft.id === proxy.id ? " sel" : "");

    const meta = document.createElement("div");
    meta.className = "meta";

    const name = document.createElement("div");
    name.className = "nm";
    const label = document.createElement("span");
    label.className = "nm-t";
    label.textContent = proxy.name || proxy.host;
    const badge = document.createElement("span");
    badge.className = `badge k-${proxy.kind}`;
    badge.textContent = kindLabel(proxy.kind);
    name.append(label, badge);

    const sub = document.createElement("div");
    sub.className = "sub";
    sub.textContent = `${proxy.host}:${proxy.port}`;
    meta.append(name, sub);

    const use = document.createElement("button");
    use.className = "use" + (active === proxy.id ? " active" : "");
    use.textContent = active === proxy.id ? "in use" : "use";
    use.onclick = async (e) => {
      e.stopPropagation();
      await call("set_active", { id: proxy.id });
      if (failed()) return;
      await refresh();
      toast(`${proxy.name || proxy.host} is now the active proxy`, "ok");
    };

    li.append(meta, use);
    li.onclick = () => {
      draft = { ...proxy };
      renderForm();
      renderList();
    };
    list.appendChild(li);
  }

  $("list-hint").hidden = state.proxies.length > 0;
}

/// Discord → relay → active proxy → internet, lit up as each hop becomes usable.
function renderPipeline(relayOn) {
  const el = $("pipeline");
  const active = state.proxies.find((p) => p.id === state.settings.active_id);
  const ready = relayOn && !!active;

  const hop = (label, on) => {
    const s = document.createElement("span");
    s.className = "hop" + (on ? " on" : "");
    s.textContent = label;
    return s;
  };
  const arrow = () => {
    const a = document.createElement("span");
    a.className = "arrow";
    a.textContent = "→";
    return a;
  };

  const hops = [
    ["Discord", relayOn],
    [`relay :${state.settings.listen_port}`, relayOn],
    [active ? active.name || active.host : "no active proxy", ready],
    ["internet", ready],
  ];

  el.replaceChildren();
  hops.forEach(([label, on], i) => {
    if (i) el.appendChild(arrow());
    el.appendChild(hop(label, on));
  });
}

function renderForm() {
  const d = draft;
  $("form-title").textContent = d.id ? d.name || "Edit proxy" : "New proxy";
  $("f-name").value = d.name;
  $("f-kind").value = d.kind;
  $("f-host").value = d.host;
  $("f-port").value = d.port || "";
  $("f-user").value = d.username;
  $("f-pass").value = d.password;

  $("btn-delete").disabled = !d.id;
  result("test-result", "");
}

function pill(id, on, label) {
  const el = $(id);
  el.className = "pill" + (on ? " on" : "");
  el.replaceChildren(document.createElement("i"), label);
}

function renderStatus() {
  const relayOn = state.relay_running;
  pill("pill-relay", relayOn, `relay ${relayOn ? "on" : "off"}`);
  pill("pill-sys", state.system_proxy, `system proxy ${state.system_proxy ? "on" : "off"}`);

  const chip = $("relay-state");
  chip.textContent = relayOn ? `listening :${state.relay_port}` : "stopped";
  chip.classList.toggle("on", relayOn);
  renderPipeline(relayOn);

  $("relay-addr").textContent = `127.0.0.1:${state.settings.listen_port}`;
  $("conn-count").textContent = String(state.connections);
  $("btn-relay").textContent = relayOn ? "Stop relay" : "Start relay";

  // This runs every 2s, so never yank an input out from under the caret.
  const typing = document.activeElement;
  if (typing !== $("f-port-listen"))
    $("f-port-listen").value = state.settings.listen_port ?? "";
  if (typing !== $("f-discord")) $("f-discord").value = state.discord_path || "";
  $("s-system").checked = state.system_proxy;
  $("s-strict").checked = !!state.settings.strict_udp;
  $("s-tray").checked = !!state.settings.close_to_tray;
}

function renderAll() {
  if (!draft) draft = emptyDraft();
  renderList();
  renderForm();
  renderStatus();
}

async function refresh() {
  if (busy) return;
  const next = await invoke("get_state").catch(() => null);
  if (!next) return;
  state = next;
  renderList();
  renderStatus();
}

// -------------------------------------------------------------------- editor

function readForm() {
  draft.name = $("f-name").value.trim();
  draft.kind = $("f-kind").value;
  draft.host = $("f-host").value.trim();
  draft.port = Number($("f-port").value) || 0;
  draft.username = $("f-user").value;
  draft.password = $("f-pass").value;
}

function validate(d) {
  if (!d.host) return "host is required";
  if (!d.port || d.port < 1 || d.port > 65535)
    return "port must be between 1 and 65535";
  if (!d.name) d.name = `${kindLabel(d.kind)} ${d.host}`;
  return null;
}

async function persistDraft() {
  readForm();
  const problem = validate(draft);
  if (problem) {
    toast(problem, "err");
    return false;
  }
  if (!draft.id) {
    draft.id = Date.now();
    if (state.proxies.some((p) => p.id === draft.id)) draft.id += 1;
  }
  await call("save_proxy", { entry: draft });
  if (failed()) return false;
  await refresh();
  renderAll();
  return true;
}

$("btn-new").onclick = () => {
  draft = emptyDraft();
  renderAll();
};

$("f-kind").onchange = () => {
  readForm();
  renderForm();
};

$("btn-save").onclick = async () => {
  if (await persistDraft()) toast("proxy saved", "ok");
};

$("btn-use").onclick = async () => {
  if (!draft.id && !(await persistDraft())) return;
  readForm();
  if (!draft.id) return;
  await call("set_active", { id: draft.id });
  if (failed()) return;
  await refresh();
  renderAll();
  toast("active proxy updated", "ok");
};

$("btn-delete").onclick = async () => {
  if (!draft.id) return;
  await call("delete_proxy", { id: draft.id });
  if (failed()) return;
  draft = emptyDraft();
  await refresh();
  renderAll();
  toast("proxy deleted", "ok");
};

$("btn-test").onclick = async () => {
  readForm();
  const problem = validate(draft);
  if (problem) return toast(problem, "err");
  if (!draft.id) return toast("save the proxy first", "err");
  result("test-result", "testing…");
  const outcome = await call("test_proxy", { id: draft.id });
  if (failed()) result("test-result", lastError, "err");
  else result("test-result", outcome, "ok");
};

// --------------------------------------------------------------------- relay

$("btn-relay").onclick = async () => {
  if (state.relay_running) {
    await call("stop_relay");
    if (failed()) return;
    toast("relay stopped", "ok");
    return refresh();
  }
  const port = await call("start_relay");
  if (failed()) return;
  toast(`relay listening on 127.0.0.1:${port}`, "ok");
  await refresh();
};

$("btn-apply-port").onclick = async () => {
  const port = Number($("f-port-listen").value);
  const next = await call("set_listen_port", { port });
  if (failed()) return;
  toast(`listen port set to ${next}`, "ok");
  await refresh();
};

// ------------------------------------------------------------------- Discord

$("btn-detect").onclick = async () => {
  const found = await call("detect_discord");
  if (failed()) return;
  if (found) {
    await refresh();
    toast(`found ${found}`, "ok");
  } else {
    toast("Discord not found - paste the Discord.exe path below", "err");
  }
};

$("f-discord").onchange = (e) => call("set_discord_path", { path: e.target.value });

$("btn-launch").onclick = async () => {
  const note = await call("launch_discord");
  if (failed()) return;
  toast(note || "Discord started through the proxy", "ok");
  await refresh();
};

$("btn-restore").onclick = async () => {
  await call("set_system_proxy", { on: false });
  if (failed()) return;
  await refresh();
  toast("Windows proxy settings restored", "ok");
};

$("s-system").onchange = async (e) => {
  await call("set_system_proxy", { on: e.target.checked });
  if (failed()) {
    e.target.checked = !e.target.checked;
    return;
  }
  await refresh();
  toast(e.target.checked ? "system proxy enabled" : "system proxy restored", "ok");
};

$("s-strict").onchange = async (e) => {
  await call("set_strict", { strict: e.target.checked });
  if (failed()) e.target.checked = !e.target.checked;
};

$("s-tray").onchange = async (e) => {
  await call("set_close_to_tray", { on: e.target.checked });
  if (failed()) e.target.checked = !e.target.checked;
};

// ---------------------------------------------------------------------- boot

(async function boot() {
  state = await invoke("get_state").catch((e) => {
    toast(typeof e === "string" ? e : String(e), "err");
    return null;
  });
  if (!state) {
    state = {
      proxies: [],
      settings: { listen_port: 17999, close_to_tray: true },
      relay_running: false,
      relay_port: 0,
      connections: 0,
      system_proxy: false,
      discord_path: null,
    };
  }
  draft = emptyDraft();
  renderAll();
  setInterval(refresh, 2000);
})();
