// DEV ONLY. Injected into index.html by dev/mock_server.py and never shipped:
// the real dashboard has no reference to this file. Stands in for Pomerium by
// choosing who you are, and shows the current authenticator code for that person.
const bar = document.createElement("div");
bar.setAttribute("role", "region");
bar.setAttribute("aria-label", "Development controls");
bar.style.cssText = [
  "position:fixed", "left:12px", "bottom:12px", "z-index:9999", "display:flex", "gap:8px", "align-items:center",
  "padding:6px 10px", "background:#6b21a8", "color:#fff", "border-radius:6px", "font:13px/1.2 system-ui,sans-serif",
  "box-shadow:0 6px 18px rgba(0,0,0,.25)",
].join(";");
document.body.appendChild(bar);

async function render() {
  const st = await (await fetch("/__dev/state", { credentials: "same-origin" })).json();
  bar.replaceChildren();
  const tag = document.createElement("strong");
  tag.textContent = "DEV";
  const select = document.createElement("select");
  select.setAttribute("aria-label", "Act as");
  select.style.cssText = "font:inherit;padding:2px 4px;border-radius:4px";
  const none = new Option("(no SSO identity)", "");
  select.add(none);
  for (const u of st.users) select.add(new Option(`${u.email} (${u.role}${u.totp_enabled ? "" : ", not enrolled"})`, u.email, false, u.email === st.email));
  if (!st.email) none.selected = true;
  select.addEventListener("change", async () => {
    await fetch("/__dev/as", { method: "POST", credentials: "same-origin", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ email: select.value }) });
    location.hash = "#/";
    location.reload();
  });
  const code = document.createElement("span");
  code.textContent = st.current_code ? `code ${st.current_code}` : "";
  code.style.cssText = "font-variant-numeric:tabular-nums;letter-spacing:.05em";
  bar.append(tag, document.createTextNode("act as"), select, code);
}

render().then(() => setInterval(async () => {
  const st = await (await fetch("/__dev/state", { credentials: "same-origin" })).json();
  const code = bar.querySelector("span");
  if (code) code.textContent = st.current_code ? `code ${st.current_code}` : "";
}, 5000));
