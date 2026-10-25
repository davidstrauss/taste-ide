//! The port browser's passkeys: the button at the address bar that says
//! which one answers, the menu that chooses, makes, and deletes them, and
//! the WebAuthn API the page is given in place of the one WebKitGTK lacks.
//!
//! The authenticator itself — the keys, the signing, what a request may be
//! answered for — is `taste_core::passkeys`. This is the page's side and
//! the user's: a script in the page's own world supplies
//! `navigator.credentials` and `PublicKeyCredential`, and sends each
//! `create` and `get` to the IDE through a message handler that answers it;
//! the IDE answers from the chosen passkey and says so in a toast. The
//! menu is the branch switcher's: one row per passkey, the chosen one
//! checked, and a field under them that makes one by name (David,
//! 2026-10-24: "make it like the branch creation UI in the branch switching
//! menu").

use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use taste_core::passkeys::Passkeys;

/// The message handler the page's script sends requests to.
const HANDLER: &str = "tastePasskey";

/// What a passkey answered, for the toast: its name, `create` or `get`,
/// and the site.
pub type UsedHook = Rc<dyn Fn(&str, &str, &str)>;

/// The WebAuthn API, as far as a relying party's script uses it: the two
/// calls, the credential and response classes with their accessors and
/// `toJSON`, and the static checks libraries make first. Installed only
/// where the engine has none of its own. Every binary field crosses to the
/// IDE as base64url, and every answer comes back so.
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
  const ask = async (op, options, signal) => {
    if (signal && signal.aborted) throw aborted(signal);
    const reply = JSON.parse(await handler.postMessage(JSON.stringify({ op, origin: location.origin, options })));
    if (reply.error) throw new DOMException(reply.message, reply.error);
    return reply.ok;
  };
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
    static isConditionalMediationAvailable() { return Promise.resolve(false); }
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
      // Autofill sign-in waits for a choice the IDE never offers, as a
      // browser's does until one is made, and ends only when called off.
      if (options.mediation === 'conditional') {
        return new Promise((_, reject) => {
          if (options.signal) options.signal.addEventListener('abort', () => reject(aborted(options.signal)));
        });
      }
      const r = await ask('get', {
        rpId: p.rpId, challenge: encode(p.challenge),
        allowCredentials: descriptors(p.allowCredentials), userVerification: p.userVerification,
      }, options.signal);
      return new PublicKeyCredential(r, new AuthenticatorAssertionResponse(r), {});
    },
    async store() { throw new DOMException('Storing credentials is not supported here.', 'NotSupportedError'); },
    async preventSilentAccess() {},
  };
  Object.assign(window, { Credential, PublicKeyCredential, AuthenticatorResponse,
    AuthenticatorAttestationResponse, AuthenticatorAssertionResponse });
  Object.defineProperty(Navigator.prototype, 'credentials', { configurable: true, enumerable: true, get: () => credentials });
})();"#;

/// Give a new content manager the page's WebAuthn and the IDE's answer to
/// it: the chosen passkey, for a page on one of `ports`. Once per manager,
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
            "get" => passkeys.assert(origin, &ports, options),
            _ => Err(taste_core::passkeys::Refusal {
                name: "NotSupportedError",
                message: format!("{op:?} is not a passkey request"),
            }),
        };
        let answer = match answered {
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
    empty: gtk::Label,
    entry: gtk::Entry,
    popover: gtk::Popover,
    passkeys: Passkeys,
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
        let empty = gtk::Label::builder()
            .label("No passkeys yet. Name one to make it; the one chosen answers this project's sign-ins.")
            .css_classes(["dim-label"])
            .wrap(true)
            .max_width_chars(34)
            .xalign(0.0)
            .margin_start(6)
            .margin_end(6)
            .margin_top(6)
            .build();
        let entry = gtk::Entry::builder()
            .placeholder_text("new-passkey-name")
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
        // Only the passkeys scroll; the create row stands under them, as
        // the branch switcher's does.
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
        content.append(&empty);
        content.append(&scroller);
        content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        content.append(&create_row);
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
            empty,
            entry,
            popover,
            passkeys,
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

    /// Read the project's passkeys if nobody has yet, off this thread, and
    /// show them.
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
        let chosen = self.passkeys.selected();
        self.name.set_label(chosen.as_deref().unwrap_or_default());
        self.name.set_visible(chosen.is_some());
        self.button.set_tooltip_text(Some(&match &chosen {
            Some(name) => format!("Passkey: {name} answers this project's sign-ins"),
            None => "Passkeys: none chosen, so sign-ins by passkey are refused".to_string(),
        }));
        while let Some(row) = self.rows.first_child() {
            self.rows.remove(&row);
        }
        let all = self.passkeys.list();
        self.empty.set_visible(all.is_empty());
        self.rows.set_visible(!all.is_empty());
        for passkey in all {
            let row = adw::ActionRow::builder()
                .use_markup(false)
                .title(&passkey.name)
                .title_lines(1)
                .subtitle(if passkey.sites.is_empty() {
                    "No sign-ups yet".to_string()
                } else {
                    passkey.sites.join(", ")
                })
                .subtitle_lines(2)
                .activatable(true)
                .build();
            if passkey.selected {
                row.add_suffix(&gtk::Image::from_icon_name("object-select-symbolic"));
            }
            // Deleting throws its keys away for good, so it is held, as
            // the bar's purge is.
            let delete = crate::holdbutton::HoldButton::new(
                "user-trash-symbolic",
                &format!("Hold to delete {} and every key it holds", passkey.name),
            );
            delete.widget.add_css_class("flat");
            delete.widget.set_valign(gtk::Align::Center);
            {
                let weak = Rc::downgrade(self);
                let name = passkey.name.clone();
                delete.set_on_confirm(move || {
                    if let Some(menu) = weak.upgrade() {
                        if menu.passkeys.delete(&name) {
                            menu.persist();
                        }
                        menu.refresh();
                    }
                });
            }
            row.add_suffix(&delete.widget);
            let weak = Rc::downgrade(self);
            let name = passkey.name.clone();
            row.connect_activated(move |_| {
                let Some(menu) = weak.upgrade() else { return };
                if menu.passkeys.select(Some(&name)) {
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
