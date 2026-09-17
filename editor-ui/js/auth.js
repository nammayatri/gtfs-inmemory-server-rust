// The sign-in gate: SSO identity comes from Pomerium; this screen handles the
// second step - setting up an authenticator app, or entering its code.
import { get, post, del, ApiError } from "./api.js";
import { h, clear } from "./util.js";
import { qrcode } from "../vendor/qrcode-generator-2.0.4/qrcode.mjs";

const gate = () => document.getElementById("gate");

// Resolves with `me` once the person has a session; until then owns the page.
export async function ensureSignedIn() {
  for (;;) {
    let me;
    try {
      me = await get("auth/me");
    } catch (e) {
      showBlocked(e);
      await new Promise(() => {});   // nothing more to do until they reload
    }
    if (me.session) {
      gate().hidden = true;
      return me;
    }
    document.getElementById("app").hidden = true;
    gate().hidden = false;
    await (me.totp_enabled ? codeStep(me) : enrolStep(me));
  }
}

function showBlocked(e) {
  document.getElementById("app").hidden = true;
  gate().hidden = false;
  const byCode = {
    no_sso_identity: ["Open the editor from its sign-in address",
      "This page only works when opened through the editor's sign-in address while you are on the VPN. Open that address and sign in with your company account."],
    not_registered: ["You have not been added yet",
      "Your company sign-in worked, but this account is not a GTFS editor user. Ask a GTFS editor admin to add you, then reload this page."],
    account_disabled: ["Your editor access is turned off",
      "Ask a GTFS editor admin to turn it back on, then reload this page."],
    network: ["The editor server cannot be reached",
      "Check that you are on the VPN, then reload this page."],
  };
  const [title, text] = byCode[e.code] || ["The editor could not start", e.message];
  clear(gate(), h("div.gate-box",
    h("p.gate-mark", "GTFS editor"),
    h("h1", title),
    h("p", text),
    h("div.btn-row", h("button.btn", { type: "button", on: { click: () => location.reload() } }, "Reload")),
  ));
}

function codeField(onComplete) {
  const input = h("input.code-input", {
    type: "text", inputmode: "numeric", autocomplete: "one-time-code", maxlength: "6",
    pattern: "[0-9]{6}", "aria-describedby": "code-error", required: true,
  });
  input.addEventListener("input", () => {
    input.value = input.value.replace(/\D/g, "").slice(0, 6);
    input.removeAttribute("aria-invalid");
    if (input.value.length === 6) onComplete(input.value);
  });
  return input;
}

function explain(e, errorEl, input, retry) {
  input.value = "";
  input.setAttribute("aria-invalid", "true");
  if (e instanceof ApiError && e.code === "locked") {
    input.disabled = true;
    let left = e.details.retry_after_seconds || 600;
    const tick = () => {
      const mins = Math.ceil(left / 60);
      errorEl.textContent = `Too many wrong codes. Sign-in is paused for ${mins} minute${mins === 1 ? "" : "s"}.`;
      if (left-- <= 0) { clearInterval(t); input.disabled = false; errorEl.textContent = ""; input.focus(); }
    };
    const t = setInterval(tick, 1000);
    tick();
    return;
  }
  if (e instanceof ApiError && e.code === "invalid_code") {
    const left = e.details.attempts_left;
    errorEl.textContent = `That code is not right.${left !== undefined ? ` ${left} more tr${left === 1 ? "y" : "ies"} before sign-in pauses for 10 minutes.` : ""} Codes change every 30 seconds; use the one showing now.`;
  } else if (e instanceof ApiError && e.code === "code_reused") {
    errorEl.textContent = "That code was just used. Wait for the app to show a new one.";
  } else {
    errorEl.textContent = e.message;
  }
  input.focus();
  if (retry) retry();
}

function codeStep(me) {
  return new Promise((resolve) => {
    const error = h("p.notice.error#code-error", { role: "alert", hidden: true });
    let busy = false;
    const submit = async (code) => {
      if (busy) return;
      busy = true;
      try {
        await post("auth/session", { code });
        resolve();
      } catch (e) {
        error.hidden = false;
        explain(e, error, input);
      } finally {
        busy = false;
      }
    };
    const input = codeField(submit);
    clear(gate(), h("form.gate-box", { on: { submit: (ev) => { ev.preventDefault(); if (input.value.length === 6) submit(input.value); } } },
      h("p.gate-mark", "GTFS editor"),
      h("h1", "Enter your sign-in code"),
      h("p", `Signed in as ${me.email}. Open your authenticator app and type the 6-digit code for GTFS Editor.`),
      h("label.field", h("span", "Code"), input),
      error,
      h("div.btn-row", h("button.btn", { type: "submit" }, "Sign in")),
      h("p.hint", "Lost your phone? Ask a GTFS editor admin to reset your two-step sign-in."),
    ));
    input.focus();
  });
}

function qrSvg(text) {
  const qr = qrcode(0, "M");
  qr.addData(text);
  qr.make();
  const n = qr.getModuleCount(), margin = 2, size = n + margin * 2;
  const ns = "http://www.w3.org/2000/svg";
  const svg = document.createElementNS(ns, "svg");
  svg.setAttribute("viewBox", `0 0 ${size} ${size}`);
  svg.setAttribute("class", "qr");
  svg.setAttribute("role", "img");
  svg.setAttribute("aria-label", "QR code for your authenticator app");
  let d = "";
  for (let r = 0; r < n; r++) for (let c = 0; c < n; c++) if (qr.isDark(r, c)) d += `M${c + margin} ${r + margin}h1v1h-1z`;
  const path = document.createElementNS(ns, "path");
  path.setAttribute("d", d);
  path.setAttribute("fill", "#14252a");
  svg.appendChild(path);
  return svg;
}

async function enrolStep(me) {
  let enrol;
  try {
    enrol = await post("auth/totp/enroll");
  } catch (e) {
    showBlocked(e);
    await new Promise(() => {});
  }
  return new Promise((resolve) => {
    const error = h("p.notice.error#code-error", { role: "alert", hidden: true });
    const submit = async (code) => {
      try {
        await post("auth/totp/confirm", { code });
        resolve();
      } catch (e) {
        error.hidden = false;
        explain(e, error, input);
      }
    };
    const input = codeField(submit);
    const grouped = enrol.secret_base32.match(/.{1,4}/g).join(" ");
    clear(gate(), h("form.gate-box", { on: { submit: (ev) => { ev.preventDefault(); if (input.value.length === 6) submit(input.value); } } },
      h("p.gate-mark", "GTFS editor"),
      h("h1", "Set up two-step sign-in"),
      h("p", `Changes you make here go live for passengers, so ${me.email} needs a second step: a code from an authenticator app on your phone.`),
      h("ol.gate-steps",
        h("li", "Install an authenticator app, such as Google Authenticator or Microsoft Authenticator."),
        h("li", h("p", "In the app, add an account and scan this code."), qrSvg(enrol.otpauth_uri),
          h("p.hint", "Cannot scan? Choose to enter a setup key and type:"), h("p.secret", grouped)),
        h("li", h("label.field", h("span", "Type the 6-digit code the app shows"), input)),
      ),
      error,
      h("div.btn-row", h("button.btn", { type: "submit" }, "Turn on two-step sign-in")),
    ));
    input.focus();
  });
}

export async function signOut() {
  try { await del("auth/session"); } finally { location.reload(); }
}
