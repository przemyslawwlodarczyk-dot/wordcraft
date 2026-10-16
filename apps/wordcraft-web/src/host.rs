//! The `postMessage` bridge to a same-origin parent page (`?host=parent`), for pages that embed
//! WordCraft in an `<iframe>` (the same protocol as GridCraft's `host.rs`, with the names
//! `wordcraft:*`). Messages are plain objects with a `type`:
//!
//! - `wordcraft:ready` (to the parent): `{ version }`, the app is listening; send a file now.
//! - `wordcraft:open` (from the parent): `{ name, bytes }`, bytes an `ArrayBuffer` or
//!   `Uint8Array`. It opens like a document picked with File › Open or dropped on the window
//!   (the format comes from the bytes).
//! - `wordcraft:save` (to the parent): `{ ticket, name, bytes }` (a transferred `ArrayBuffer`)
//!   for every file WordCraft would otherwise download: Save and Save As (the document) and
//!   the exports (PDF, page images). The parent stores it.
//! - `wordcraft:saved` (from the parent): `{ ticket, ok, error? }`. A document counts as saved
//!   only once the parent confirms its ticket; `ok: false` keeps it unsaved and shows `error`.
//! - `wordcraft:dirty` (to the parent): `{ dirty }` whenever unsaved changes appear or go away.
//! - `wordcraft:execute` (from the parent): `{ id, command, params? }` runs a command by its id
//!   (the same ids as the CLI, the control channel and MCP, e.g. `document.inspect`); `params` is
//!   an object or a JSON string. The app answers with
//! - `wordcraft:result` (to the parent): `{ id, ok, valueJson? , error? }`.
//! - `wordcraft:failed` (to the parent): `{ error }` when the app couldn't start.
//!
//! Only the parent window on the page's own origin is heard or answered. In host mode the page
//! starts with an empty document and never opens a *document* from the browser (File › Open, a
//! dropped file): the parent owns the document. Pictures, fonts and recipient lists still come
//! from the browser.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use wasm_bindgen::JsCast as _;
use wasm_bindgen::JsValue;
use wasm_bindgen::closure::Closure;
use wordcraft_ui_egui::async_saves::{AsyncSaves, SaveOutcome};
use wordcraft_ui_egui::{Inbox, Services, WordApp};

/// The longest user name taken from `?author=`.
const MAX_AUTHOR_CHARS: usize = 200;
/// Most commands waiting for a frame; further ones are answered with an error at once.
const MAX_PENDING_COMMANDS: usize = 256;
/// Longest `params` a command may carry (bytes of JSON).
const MAX_PARAMS_BYTES: usize = 16 << 20;

/// One `wordcraft:execute` waiting for the app's next frame.
struct Command {
    id: String,
    command: String,
    params: Result<serde_json::Value, String>,
}

#[derive(Clone)]
pub struct Bridge {
    window: web_sys::Window,
    parent: web_sys::Window,
    origin: String,
    saves: AsyncSaves,
    /// The unsaved-changes state last reported to the parent.
    reported: Rc<Cell<Option<bool>>>,
    commands: Rc<RefCell<Vec<Command>>>,
}

impl Bridge {
    /// `None` when the page isn't framed (there is no parent to talk to).
    pub fn new(saves: AsyncSaves) -> Option<Self> {
        let window = web_sys::window()?;
        let parent = window.parent().ok().flatten()?;
        if js_sys::Object::is(&parent, &window) {
            return None;
        }
        let origin = window.location().origin().ok()?;
        Some(Self { window, parent, origin, saves, reported: Rc::default(), commands: Rc::default() })
    }

    /// Send saved and exported files to the parent instead of downloading them, hear files and
    /// commands from it, and keep File › Open for pictures, fonts and recipient lists only.
    pub fn connect(&self, services: &mut Services, inbox: Inbox, ctx: egui::Context) {
        self.listen(inbox, ctx);
        let bridge = self.clone();
        services.download = Some(Box::new(move |name: &str, bytes: &[u8]| bridge.post_save(name, bytes)));
        let picker = services.open_async.take();
        services.open_async = Some(Box::new(move |purpose: &str| {
            if purpose == "document" {
                log::info!("host mode: the page owns the document; File › Open ignored");
                return;
            }
            if let Some(p) = &picker {
                p(purpose);
            }
        }));
    }

    /// `?author=`: the host's name for the user signs comments and tracked changes.
    pub fn set_author(app: &mut WordApp, author: &str) {
        let author: String = author.trim().chars().take(MAX_AUTHOR_CHARS).collect();
        if !author.is_empty() {
            app.session.author = author;
        }
    }

    pub fn post_ready(&self) {
        self.send(&[("type", "wordcraft:ready".into()), ("version", env!("CARGO_PKG_VERSION").into())]);
    }

    pub fn post_failed(&self, error: &str) {
        self.send(&[("type", "wordcraft:failed".into()), ("error", error.into())]);
    }

    /// Each frame: run the commands the parent sent and answer them, and tell the parent when
    /// unsaved changes appear or go away.
    pub fn sync(&self, app: &mut WordApp) {
        let commands = std::mem::take(&mut *self.commands.borrow_mut());
        for c in commands {
            let outcome = match c.params {
                Ok(params) => app.execute(&c.command, params),
                Err(e) => Err(e),
            };
            match outcome {
                Ok(v) => {
                    let text = serde_json::to_string(&v).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"));
                    self.send(&[
                        ("type", "wordcraft:result".into()),
                        ("id", c.id.as_str().into()),
                        ("ok", true.into()),
                        ("valueJson", text.as_str().into()),
                    ]);
                }
                Err(e) => {
                    self.send(&[
                        ("type", "wordcraft:result".into()),
                        ("id", c.id.as_str().into()),
                        ("ok", false.into()),
                        ("error", e.as_str().into()),
                    ]);
                }
            }
        }
        let dirty = app.session.dirty || self.saves.pending() > 0;
        if self.reported.get() != Some(dirty) && self.send(&[("type", "wordcraft:dirty".into()), ("dirty", dirty.into())]) {
            self.reported.set(Some(dirty));
        }
    }

    /// Hands the bytes to the parent and opens a save ticket the parent closes with
    /// `wordcraft:saved`; until then the document isn't confirmed saved.
    fn post_save(&self, path: &str, bytes: &[u8]) -> Result<(), String> {
        let name = std::path::Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| path.to_string());
        let ticket = self.saves.start();
        let buffer = js_sys::Uint8Array::from(bytes).buffer();
        let message = message(&[
            ("type", "wordcraft:save".into()),
            ("ticket", JsValue::from_f64(ticket as f64)),
            ("name", name.as_str().into()),
            ("bytes", buffer.clone().into()),
        ])
        .ok_or_else(|| "couldn't build the message".to_string())?;
        if let Err(e) = self.parent.post_message_with_transfer(&message, &self.origin, &js_sys::Array::of1(&buffer)) {
            let msg = e.as_string().unwrap_or_else(|| format!("{e:?}"));
            self.saves.finish(ticket, SaveOutcome::Failed(msg.clone()));
            return Err(msg);
        }
        Ok(())
    }

    fn listen(&self, inbox: Inbox, ctx: egui::Context) {
        let parent = self.parent.clone();
        let origin = self.origin.clone();
        let saves = self.saves.clone();
        let commands = self.commands.clone();
        let on_message = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |event: web_sys::MessageEvent| {
            if event.origin() != origin || !event.source().is_some_and(|s| js_sys::Object::is(&s, &parent)) {
                return;
            }
            let data = event.data();
            match string(&data, "type").as_deref() {
                Some("wordcraft:open") => {
                    let name = string(&data, "name").filter(|n| !n.trim().is_empty()).unwrap_or_else(|| "Document.docx".to_string());
                    let Some(bytes) = bytes(&data) else { return };
                    inbox.lock().unwrap_or_else(|e| e.into_inner()).push((name, bytes));
                    ctx.request_repaint();
                }
                Some("wordcraft:saved") => {
                    let Some(ticket) = number(&data, "ticket") else { return };
                    let ok = js_sys::Reflect::get(&data, &JsValue::from_str("ok")).ok().and_then(|v| v.as_bool()).unwrap_or(false);
                    let outcome = if ok {
                        SaveOutcome::Saved
                    } else {
                        SaveOutcome::Failed(string(&data, "error").unwrap_or_else(|| "the page couldn't save the document".to_string()))
                    };
                    saves.finish(ticket, outcome);
                    ctx.request_repaint();
                }
                Some("wordcraft:execute") => {
                    let id = string(&data, "id").unwrap_or_default();
                    let command = string(&data, "command").unwrap_or_default();
                    let params = params(&data);
                    let mut queue = commands.borrow_mut();
                    if queue.len() >= MAX_PENDING_COMMANDS {
                        queue.push(Command { id, command, params: Err("too many commands waiting".to_string()) });
                    } else {
                        queue.push(Command { id, command, params });
                    }
                    ctx.request_repaint();
                }
                _ => {}
            }
        });
        if self.window.add_event_listener_with_callback("message", on_message.as_ref().unchecked_ref()).is_ok() {
            // The listener lives as long as the page.
            on_message.forget();
        }
    }

    /// Post a message to the parent; false if it couldn't be sent.
    fn send(&self, fields: &[(&str, JsValue)]) -> bool {
        message(fields).is_some_and(|m| self.parent.post_message(&m, &self.origin).is_ok())
    }
}

fn message(fields: &[(&str, JsValue)]) -> Option<js_sys::Object> {
    let m = js_sys::Object::new();
    for (key, value) in fields {
        js_sys::Reflect::set(&m, &JsValue::from_str(key), value).ok()?;
    }
    Some(m)
}

fn string(data: &JsValue, key: &str) -> Option<String> {
    js_sys::Reflect::get(data, &JsValue::from_str(key)).ok()?.as_string()
}

fn number(data: &JsValue, key: &str) -> Option<u64> {
    let v = js_sys::Reflect::get(data, &JsValue::from_str(key)).ok()?.as_f64()?;
    if v.is_finite() && v >= 0.0 { Some(v as u64) } else { None }
}

/// `bytes` as an `ArrayBuffer` or `Uint8Array`.
fn bytes(data: &JsValue) -> Option<Vec<u8>> {
    let bytes = js_sys::Reflect::get(data, &JsValue::from_str("bytes")).ok()?;
    if let Some(array) = bytes.dyn_ref::<js_sys::Uint8Array>() {
        Some(array.to_vec())
    } else if bytes.is_instance_of::<js_sys::ArrayBuffer>() {
        Some(js_sys::Uint8Array::new(&bytes).to_vec())
    } else {
        None
    }
}

/// `params` as a JSON object or a JSON string; missing means no params.
fn params(data: &JsValue) -> Result<serde_json::Value, String> {
    let Ok(raw) = js_sys::Reflect::get(data, &JsValue::from_str("params")) else {
        return Ok(serde_json::json!({}));
    };
    if raw.is_undefined() || raw.is_null() {
        return Ok(serde_json::json!({}));
    }
    let text = match raw.as_string() {
        Some(s) => s,
        None => js_sys::JSON::stringify(&raw).ok().and_then(|s| s.as_string()).ok_or_else(|| "params are not JSON".to_string())?,
    };
    if text.len() > MAX_PARAMS_BYTES {
        return Err("params too large".to_string());
    }
    serde_json::from_str(&text).map_err(|e| format!("params are not JSON: {e}"))
}
