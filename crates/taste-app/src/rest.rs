//! The REST client face of a port tab: a request to the service behind a
//! forwarded port, and its answer, with the service's own schema as the
//! map when it publishes one.
//!
//! Three things and no more:
//!
//! - **A request.** Method, path, headers, body. The body is JSON in a
//!   source view with a Format action that pretty-prints it and says, in
//!   place, where it is not JSON. Nothing is sent that does not parse.
//! - **A schema.** OpenAPI 3 and Swagger 2, JSON documents, looked for at
//!   the places services put them and at any path the user types. Each
//!   operation is a row; choosing one fills the method and path and writes
//!   an example body from the request schema — the shape of what the
//!   service wants, so the user edits values rather than typing keys.
//! - **A response.** Status, timing, headers, and the body — pretty-printed
//!   and highlighted when it is JSON, verbatim otherwise.
//!
//! HTTP is loopback HTTP/1.1 through hyper on the tokio runtime — the
//! same client stack the auth proxy uses — never on the GTK thread. The
//! port is published on 127.0.0.1 by the supervisor, so that is the only
//! host this ever dials.
//!
//! Deliberately not here: auth helpers, environments, collections, history
//! (SEARCH.md's rule that a query is a moment applies to a request too),
//! and a search box of its own.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Instant;

use adw::prelude::*;
use gtk::glib;
use sourceview5::prelude::*;

use crate::hover::FullTextOnHover;
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};

/// Response bodies larger than this are cut: this is a client for looking
/// at an API's answers, not for downloading from it.
const BODY_LIMIT: usize = 8 << 20;

/// Where services publish their schema. Tried in order until one parses.
pub const SCHEMA_CANDIDATES: [&str; 8] = [
    "/openapi.json",
    "/swagger.json",
    "/api/openapi.json",
    "/api-docs",
    "/v3/api-docs",
    "/docs/openapi.json",
    "/.well-known/openapi.json",
    "/swagger/v1/swagger.json",
];

const METHODS: [&str; 7] = ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];

// --- HTTP -------------------------------------------------------------------

/// One answer from the service.
#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub elapsed_ms: u128,
}

impl Response {
    pub fn content_type(&self) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
            .map(|(_, v)| v.as_str())
    }

    pub fn is_json(&self) -> bool {
        self.content_type().is_some_and(|t| t.contains("json"))
            || serde_json::from_slice::<serde_json::Value>(&self.body).is_ok()
                && !self.body.is_empty()
    }
}

/// Send one request. Runs on the tokio runtime; the caller awaits the
/// join handle from the GTK thread.
pub async fn send(
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    body: Option<String>,
) -> anyhow::Result<Response> {
    let client: hyper_util::client::legacy::Client<_, Full<Bytes>> =
        hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
            .build_http();
    let mut request = http::Request::builder()
        .method(http::Method::from_bytes(method.as_bytes())?)
        .uri(&url);
    for (key, value) in &headers {
        request = request.header(key.as_str(), value.as_str());
    }
    let request = request.body(Full::new(Bytes::from(body.unwrap_or_default())))?;
    let started = Instant::now();
    let response = client.request(request).await?;
    let status = response.status();
    let response_headers: Vec<(String, String)> = response
        .headers()
        .iter()
        .map(|(k, v)| {
            (
                k.to_string(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
        .collect();
    let body = Limited::new(response.into_body(), BODY_LIMIT)
        .collect()
        .await
        .map_err(|e| anyhow::anyhow!("reading the body: {e}"))?
        .to_bytes();
    Ok(Response {
        status: status.as_u16(),
        reason: status.canonical_reason().unwrap_or("").to_string(),
        headers: response_headers,
        body: body.to_vec(),
        elapsed_ms: started.elapsed().as_millis(),
    })
}

/// Fetch a JSON document, if that is what lives at `url`.
pub async fn fetch_json(url: String) -> anyhow::Result<serde_json::Value> {
    let response = send(
        "GET".into(),
        url,
        vec![("Accept".into(), "application/json".into())],
        None,
    )
    .await?;
    if response.status >= 400 {
        anyhow::bail!("{} {}", response.status, response.reason);
    }
    Ok(serde_json::from_slice(&response.body)?)
}

// --- schema -----------------------------------------------------------------

/// One operation the schema describes, ready to become a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Operation {
    pub method: String,
    /// The path template with required query parameters appended as
    /// placeholders (`/pets?limit=`), so what the user sends is close to
    /// what the service accepts.
    pub path: String,
    pub summary: String,
    /// An example request body from the operation's JSON schema, pretty
    /// printed; `None` when the operation takes no body.
    pub body_example: Option<String>,
}

/// What a service said about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schema {
    pub title: String,
    /// `OpenAPI 3.1.0`, `Swagger 2.0`.
    pub flavour: String,
    pub operations: Vec<Operation>,
}

impl Schema {
    pub fn describe(&self) -> String {
        format!(
            "{} · {} · {} operation{}",
            self.title,
            self.flavour,
            self.operations.len(),
            if self.operations.len() == 1 { "" } else { "s" }
        )
    }
}

/// Read an OpenAPI 3 or Swagger 2 document. `None` when the JSON is not
/// one — a service's `/api-docs` may well be a page of its own.
pub fn parse_schema(doc: &serde_json::Value) -> Option<Schema> {
    let version_of = |key: &str| doc.get(key).and_then(|v| v.as_str());
    let flavour = version_of("openapi")
        .map(|version| format!("OpenAPI {version}"))
        .or_else(|| version_of("swagger").map(|version| format!("Swagger {version}")))?;
    let title = doc
        .pointer("/info/title")
        .and_then(|t| t.as_str())
        .unwrap_or("Untitled API")
        .to_string();
    let mut operations = Vec::new();
    let paths = doc.get("paths").and_then(|p| p.as_object())?;
    for (path, item) in paths {
        let Some(item) = item.as_object() else {
            continue;
        };
        for method in ["get", "post", "put", "patch", "delete", "head", "options"] {
            let Some(op) = item.get(method) else { continue };
            let summary = op
                .get("summary")
                .or_else(|| op.get("operationId"))
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string();
            // Parameters: the path item's, then the operation's.
            let mut required_query: Vec<String> = Vec::new();
            for source in [item.get("parameters"), op.get("parameters")] {
                let Some(list) = source.and_then(|p| p.as_array()) else {
                    continue;
                };
                for parameter in list {
                    let parameter = resolve(doc, parameter);
                    if parameter.get("in").and_then(|i| i.as_str()) == Some("query")
                        && parameter.get("required").and_then(|r| r.as_bool()) == Some(true)
                    {
                        if let Some(name) = parameter.get("name").and_then(|n| n.as_str()) {
                            required_query.push(name.to_string());
                        }
                    }
                }
            }
            let mut full_path = path.clone();
            if !required_query.is_empty() {
                full_path.push('?');
                full_path.push_str(
                    &required_query
                        .iter()
                        .map(|name| format!("{name}="))
                        .collect::<Vec<_>>()
                        .join("&"),
                );
            }
            let body_schema = op
                .pointer("/requestBody/content/application~1json/schema")
                .or_else(|| {
                    // Swagger 2: a parameter `in: body`.
                    op.get("parameters")
                        .and_then(|p| p.as_array())
                        .and_then(|list| {
                            list.iter()
                                .find(|p| p.get("in").and_then(|i| i.as_str()) == Some("body"))
                        })
                        .and_then(|p| p.get("schema"))
                });
            let body_example = body_schema.map(|schema| {
                serde_json::to_string_pretty(&example_of(doc, schema, 0)).unwrap_or_default()
            });
            operations.push(Operation {
                method: method.to_uppercase(),
                path: full_path,
                summary,
                body_example,
            });
        }
    }
    Some(Schema {
        title,
        flavour,
        operations,
    })
}

/// Follow a `$ref` inside the document (`#/components/schemas/Pet`,
/// `#/definitions/Pet`). External references stay unresolved.
fn resolve<'a>(doc: &'a serde_json::Value, node: &'a serde_json::Value) -> &'a serde_json::Value {
    let mut current = node;
    for _ in 0..8 {
        let Some(reference) = current.get("$ref").and_then(|r| r.as_str()) else {
            return current;
        };
        let Some(pointer) = reference.strip_prefix('#') else {
            return current;
        };
        match doc.pointer(pointer) {
            Some(target) => current = target,
            None => return current,
        }
    }
    current
}

/// An example value for a JSON schema: what the service says it wants,
/// with placeholder values the user overwrites. Bounded in depth so a
/// recursive schema (a tree of nodes) ends.
pub fn example_of(
    doc: &serde_json::Value,
    schema: &serde_json::Value,
    depth: usize,
) -> serde_json::Value {
    use serde_json::Value;
    let schema = resolve(doc, schema);
    if let Some(example) = schema.get("example") {
        return example.clone();
    }
    if let Some(default) = schema.get("default") {
        return default.clone();
    }
    if let Some(first) = schema
        .get("enum")
        .and_then(|e| e.as_array())
        .and_then(|e| e.first())
    {
        return first.clone();
    }
    if depth > 6 {
        return Value::Null;
    }
    for combinator in ["oneOf", "anyOf", "allOf"] {
        if let Some(options) = schema.get(combinator).and_then(|o| o.as_array()) {
            if combinator == "allOf" {
                let mut merged = serde_json::Map::new();
                for option in options {
                    if let Value::Object(fields) = example_of(doc, option, depth + 1) {
                        merged.extend(fields);
                    }
                }
                return Value::Object(merged);
            }
            if let Some(first) = options.first() {
                return example_of(doc, first, depth + 1);
            }
        }
    }
    let kind = match schema.get("type") {
        Some(Value::String(kind)) => kind.as_str(),
        Some(Value::Array(kinds)) => kinds
            .iter()
            .filter_map(|k| k.as_str())
            .find(|k| *k != "null")
            .unwrap_or("object"),
        _ if schema.get("properties").is_some() => "object",
        _ if schema.get("items").is_some() => "array",
        _ => "object",
    };
    match kind {
        "object" => {
            let mut fields = serde_json::Map::new();
            if let Some(properties) = schema.get("properties").and_then(|p| p.as_object()) {
                for (name, property) in properties {
                    if property.get("readOnly").and_then(|r| r.as_bool()) == Some(true) {
                        continue;
                    }
                    fields.insert(name.clone(), example_of(doc, property, depth + 1));
                }
            }
            Value::Object(fields)
        }
        "array" => match schema.get("items") {
            Some(items) => Value::Array(vec![example_of(doc, items, depth + 1)]),
            None => Value::Array(Vec::new()),
        },
        "integer" => Value::from(0),
        "number" => Value::from(0.0),
        "boolean" => Value::Bool(true),
        "null" => Value::Null,
        _ => Value::String(match schema.get("format").and_then(|f| f.as_str()) {
            Some("date-time") => "2026-01-01T00:00:00Z".into(),
            Some("date") => "2026-01-01".into(),
            Some("email") => "user@example.com".into(),
            Some("uri") | Some("url") => "https://example.com".into(),
            Some("uuid") => "00000000-0000-0000-0000-000000000000".into(),
            _ => "string".into(),
        }),
    }
}

/// Pretty-print JSON, or say where it stops being JSON.
pub fn format_json(text: &str) -> Result<String, String> {
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(value) => serde_json::to_string_pretty(&value).map_err(|e| e.to_string()),
        Err(e) => Err(format!("line {}, column {}: {}", e.line(), e.column(), e)),
    }
}

/// `Key: Value` lines into header pairs. Blank lines and lines without a
/// colon are skipped rather than refused — a stray line is not a reason
/// to withhold the request.
pub fn parse_headers(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            let key = key.trim();
            (!key.is_empty()).then(|| (key.to_string(), value.trim().to_string()))
        })
        .collect()
}

fn human_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} kB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

// --- the widget ---------------------------------------------------------------

pub struct RestClient {
    pub widget: gtk::Box,
    base_url: RefCell<String>,
    method: gtk::DropDown,
    path: gtk::Entry,
    send_button: gtk::Button,
    schema_path: gtk::Entry,
    schema_status: gtk::Label,
    endpoints: gtk::ListBox,
    sidebar: gtk::ScrolledWindow,
    headers_buffer: gtk::TextBuffer,
    body_buffer: sourceview5::Buffer,
    body_note: gtk::Label,
    status_line: gtk::Label,
    response_headers: gtk::Label,
    response_expander: gtk::Expander,
    response_buffer: sourceview5::Buffer,
    schema: RefCell<Option<Schema>>,
    /// Bumped per request; a reply for an older one is dropped.
    generation: Cell<u64>,
}

impl RestClient {
    pub fn new(base_url: &str) -> Rc<Self> {
        let json = sourceview5::LanguageManager::default().language("json");

        // Request line: method, path, send.
        let method = gtk::DropDown::from_strings(&METHODS);
        method.set_tooltip_text(Some("Method"));
        let path = gtk::Entry::builder()
            .text("/")
            .placeholder_text("/path?query=")
            .hexpand(true)
            // Narrow on purpose: the editor pane's minimum is the sum of
            // whatever refuses to fold, and an entry's default is wide.
            .width_chars(8)
            .tooltip_text(format!("Relative to {base_url}"))
            .build();
        let send_button = gtk::Button::builder()
            .label("Send")
            .css_classes(["suggested-action"])
            .tooltip_text("Send the request (Ctrl+Enter)")
            .build();
        let request_line = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        request_line.set_margin_top(8);
        request_line.set_margin_start(10);
        request_line.set_margin_end(10);
        request_line.append(&method);
        request_line.append(&path);
        request_line.append(&send_button);

        // Schema line: where to look, and what was found.
        let schema_path = gtk::Entry::builder()
            .text(SCHEMA_CANDIDATES[0])
            .placeholder_text("/openapi.json")
            .width_chars(8)
            .tooltip_text("Where the service publishes its OpenAPI or Swagger document")
            .build();
        let schema_button = gtk::Button::builder()
            .label("Load Schema")
            .css_classes(["flat"])
            .tooltip_text("Look for a schema here, then at the usual places")
            .build();
        let schema_status = gtk::Label::builder()
            .label("No schema loaded")
            .css_classes(["caption", "dim-label"])
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .xalign(0.0)
            .hexpand(true)
            .build()
            .full_text_on_hover();
        let schema_line = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        schema_line.set_margin_top(6);
        schema_line.set_margin_bottom(6);
        schema_line.set_margin_start(10);
        schema_line.set_margin_end(10);
        schema_line.append(&schema_path);
        schema_line.append(&schema_button);
        schema_line.append(&schema_status);

        // The operations, when a schema gave some.
        let endpoints = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Single)
            .css_classes(["navigation-sidebar"])
            .build();
        let sidebar = gtk::ScrolledWindow::builder()
            .child(&endpoints)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .visible(false)
            // A cap on the comfortable width, not a floor: the rows
            // ellipsize, so the true minimum stays small.
            .max_content_width(260)
            .build();

        // Request: headers, then the body.
        let caption = |text: &str| {
            gtk::Label::builder()
                .label(text)
                .css_classes(["caption-heading", "dim-label"])
                .xalign(0.0)
                .hexpand(true)
                .build()
        };
        let headers_view = gtk::TextView::builder()
            .monospace(true)
            .wrap_mode(gtk::WrapMode::WordChar)
            .accepts_tab(false)
            .left_margin(8)
            .right_margin(8)
            .top_margin(4)
            .bottom_margin(4)
            .build();
        let headers_buffer = headers_view.buffer();
        headers_buffer.set_text("Accept: application/json\nContent-Type: application/json");
        let headers_frame = gtk::Frame::builder().child(&headers_view).build();

        let body_buffer = sourceview5::Buffer::new(None);
        body_buffer.set_language(json.as_ref());
        body_buffer.set_highlight_syntax(true);
        crate::editor::apply_scheme_for_style(&body_buffer);
        let body_view = sourceview5::View::with_buffer(&body_buffer);
        body_view.set_monospace(true);
        body_view.set_show_line_numbers(true);
        body_view.set_auto_indent(true);
        body_view.set_indent_width(2);
        body_view.set_insert_spaces_instead_of_tabs(true);
        body_view.set_wrap_mode(gtk::WrapMode::WordChar);
        body_view.set_vexpand(true);
        let body_scroller = gtk::ScrolledWindow::builder()
            .child(&body_view)
            .vexpand(true)
            .build();
        let format_button = gtk::Button::builder()
            .label("Format")
            .css_classes(["flat"])
            .tooltip_text("Pretty-print the body as JSON")
            .build();
        let body_note = gtk::Label::builder()
            .css_classes(["caption", "error"])
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .xalign(1.0)
            .build()
            .full_text_on_hover();
        let body_head = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        body_head.append(&caption("Body"));
        body_head.append(&body_note);
        body_head.append(&format_button);

        let request_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
        request_box.set_margin_start(10);
        request_box.set_margin_end(10);
        request_box.append(&caption("Headers"));
        request_box.append(&headers_frame);
        request_box.append(&body_head);
        request_box.append(&body_scroller);

        // Response: status, headers behind an expander, body.
        let status_line = gtk::Label::builder()
            .label("No request sent yet")
            .css_classes(["caption", "dim-label"])
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .xalign(0.0)
            .hexpand(true)
            .selectable(true)
            .build()
            .full_text_on_hover();
        let response_headers = gtk::Label::builder()
            .css_classes(["monospace", "caption"])
            .xalign(0.0)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .max_width_chars(40)
            .selectable(true)
            .build();
        let response_expander = gtk::Expander::builder()
            .label("Headers")
            .child(&response_headers)
            .sensitive(false)
            .build();
        let response_buffer = sourceview5::Buffer::new(None);
        response_buffer.set_highlight_syntax(true);
        crate::editor::apply_scheme_for_style(&response_buffer);
        let response_view = sourceview5::View::with_buffer(&response_buffer);
        response_view.set_monospace(true);
        response_view.set_editable(false);
        response_view.set_cursor_visible(false);
        response_view.set_show_line_numbers(true);
        response_view.set_wrap_mode(gtk::WrapMode::WordChar);
        response_view.set_vexpand(true);
        let response_scroller = gtk::ScrolledWindow::builder()
            .child(&response_view)
            .vexpand(true)
            .build();
        let response_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
        response_box.set_margin_start(10);
        response_box.set_margin_end(10);
        response_box.set_margin_bottom(8);
        let response_head = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        response_head.append(&caption("Response"));
        response_head.append(&status_line);
        response_box.append(&response_head);
        response_box.append(&response_expander);
        response_box.append(&response_scroller);

        let exchange = gtk::Paned::builder()
            .orientation(gtk::Orientation::Vertical)
            .start_child(&request_box)
            .end_child(&response_box)
            .shrink_start_child(false)
            .shrink_end_child(false)
            .position(220)
            .build();
        let split = gtk::Paned::builder()
            .orientation(gtk::Orientation::Horizontal)
            .start_child(&sidebar)
            .end_child(&exchange)
            .resize_start_child(false)
            .shrink_start_child(true)
            .shrink_end_child(false)
            // Where the operations list opens when a schema gives it
            // something to list; the user's to drag from there.
            .position(240)
            .vexpand(true)
            .build();

        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.append(&request_line);
        widget.append(&schema_line);
        widget.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        widget.append(&split);

        let client = Rc::new(Self {
            widget,
            base_url: RefCell::new(base_url.trim_end_matches('/').to_string()),
            method,
            path,
            send_button,
            schema_path,
            schema_status,
            endpoints,
            sidebar,
            headers_buffer,
            body_buffer,
            body_note,
            status_line,
            response_headers,
            response_expander,
            response_buffer,
            schema: RefCell::new(None),
            generation: Cell::new(0),
        });

        {
            let weak = Rc::downgrade(&client);
            client.send_button.connect_clicked(move |_| {
                if let Some(client) = weak.upgrade() {
                    client.send_request();
                }
            });
        }
        {
            let weak = Rc::downgrade(&client);
            client.path.connect_activate(move |_| {
                if let Some(client) = weak.upgrade() {
                    client.send_request();
                }
            });
        }
        {
            // Ctrl+Enter from the body sends, as a chat composer would.
            let weak = Rc::downgrade(&client);
            let keys = gtk::EventControllerKey::new();
            keys.connect_key_pressed(move |_, key, _, modifier| {
                if (key == gtk::gdk::Key::Return || key == gtk::gdk::Key::KP_Enter)
                    && modifier.contains(gtk::gdk::ModifierType::CONTROL_MASK)
                {
                    if let Some(client) = weak.upgrade() {
                        client.send_request();
                    }
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            });
            body_view.add_controller(keys);
        }
        {
            let weak = Rc::downgrade(&client);
            format_button.connect_clicked(move |_| {
                if let Some(client) = weak.upgrade() {
                    client.format_body();
                }
            });
        }
        {
            let weak = Rc::downgrade(&client);
            schema_button.connect_clicked(move |_| {
                if let Some(client) = weak.upgrade() {
                    client.load_schema();
                }
            });
        }
        {
            let weak = Rc::downgrade(&client);
            client.schema_path.connect_activate(move |_| {
                if let Some(client) = weak.upgrade() {
                    client.load_schema();
                }
            });
        }
        {
            let weak = Rc::downgrade(&client);
            client.endpoints.connect_row_activated(move |_, row| {
                let Some(client) = weak.upgrade() else { return };
                let index = row.index();
                if index < 0 {
                    return;
                }
                let operation = client
                    .schema
                    .borrow()
                    .as_ref()
                    .and_then(|s| s.operations.get(index as usize).cloned());
                if let Some(operation) = operation {
                    client.fill_from(&operation);
                }
            });
        }
        client
    }

    /// The full URL the request line names right now.
    fn url(&self) -> String {
        let path = self.path.text().to_string();
        let path = if path.starts_with('/') || path.is_empty() {
            path
        } else {
            format!("/{path}")
        };
        format!("{}{}", self.base_url.borrow(), path)
    }

    fn method_name(&self) -> String {
        METHODS
            .get(self.method.selected() as usize)
            .copied()
            .unwrap_or("GET")
            .to_string()
    }

    fn body_text(&self) -> String {
        let buffer = &self.body_buffer;
        buffer
            .text(&buffer.start_iter(), &buffer.end_iter(), false)
            .to_string()
    }

    /// Pretty-print the body, or say where it stops being JSON.
    pub fn format_body(&self) -> bool {
        let text = self.body_text();
        if text.trim().is_empty() {
            self.body_note.set_label("");
            return true;
        }
        match format_json(&text) {
            Ok(pretty) => {
                self.body_buffer.set_text(&pretty);
                self.body_note.set_label("");
                true
            }
            Err(e) => {
                self.body_note.set_label(&format!("Not JSON — {e}"));
                false
            }
        }
    }

    /// Put an operation on the request line, with an example body when the
    /// schema describes one.
    pub fn fill_from(&self, operation: &Operation) {
        if let Some(index) = METHODS.iter().position(|m| *m == operation.method) {
            self.method.set_selected(index as u32);
        }
        self.path.set_text(&operation.path);
        match &operation.body_example {
            Some(example) => self.body_buffer.set_text(example),
            None => self.body_buffer.set_text(""),
        }
        self.body_note.set_label("");
    }

    pub fn send_request(self: &Rc<Self>) {
        let method = self.method_name();
        let has_body = matches!(method.as_str(), "POST" | "PUT" | "PATCH" | "DELETE");
        let body = self.body_text();
        if has_body && !body.trim().is_empty() && !self.format_body() {
            // Refused rather than sent: the note under the body says why.
            return;
        }
        let headers = {
            let buffer = &self.headers_buffer;
            parse_headers(&buffer.text(&buffer.start_iter(), &buffer.end_iter(), false))
        };
        let url = self.url();
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        self.status_line.set_label(&format!("{method} {url} …"));
        self.send_button.set_sensitive(false);
        let weak = Rc::downgrade(self);
        let body = (has_body && !body.trim().is_empty()).then_some(body);
        glib::spawn_future_local(async move {
            let handle = crate::runtime::runtime().spawn(send(method, url, headers, body));
            let result = handle.await;
            let Some(client) = weak.upgrade() else { return };
            if client.generation.get() != generation {
                return;
            }
            client.send_button.set_sensitive(true);
            match result {
                Ok(Ok(response)) => client.show_response(&response),
                Ok(Err(e)) => client.show_error(&format!("{e:#}")),
                Err(e) => client.show_error(&format!("{e}")),
            }
        });
    }

    fn show_error(&self, message: &str) {
        self.status_line.set_label(message);
        self.status_line.remove_css_class("success");
        self.status_line.add_css_class("error");
        self.response_expander.set_sensitive(false);
        self.response_buffer.set_text("");
    }

    pub fn show_response(&self, response: &Response) {
        let kind = response
            .content_type()
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("");
        self.status_line.set_label(&format!(
            "{} {} · {} ms · {}{}",
            response.status,
            response.reason,
            response.elapsed_ms,
            human_size(response.body.len()),
            if kind.is_empty() {
                String::new()
            } else {
                format!(" · {kind}")
            }
        ));
        self.status_line.remove_css_class("error");
        self.status_line.remove_css_class("success");
        self.status_line.add_css_class(if response.status < 400 {
            "success"
        } else {
            "error"
        });
        self.response_headers.set_label(
            &response
                .headers
                .iter()
                .map(|(k, v)| format!("{k}: {v}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        self.response_expander.set_sensitive(true);
        let text = String::from_utf8_lossy(&response.body).into_owned();
        let json = response.is_json();
        let shown = if json {
            format_json(&text).unwrap_or(text)
        } else {
            text
        };
        let language = sourceview5::LanguageManager::default();
        self.response_buffer.set_language(
            if json {
                language.language("json")
            } else if kind.contains("html") {
                language.language("html")
            } else if kind.contains("xml") {
                language.language("xml")
            } else {
                None
            }
            .as_ref(),
        );
        self.response_buffer.set_text(&shown);
    }

    /// Look for a schema: the path in the entry first, then the usual
    /// places. The first document that parses wins.
    pub fn load_schema(self: &Rc<Self>) {
        let base = self.base_url.borrow().clone();
        let typed = self.schema_path.text().to_string();
        let mut candidates: Vec<String> = Vec::new();
        if !typed.trim().is_empty() {
            let typed = typed.trim();
            candidates.push(if typed.starts_with('/') {
                typed.to_string()
            } else {
                format!("/{typed}")
            });
        }
        for candidate in SCHEMA_CANDIDATES {
            if !candidates.iter().any(|c| c == candidate) {
                candidates.push(candidate.to_string());
            }
        }
        self.schema_status.set_label("Looking for a schema…");
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let handle = crate::runtime::runtime().spawn(async move {
                for candidate in candidates {
                    let url = format!("{base}{candidate}");
                    if let Ok(doc) = fetch_json(url).await {
                        if let Some(schema) = parse_schema(&doc) {
                            return Some((candidate, schema));
                        }
                    }
                }
                None
            });
            let Ok(found) = handle.await else { return };
            let Some(client) = weak.upgrade() else { return };
            match found {
                Some((at, schema)) => {
                    client.schema_path.set_text(&at);
                    client.set_schema(Some(schema));
                }
                None => {
                    client.set_schema(None);
                    client
                        .schema_status
                        .set_label("No OpenAPI or Swagger document found at the usual places");
                }
            }
        });
    }

    /// Install a schema (or none): the status line and the operations list.
    pub fn set_schema(&self, schema: Option<Schema>) {
        while let Some(child) = self.endpoints.first_child() {
            self.endpoints.remove(&child);
        }
        match &schema {
            Some(schema) => {
                self.schema_status.set_label(&schema.describe());
                for operation in &schema.operations {
                    let row = adw::ActionRow::builder()
                        .title(format!("{} {}", operation.method, operation.path))
                        .subtitle(&operation.summary)
                        .title_lines(1)
                        .subtitle_lines(1)
                        .activatable(true)
                        .build();
                    self.endpoints.append(&row);
                }
                self.sidebar.set_visible(!schema.operations.is_empty());
            }
            None => self.sidebar.set_visible(false),
        }
        *self.schema.borrow_mut() = schema;
    }

    /// TASTE_PROBE_CHECK only: a schema, a filled request and an answer, so
    /// the frame shows the client at work rather than empty.
    #[doc(hidden)]
    pub fn seed_for_probe(&self) {
        let doc: serde_json::Value = serde_json::from_str(PROBE_SCHEMA).expect("probe schema");
        let schema = parse_schema(&doc).expect("probe schema parses");
        self.schema_path.set_text("/openapi.json");
        let operation = schema
            .operations
            .iter()
            .find(|o| o.method == "POST")
            .cloned()
            .expect("a POST in the probe schema");
        self.set_schema(Some(schema));
        self.fill_from(&operation);
        self.endpoints
            .select_row(self.endpoints.row_at_index(1).as_ref());
        self.show_response(&Response {
            status: 201,
            reason: "Created".into(),
            headers: vec![
                ("content-type".into(), "application/json; charset=utf-8".into()),
                ("content-length".into(), "93".into()),
                ("x-request-id".into(), "7f3a1c".into()),
            ],
            body: br#"{"id":42,"title":"Keep the scroll position across the rebuild","state":"queued","createdAt":"2026-09-06T17:54:02Z"}"#.to_vec(),
            elapsed_ms: 12,
        });
    }
}

/// The schema the probe frame is shot against: small, and shaped like a
/// real one (a `$ref`, a required query parameter, a body).
#[doc(hidden)]
pub const PROBE_SCHEMA: &str = r##"{
  "openapi": "3.1.0",
  "info": { "title": "Backlog API", "version": "1.0.0" },
  "paths": {
    "/issues": {
      "get": {
        "summary": "List issues",
        "parameters": [
          { "name": "state", "in": "query", "required": true, "schema": { "type": "string", "enum": ["queued", "working", "review"] } }
        ]
      },
      "post": {
        "summary": "File an issue",
        "requestBody": { "content": { "application/json": { "schema": { "$ref": "#/components/schemas/NewIssue" } } } }
      }
    },
    "/issues/{id}": {
      "get": { "summary": "One issue" },
      "delete": { "summary": "Decline an issue" }
    }
  },
  "components": {
    "schemas": {
      "NewIssue": {
        "type": "object",
        "properties": {
          "title": { "type": "string", "example": "Keep the scroll position across the rebuild" },
          "body": { "type": "string" },
          "priority": { "type": "string", "enum": ["low", "medium", "high"] },
          "tags": { "type": "array", "items": { "type": "string" } }
        }
      }
    }
  }
}"##;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_openapi_operations_and_examples() {
        let doc: serde_json::Value = serde_json::from_str(PROBE_SCHEMA).unwrap();
        let schema = parse_schema(&doc).unwrap();
        assert_eq!(schema.title, "Backlog API");
        assert_eq!(schema.flavour, "OpenAPI 3.1.0");
        assert_eq!(schema.operations.len(), 4);
        let list = &schema.operations[0];
        assert_eq!(
            (list.method.as_str(), list.path.as_str()),
            ("GET", "/issues?state=")
        );
        assert!(list.body_example.is_none());
        let file = &schema.operations[1];
        assert_eq!(file.method, "POST");
        let body: serde_json::Value =
            serde_json::from_str(file.body_example.as_ref().unwrap()).unwrap();
        // The $ref was followed, the example honoured, the enum's first
        // value taken, the array given one item.
        assert_eq!(body["title"], "Keep the scroll position across the rebuild");
        assert_eq!(body["priority"], "low");
        assert_eq!(body["tags"], serde_json::json!(["string"]));
        assert_eq!(body["body"], "string");
    }

    #[test]
    fn swagger_two_bodies_come_from_the_body_parameter() {
        let doc = serde_json::json!({
            "swagger": "2.0",
            "info": { "title": "Old" },
            "paths": { "/pets": { "post": { "parameters": [
                { "in": "body", "name": "pet", "schema": { "$ref": "#/definitions/Pet" } }
            ] } } },
            "definitions": { "Pet": { "type": "object", "properties": {
                "name": { "type": "string" }, "age": { "type": "integer" }, "id": { "type": "integer", "readOnly": true }
            } } }
        });
        let schema = parse_schema(&doc).unwrap();
        assert_eq!(schema.flavour, "Swagger 2.0");
        let body: serde_json::Value =
            serde_json::from_str(schema.operations[0].body_example.as_ref().unwrap()).unwrap();
        assert_eq!(body, serde_json::json!({ "name": "string", "age": 0 }));
    }

    #[test]
    fn not_a_schema_is_none_and_recursion_ends() {
        assert!(parse_schema(&serde_json::json!({ "hello": "world" })).is_none());
        let doc = serde_json::json!({ "components": { "schemas": { "Node": {
            "type": "object", "properties": { "child": { "$ref": "#/components/schemas/Node" } }
        } } } });
        let example = example_of(
            &doc,
            &serde_json::json!({ "$ref": "#/components/schemas/Node" }),
            0,
        );
        assert!(example.is_object(), "{example}");
    }

    #[test]
    fn json_formatting_says_where_it_broke_and_headers_parse() {
        assert_eq!(format_json(r#"{"a":1}"#).unwrap(), "{\n  \"a\": 1\n}");
        let err = format_json("{\"a\": }").unwrap_err();
        assert!(err.starts_with("line 1, column"), "{err}");
        assert_eq!(
            parse_headers("Accept: application/json\n\nnot a header\nX-Id:  7 "),
            vec![
                ("Accept".to_string(), "application/json".to_string()),
                ("X-Id".to_string(), "7".to_string())
            ]
        );
    }
}
