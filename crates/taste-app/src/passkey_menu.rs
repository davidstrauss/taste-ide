//! The port browser's passkeys: the button at the address bar that says
//! which authenticator answers, the menu that chooses, makes, and deletes
//! them, and the WebAuthn API the page is given in place of the one
//! WebKitGTK lacks.
//!
//! The authenticators themselves — the keys, the signing, what a request
//! may be answered for — are `taste_core::passkeys`. This is the page's
//! side and the user's: a script in the page's own world supplies
//! `navigator.credentials` and `PublicKeyCredential`, and sends each
//! `create` and `get` to the IDE through a message handler that answers it;
//! the IDE answers from the chosen authenticator and says so in a toast.
//! The menu is the branch switcher's: one row per authenticator, the chosen
//! one checked and one always chosen, those that hold no key yet badged
//! "new", a field under them that makes one by name (David, 2026-10-24:
//! "make it like the branch creation UI in the branch switching menu"),
//! and a check for autofill sign-in.
//!
//! Autofill sign-in (`mediation: "conditional"`) is drawn in the page, as
//! a password manager draws its own: a list under the field marked
//! `autocomplete="… webauthn"`, in a closed shadow root, of the chosen
//! authenticator's keys for the site. Every tab a page opens shares its
//! content manager, and so this handler, and a message does not say which
//! view sent it, so a GTK popover would not know where to point.

use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use taste_core::passkeys::Passkeys;

/// The message handler the page's script sends requests to.
const HANDLER: &str = "tastePasskey";

/// What an authenticator answered, for the toast: its name, `create` or
/// `get`, and the site.
pub type UsedHook = Rc<dyn Fn(&str, &str, &str)>;

/// The WebAuthn API, as far as a relying party's script uses it: the two
/// calls, the credential and response classes with their accessors and
/// `toJSON`, the static checks libraries make first, and autofill's list.
/// Installed only where the engine has none of its own. Every binary field
/// crosses to the IDE as base64url, and every answer comes back so. What
/// it draws is styled through the CSSOM, which a page's CSP does not
/// refuse as it would a `<style>` or a `style` attribute.
const SCRIPT: &str = r#"(() => {
  if ('PublicKeyCredential' in window) return;
  const handler = window.webkit && window.webkit.messageHandlers && window.webkit.messageHandlers.HANDLER;
  if (!handler) return;
  const bytesOf = (source) => {
    if (source instanceof ArrayBuffer) return new Uint8Array(source);
    if (ArrayBuffer.isView(source)) return new Uint8Array(source.buffer, source.byteOffset, source.byteLength);
    throw new TypeError('expected an ArrayBuffer or a view of one');
  };
  const encode = (source) => {
    let text = '';
    for (const b of bytesOf(source)) text += String.fromCharCode(b);
    return btoa(text).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
  };
  const decode = (text) => {
    const plain = atob(text.replace(/-/g, '+').replace(/_/g, '/') + '==='.slice((text.length + 3) % 4));
    const out = new Uint8Array(plain.length);
    for (let i = 0; i < plain.length; i++) out[i] = plain.charCodeAt(i);
    return out.buffer;
  };
  const descriptors = (list) => (list || []).map((d) => ({ type: d.type, id: encode(d.id), transports: d.transports }));
  const aborted = (signal) => signal.reason || new DOMException('The operation was aborted.', 'AbortError');
  const ask = async (op, options, signal, pick) => {
    if (signal && signal.aborted) throw aborted(signal);
    const reply = JSON.parse(await handler.postMessage(JSON.stringify({ op, origin: location.origin, options, pick })));
    if (reply.error) throw new DOMException(reply.message, reply.error);
    return reply.ok;
  };
  const requestOf = (p) => ({ rpId: p.rpId, challenge: encode(p.challenge),
    allowCredentials: descriptors(p.allowCredentials), userVerification: p.userVerification });
  class Credential {
    constructor(id, type) { this.id = id; this.type = type; }
  }
  class AuthenticatorResponse {
    constructor(r) { this.clientDataJSON = decode(r.clientDataJSON); }
  }
  class AuthenticatorAttestationResponse extends AuthenticatorResponse {
    #r;
    constructor(r) { super(r); this.#r = r; this.attestationObject = decode(r.attestationObject); }
    getTransports() { return [...this.#r.transports]; }
    getAuthenticatorData() { return decode(this.#r.authenticatorData); }
    getPublicKey() { return decode(this.#r.publicKey); }
    getPublicKeyAlgorithm() { return this.#r.publicKeyAlgorithm; }
    toJSON() {
      const r = this.#r;
      return { clientDataJSON: r.clientDataJSON, attestationObject: r.attestationObject,
        authenticatorData: r.authenticatorData, transports: [...r.transports],
        publicKey: r.publicKey, publicKeyAlgorithm: r.publicKeyAlgorithm };
    }
  }
  class AuthenticatorAssertionResponse extends AuthenticatorResponse {
    #r;
    constructor(r) {
      super(r); this.#r = r;
      this.authenticatorData = decode(r.authenticatorData);
      this.signature = decode(r.signature);
      this.userHandle = r.userHandle ? decode(r.userHandle) : null;
    }
    toJSON() {
      const r = this.#r;
      return { clientDataJSON: r.clientDataJSON, authenticatorData: r.authenticatorData,
        signature: r.signature, userHandle: r.userHandle || null };
    }
  }
  class PublicKeyCredential extends Credential {
    #extensions;
    constructor(r, response, extensions) {
      super(r.id, 'public-key');
      this.rawId = decode(r.id);
      this.response = response;
      this.authenticatorAttachment = 'platform';
      this.#extensions = extensions;
    }
    getClientExtensionResults() { return this.#extensions; }
    toJSON() {
      return { id: this.id, rawId: this.id, type: this.type, response: this.response.toJSON(),
        authenticatorAttachment: this.authenticatorAttachment, clientExtensionResults: this.#extensions };
    }
    static isUserVerifyingPlatformAuthenticatorAvailable() { return Promise.resolve(true); }
    static async isConditionalMediationAvailable() {
      try { return await ask('autofill', {}); } catch { return false; }
    }
    static parseCreationOptionsFromJSON(o) {
      return { ...o, challenge: decode(o.challenge), user: { ...o.user, id: decode(o.user.id) },
        excludeCredentials: (o.excludeCredentials || []).map((d) => ({ ...d, id: decode(d.id) })) };
    }
    static parseRequestOptionsFromJSON(o) {
      return { ...o, challenge: decode(o.challenge),
        allowCredentials: (o.allowCredentials || []).map((d) => ({ ...d, id: decode(d.id) })) };
    }
  }
  const credentials = {
    async create(options = {}) {
      const p = options.publicKey;
      if (!p) throw new DOMException('Only public-key credentials are made here.', 'NotSupportedError');
      const r = await ask('create', {
        rp: p.rp, challenge: encode(p.challenge),
        user: { id: encode(p.user.id), name: p.user.name, displayName: p.user.displayName },
        pubKeyCredParams: p.pubKeyCredParams, excludeCredentials: descriptors(p.excludeCredentials),
        authenticatorSelection: p.authenticatorSelection, attestation: p.attestation,
      }, options.signal);
      const extensions = p.extensions && p.extensions.credProps ? { credProps: { rk: true } } : {};
      return new PublicKeyCredential(r, new AuthenticatorAttestationResponse(r), extensions);
    },
    async get(options = {}) {
      const p = options.publicKey;
      if (!p) throw new DOMException('Only public-key credentials are asked for here.', 'NotSupportedError');
      // Autofill sign-in waits, as a browser's does, until a key is taken
      // from under a sign-in field, and ends early only when called off.
      if (options.mediation === 'conditional') {
        if (options.signal && options.signal.aborted) throw aborted(options.signal);
        return new Promise((resolve, reject) => {
          if (pending) pending.reject(new DOMException('A newer autofill request replaced this one.', 'AbortError'));
          const asked = { p, resolve, reject };
          pending = asked;
          if (options.signal) options.signal.addEventListener('abort', () => {
            if (pending === asked) { pending = null; hide(); }
            reject(aborted(options.signal));
          });
          offer(document.activeElement);
        });
      }
      const r = await ask('get', requestOf(p), options.signal);
      return new PublicKeyCredential(r, new AuthenticatorAssertionResponse(r), {});
    },
    async store() { throw new DOMException('Storing credentials is not supported here.', 'NotSupportedError'); },
    async preventSilentAccess() {},
  };
  // Autofill: the request waiting, and the list drawn under its field.
  let pending = null;
  let list = null;
  const marked = (el) => el instanceof HTMLInputElement
    && (el.getAttribute('autocomplete') || '').toLowerCase().split(/\s+/).includes('webauthn');
  const sheet = new CSSStyleSheet();
  sheet.replaceSync(`
    .menu { box-sizing: border-box; margin-top: 4px; padding: 6px; border-radius: 12px;
      background: #ffffff; color: rgba(0, 0, 0, 0.8);
      box-shadow: 0 0 0 1px rgba(0, 0, 0, 0.14), 0 2px 8px 2px rgba(0, 0, 0, 0.12);
      font: 14.5px/1.3 "Adwaita Sans", Cantarell, system-ui, sans-serif; }
    .row { display: flex; align-items: center; gap: 12px; padding: 6px 10px; border-radius: 6px; cursor: default; }
    .row.lit { background: rgba(0, 0, 0, 0.08); }
    svg { flex: none; width: 16px; height: 16px; fill: currentColor; }
    .text { min-width: 0; }
    .title, .subtitle { white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
    .subtitle { font-size: 12px; opacity: 0.55; }
    @media (prefers-color-scheme: dark) {
      .menu { background: #36363a; color: #ffffff;
        box-shadow: 0 0 0 1px rgba(0, 0, 0, 0.5), 0 2px 8px 2px rgba(0, 0, 0, 0.3); }
      .row.lit { background: rgba(255, 255, 255, 0.1); }
    }`);
  const element = (tag, className, text) => {
    const el = document.createElement(tag);
    if (className) el.className = className;
    if (text !== undefined) el.textContent = text;
    return el;
  };
  // The passkey button's glyph, a person and a key.
  const glyph = () => {
    const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
    svg.setAttribute('viewBox', '0 0 16 16');
    for (const d of ['M5.5 1.75A2.75 2.75 0 1 1 5.5 7.25A2.75 2.75 0 1 1 5.5 1.75Z',
      'M0.5 14.5C0.5 10.9 2.6 8.75 5.5 8.75C6.95 8.75 8.2 9.25 9.1 10.2L9.1 14.5Z',
      'M12.6 2.25A2.6 2.6 0 1 1 12.6 7.45A2.6 2.6 0 1 1 12.6 2.25ZM12.6 3.9A0.95 0.95 0 1 0 12.6 5.8A0.95 0.95 0 1 0 12.6 3.9Z',
      'M11.9 7.2L13.3 7.2L13.3 15L11.9 15Z', 'M13.3 10.6L15.2 10.6L15.2 11.9L13.3 11.9Z',
      'M13.3 12.8L14.6 12.8L14.6 14.1L13.3 14.1Z']) {
      const path = document.createElementNS('http://www.w3.org/2000/svg', 'path');
      path.setAttribute('d', d);
      path.setAttribute('fill-rule', 'evenodd');
      svg.append(path);
    }
    return svg;
  };
  const hide = () => { if (list) { list.host.remove(); list = null; } };
  const place = () => {
    if (!list) return;
    if (!list.field.isConnected) { hide(); return; }
    const r = list.field.getBoundingClientRect();
    const style = list.host.style;
    for (const [name, value] of [['all', 'initial'], ['position', 'fixed'], ['z-index', '2147483647'],
      ['left', `${r.left}px`], ['top', `${r.bottom}px`], ['width', `${Math.max(r.width, 240)}px`]]) {
      style.setProperty(name, value, 'important');
    }
  };
  const light = (i) => {
    if (!list) return;
    list.lit = i;
    list.rows.forEach((row, j) => row.classList.toggle('lit', j === i));
  };
  const take = async (id) => {
    const asked = pending;
    if (!asked) return;
    pending = null;
    hide();
    try {
      const r = await ask('get', requestOf(asked.p), null, id);
      asked.resolve(new PublicKeyCredential(r, new AuthenticatorAssertionResponse(r), {}));
    } catch (e) { asked.reject(e); }
  };
  const offer = async (field) => {
    if (!pending || !marked(field) || (list && list.field === field)) return;
    const asked = pending;
    let offers;
    try { offers = await ask('offers', requestOf(asked.p)); } catch { return; }
    if (pending !== asked || document.activeElement !== field || !offers.length) return;
    if (list && list.field === field) return;
    hide();
    const host = element('div');
    const root = host.attachShadow({ mode: 'closed' });
    root.adoptedStyleSheets = [sheet];
    const menu = element('div', 'menu');
    menu.setAttribute('role', 'listbox');
    const rows = offers.map((o, i) => {
      const row = element('div', 'row');
      row.setAttribute('role', 'option');
      const text = element('div', 'text');
      text.append(element('div', 'title', o.user || 'Passkey'), element('div', 'subtitle', `Passkey · authenticator ${o.authenticator}`));
      row.append(glyph(), text);
      // The field keeps the keyboard, as it does under a browser's list.
      row.addEventListener('pointerdown', (e) => e.preventDefault());
      row.addEventListener('pointerenter', () => light(i));
      row.addEventListener('click', () => take(o.id));
      return row;
    });
    menu.append(...rows);
    root.append(menu);
    list = { host, field, rows, offers, lit: -1 };
    document.documentElement.append(host);
    place();
  };
  document.addEventListener('focusin', (e) => offer(e.target), true);
  document.addEventListener('click', (e) => offer(e.target), true);
  document.addEventListener('focusout', (e) => { if (list && e.target === list.field) hide(); }, true);
  document.addEventListener('keydown', (e) => {
    if (!list || e.target !== list.field) return;
    const n = list.rows.length;
    if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
      light(((list.lit < 0 && e.key === 'ArrowUp' ? 0 : list.lit) + (e.key === 'ArrowDown' ? 1 : -1) + n) % n);
    } else if (e.key === 'Enter' && list.lit >= 0) {
      take(list.offers[list.lit].id);
    } else if (e.key === 'Escape') {
      hide();
    } else {
      return;
    }
    e.preventDefault();
    e.stopImmediatePropagation();
  }, true);
  let placing = false;
  const replace = () => {
    if (!list || placing) return;
    placing = true;
    requestAnimationFrame(() => { placing = false; place(); });
  };
  window.addEventListener('scroll', replace, { capture: true, passive: true });
  window.addEventListener('resize', replace, { passive: true });
  Object.assign(window, { Credential, PublicKeyCredential, AuthenticatorResponse,
    AuthenticatorAttestationResponse, AuthenticatorAssertionResponse });
  Object.defineProperty(Navigator.prototype, 'credentials', { configurable: true, enumerable: true, get: () => credentials });
})();"#;

/// Give a new content manager the page's WebAuthn and the IDE's answer to
/// it: the chosen authenticator, for a page on one of `ports`. Once per manager,
/// as `Problems::install` is: a tab opened from another shares its manager.
pub fn install(
    manager: &webkit6::UserContentManager,
    passkeys: Passkeys,
    ports: Vec<u16>,
    on_used: UsedHook,
) {
    manager.add_script(&webkit6::UserScript::new(
        &SCRIPT.replace("HANDLER", HANDLER),
        webkit6::UserContentInjectedFrames::TopFrame,
        webkit6::UserScriptInjectionTime::Start,
        &[],
        &[],
    ));
    manager.register_script_message_handler_with_reply(HANDLER, None);
    manager.connect_script_message_with_reply_received(Some(HANDLER), move |_, value, reply| {
        let request: serde_json::Value = serde_json::from_str(&value.to_str()).unwrap_or_default();
        let origin = request["origin"].as_str().unwrap_or_default();
        let options = &request["options"];
        let op = request["op"].as_str().unwrap_or_default();
        let answered = match op {
            "create" => passkeys.register(origin, &ports, options),
            "get" => passkeys.assert(origin, &ports, options, request["pick"].as_str()),
            // Neither of these is a use of a key: answered, and nothing
            // kept or told.
            "autofill" => Ok(serde_json::json!(passkeys.autofill())),
            "offers" => passkeys.offers(origin, &ports, options).map(|offers| {
                let name = passkeys.selected().unwrap_or_default();
                offers
                    .into_iter()
                    .map(|o| serde_json::json!({ "id": o.id, "user": o.user_name, "authenticator": name }))
                    .collect()
            }),
            _ => Err(taste_core::passkeys::Refusal {
                name: "NotSupportedError",
                message: format!("{op:?} is not a passkey request"),
            }),
        };
        let answer = match answered {
            Ok(answer) if matches!(op, "autofill" | "offers") => serde_json::json!({ "ok": answer }),
            Ok(credential) => {
                let book = passkeys.clone();
                crate::runtime::runtime().spawn_blocking(move || {
                    if let Err(e) = book.persist() {
                        tracing::warn!("keeping the project's passkeys: {e:#}");
                    }
                });
                let site = options["rp"]["id"]
                    .as_str()
                    .or(options["rpId"].as_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| host_of(origin));
                on_used(&passkeys.selected().unwrap_or_default(), op, &site);
                serde_json::json!({ "ok": credential })
            }
            Err(refusal) => {
                serde_json::json!({ "error": refusal.name, "message": refusal.message })
            }
        };
        match value.context() {
            Some(context) => reply.return_value(&webkit6::javascriptcore::Value::new_string(
                &context,
                Some(&answer.to_string()),
            )),
            None => reply.return_error_message("the page went away"),
        }
        true
    });
}

/// `origin`'s host, for a toast that names the site.
fn host_of(origin: &str) -> String {
    let rest = origin.split_once("://").map_or(origin, |(_, rest)| rest);
    match rest.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) => host.to_string(),
        _ => rest.to_string(),
    }
}

/// The button at the address bar, and the menu it opens.
pub struct PasskeyMenu {
    pub button: gtk::MenuButton,
    name: gtk::Label,
    rows: gtk::ListBox,
    entry: gtk::Entry,
    autofill: gtk::CheckButton,
    popover: gtk::Popover,
    passkeys: Passkeys,
    /// The rows' delete buttons, until the rows are drawn again: a hold
    /// button keeps itself only weakly, and one nothing held never fired.
    deletes: std::cell::RefCell<Vec<Rc<crate::holdbutton::HoldButton>>>,
}

impl PasskeyMenu {
    pub fn new(passkeys: Passkeys) -> Rc<Self> {
        let name = gtk::Label::builder()
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .max_width_chars(12)
            .visible(false)
            .build();
        let face = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        face.append(&gtk::Image::from_icon_name("taste-passkey-symbolic"));
        face.append(&name);
        let rows = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["navigation-sidebar"])
            .build();
        let entry = gtk::Entry::builder()
            .placeholder_text("new-authenticator-name")
            .hexpand(true)
            .max_length(taste_core::passkeys::MAX_NAME as i32)
            .build();
        let create = gtk::Button::builder()
            .label("Create")
            .css_classes(["suggested-action"])
            .build();
        let create_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        create_row.set_margin_top(6);
        create_row.append(&entry);
        create_row.append(&create);
        let autofill = gtk::CheckButton::builder()
            .label("Offer passkeys in sign-in fields")
            .tooltip_text(
                "Autofill sign-in: a page that asks for it is offered the chosen authenticator's \
                 passkeys under its sign-in field. Off, the page is told autofill is unavailable, \
                 from its next load.",
            )
            .margin_top(2)
            .build();
        // Only the authenticators scroll; the create row stands under them,
        // as the branch switcher's does.
        let scroller = gtk::ScrolledWindow::builder()
            .child(&rows)
            .propagate_natural_height(true)
            .max_content_height(360)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();
        let content = gtk::Box::new(gtk::Orientation::Vertical, 4);
        // `TASTE_PROBE_PORT_PASSKEYS` shoots it by this name.
        content.set_widget_name("passkey-menu");
        content.set_width_request(280);
        content.append(&scroller);
        content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        content.append(&create_row);
        content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        content.append(&autofill);
        let popover = gtk::Popover::builder().child(&content).build();
        let button = gtk::MenuButton::builder()
            .child(&face)
            .popover(&popover)
            .css_classes(["flat"])
            .build();
        let menu = Rc::new(Self {
            button,
            name,
            rows,
            entry,
            autofill,
            popover,
            passkeys,
            deletes: std::cell::RefCell::new(Vec::new()),
        });
        {
            let weak = Rc::downgrade(&menu);
            create.connect_clicked(move |_| {
                if let Some(menu) = weak.upgrade() {
                    menu.create();
                }
            });
        }
        // Enter in the field is Create, as in the branch switcher.
        {
            let create = create.clone();
            menu.entry.connect_activate(move |_| create.emit_clicked());
        }
        menu.entry.connect_changed(|entry| {
            entry.remove_css_class("error");
            entry.set_tooltip_text(None);
        });
        {
            let weak = Rc::downgrade(&menu);
            menu.autofill.connect_toggled(move |check| {
                let Some(menu) = weak.upgrade() else { return };
                // A refresh that shows the stored answer changes nothing.
                if !menu.passkeys.loaded() || check.is_active() == menu.passkeys.autofill() {
                    return;
                }
                menu.passkeys.set_autofill(check.is_active());
                menu.persist();
            });
        }
        // Read when shown: another tab may have chosen or made one.
        {
            let weak = Rc::downgrade(&menu);
            menu.popover.connect_show(move |_| {
                if let Some(menu) = weak.upgrade() {
                    menu.load_then_refresh();
                }
            });
        }
        menu.refresh();
        menu
    }

    /// Read the project's authenticators if nobody has yet, off this
    /// thread, and show them.
    pub fn load_then_refresh(self: &Rc<Self>) {
        if self.passkeys.loaded() {
            self.refresh();
            return;
        }
        let passkeys = self.passkeys.clone();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let _ = crate::runtime::runtime()
                .spawn_blocking(move || passkeys.load())
                .await;
            if let Some(menu) = weak.upgrade() {
                menu.refresh();
            }
        });
    }

    /// The button's name and the menu's rows, as the store has them.
    pub fn refresh(self: &Rc<Self>) {
        let all = self.passkeys.list();
        let chosen = all.iter().find(|a| a.selected);
        self.name
            .set_label(chosen.map(|a| a.name.as_str()).unwrap_or_default());
        self.name.set_visible(chosen.is_some());
        self.button.set_tooltip_text(Some(&match chosen {
            Some(a) if a.sites.is_empty() => format!(
                "Authenticator {}, which holds no passkey yet, answers this project's sign-ins",
                a.name
            ),
            Some(a) => format!("Authenticator {} answers this project's sign-ins", a.name),
            None => "Passkeys: the project's authenticators are being read".to_string(),
        }));
        self.autofill.set_active(self.passkeys.autofill());
        self.autofill.set_sensitive(self.passkeys.loaded());
        while let Some(row) = self.rows.first_child() {
            self.rows.remove(&row);
        }
        self.deletes.borrow_mut().clear();
        // Deleting the only authenticator when it holds nothing would make
        // the same fresh one again, so there is nothing to delete.
        let deletable = all.len() > 1 || all.iter().any(|a| !a.sites.is_empty());
        for authenticator in &all {
            let row = adw::ActionRow::builder()
                .use_markup(false)
                .title(&authenticator.name)
                .title_lines(1)
                .subtitle(authenticator.sites.join(", "))
                .subtitle_lines(2)
                .activatable(true)
                .build();
            if authenticator.sites.is_empty() {
                // What a site sees as somebody new.
                row.add_suffix(
                    &gtk::Label::builder()
                        .label("new")
                        .css_classes(["caption", "fresh-badge"])
                        .valign(gtk::Align::Center)
                        .tooltip_text("Holds no passkey yet: to a site, somebody new")
                        .build(),
                );
            }
            if authenticator.selected {
                row.add_suffix(&gtk::Image::from_icon_name("object-select-symbolic"));
            }
            // Deleting throws its keys away for good, so it is held, as
            // the bar's purge is.
            let delete = crate::holdbutton::HoldButton::new(
                "user-trash-symbolic",
                &format!(
                    "Hold to delete {} and every passkey it holds",
                    authenticator.name
                ),
            );
            delete.widget.add_css_class("flat");
            delete.widget.set_valign(gtk::Align::Center);
            delete.widget.set_visible(deletable);
            {
                let weak = Rc::downgrade(self);
                let name = authenticator.name.clone();
                delete.set_on_confirm(move || {
                    if let Some(menu) = weak.upgrade() {
                        if menu.passkeys.delete(&name) {
                            menu.persist();
                        }
                        menu.refresh();
                    }
                });
            }
            // A press let go early says to hold, as the bar's purge does,
            // rather than doing nothing where a delete was looked for.
            {
                let button = delete.widget.downgrade();
                delete.set_on_early_release(move |note| {
                    if let Some(button) = button.upgrade() {
                        crate::portview::toast(button.upcast_ref(), &note);
                    }
                });
            }
            row.add_suffix(&delete.widget);
            self.deletes.borrow_mut().push(delete.clone());
            let weak = Rc::downgrade(self);
            let name = authenticator.name.clone();
            row.connect_activated(move |_| {
                let Some(menu) = weak.upgrade() else { return };
                if menu.passkeys.select(&name) {
                    menu.persist();
                }
                menu.popover.popdown();
                menu.refresh();
            });
            self.rows.append(&row);
        }
    }

    fn create(self: &Rc<Self>) {
        let name = self.entry.text().trim().to_string();
        if name.is_empty() {
            return;
        }
        match self.passkeys.create(&name) {
            Ok(()) => {
                self.entry.set_text("");
                self.persist();
                self.popover.popdown();
                self.refresh();
            }
            Err(why) => {
                self.entry.add_css_class("error");
                self.entry.set_tooltip_text(Some(&why));
            }
        }
    }

    fn persist(&self) {
        let passkeys = self.passkeys.clone();
        crate::runtime::runtime().spawn_blocking(move || {
            if let Err(e) = passkeys.persist() {
                tracing::warn!("keeping the project's passkeys: {e:#}");
            }
        });
    }

    /// TASTE_PROBE_CHECK only: the menu open.
    #[doc(hidden)]
    pub fn open_for_probe(&self) {
        self.button.popup();
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_toast_names_the_host() {
        assert_eq!(super::host_of("http://localhost:8000"), "localhost");
        assert_eq!(super::host_of("http://[::1]:8000"), "[::1]");
        assert_eq!(super::host_of("https://app.localhost"), "app.localhost");
    }
}
