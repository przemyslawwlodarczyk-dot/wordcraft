//! Host mode: a page that embeds WordCraft (the *host*) supplies the document and takes the
//! saves, instead of the browser's file picker and downloads. The desktop and the plain web app
//! are untouched: host mode is on only when the page starts the app with `?host=1`.
//!
//! The host drives the app through functions exported by `wasm-bindgen` (with Trunk they are on
//! `window.wasmBindings`; a page that loads the generated module itself has them as its exports):
//!
//! - `host_ready() -> bool` — the app is up and takes documents.
//! - `host_open(name, bytes: Uint8Array)` — open `bytes` as the document called `name` (the
//!   format comes from the bytes, as for a dropped file). Calls made before the app is up are
//!   kept and applied when it is.
//! - `host_set_save(cb)` — `cb(name, bytes: Uint8Array)` receives every save and export. It may
//!   return a `Promise`: the document counts as saved once it resolves, and stays unsaved when it
//!   rejects (the app shows the rejection's message). A plain return value means "done".
//! - `host_dirty() -> bool` — whether the document has unsaved changes.
//! - `host_execute(id, paramsJson) -> Promise<string>` — run a command by id (the same ids as the
//!   CLI, the control channel and MCP; `document.inspect` for the document's structure) and get
//!   its result as JSON. Commands run on the app's next frame, in order. Rejects with the
//!   command's error message.
//!
//! In host mode the app never opens a *document* from the browser (File › Open, a dropped file):
//! the host owns the document. Pictures, fonts and recipient lists still come from the browser.
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use wasm_bindgen::JsCast as _;
use wasm_bindgen::prelude::wasm_bindgen;
use wordcraft_ui_egui::async_saves::{AsyncSaves, SaveOutcome};
use wordcraft_ui_egui::{Inbox, WordApp};

/// Most bytes one `host_open` accepts (the host's own file limit is far below; this only keeps a
/// runaway page from exhausting wasm memory).
const MAX_OPEN_BYTES: u32 = 256 << 20;
/// Most commands waiting for a frame; further ones are rejected at once.
const MAX_PENDING_COMMANDS: usize = 256;
/// Longest JSON a command may carry (bytes).
const MAX_PARAMS_BYTES: usize = 16 << 20;

/// One `host_execute` call waiting for the app's next frame.
struct PendingCommand {
    id: String,
    params: serde_json::Value,
    resolve: js_sys::Function,
    reject: js_sys::Function,
}

#[derive(Default)]
struct HostState {
    /// Set by [`attach`] once the app is running.
    inbox: Option<Inbox>,
    ctx: Option<egui::Context>,
    saves: Option<AsyncSaves>,
    dirty: Option<Rc<Cell<bool>>>,
    /// The host's save handler.
    save: Option<js_sys::Function>,
    /// `host_open` calls made before [`attach`].
    pending_open: Vec<(String, Vec<u8>)>,
    commands: Vec<PendingCommand>,
}

thread_local! {
    static HOST: RefCell<HostState> = RefCell::new(HostState::default());
}

/// Whether the page asked for host mode (`?host=1` or `?host`).
pub fn enabled() -> bool {
    let query = web_sys::window().and_then(|w| w.location().search().ok()).unwrap_or_default();
    query.trim_start_matches('?').split('&').any(|p| p == "host" || p == "host=1")
}

/// Called by the web shell once the app runs: from now on `host_open` goes straight to the inbox,
/// and documents the host sent earlier are opened.
pub fn attach(inbox: Inbox, ctx: egui::Context, saves: AsyncSaves, dirty: Rc<Cell<bool>>) {
    let pending = HOST.with(|h| {
        let mut h = h.borrow_mut();
        h.inbox = Some(inbox.clone());
        h.ctx = Some(ctx.clone());
        h.saves = Some(saves);
        h.dirty = Some(dirty);
        std::mem::take(&mut h.pending_open)
    });
    if !pending.is_empty() {
        inbox.lock().unwrap_or_else(|e| e.into_inner()).extend(pending);
        ctx.request_repaint();
    }
}

/// The save hook for host mode: hands the bytes to the host's handler. Without a handler the save
/// fails and the document stays unsaved (nothing is downloaded, so nothing is silently lost).
pub fn download(name: &str, bytes: &[u8]) -> Result<(), String> {
    let (cb, saves, ctx) = HOST.with(|h| {
        let h = h.borrow();
        (h.save.clone(), h.saves.clone(), h.ctx.clone())
    });
    let Some(cb) = cb else {
        return Err("the host has no save handler".into());
    };
    let js = |e: wasm_bindgen::JsValue| js_message(&e);
    let array = js_sys::Uint8Array::from(bytes);
    let ret = cb.call2(&wasm_bindgen::JsValue::NULL, &wasm_bindgen::JsValue::from_str(name), &array).map_err(js)?;
    let Ok(promise) = ret.dyn_into::<js_sys::Promise>() else {
        return Ok(());
    };
    let (Some(saves), Some(ctx)) = (saves, ctx) else {
        return Err("host mode is not attached".into());
    };
    // The app holds back a Save Changes prompt's New/Open until this ticket ends (#1315).
    let ticket = saves.start();
    let name = name.to_string();
    wasm_bindgen_futures::spawn_local(async move {
        let outcome = match wasm_bindgen_futures::JsFuture::from(promise).await {
            Ok(_) => {
                log::info!("host saved {name}");
                SaveOutcome::Saved
            }
            Err(e) => {
                let msg = js_message(&e);
                log::error!("host couldn't save {name}: {msg}");
                SaveOutcome::Failed(msg)
            }
        };
        saves.finish(ticket, outcome);
        ctx.request_repaint();
    });
    Ok(())
}

/// Runs the commands the host queued since the last frame. Called by the web shell each frame.
pub fn drain_commands(app: &mut WordApp) {
    let commands = HOST.with(|h| std::mem::take(&mut h.borrow_mut().commands));
    for c in commands {
        match app.execute(&c.id, c.params) {
            Ok(v) => {
                let text = serde_json::to_string(&v).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"));
                let _ = c.resolve.call1(&wasm_bindgen::JsValue::NULL, &wasm_bindgen::JsValue::from_str(&text));
            }
            Err(e) => {
                let _ = c.reject.call1(&wasm_bindgen::JsValue::NULL, &js_sys::Error::new(&e));
            }
        }
    }
}

/// A readable message for a JavaScript error value (an `Error`'s message, else its string form).
fn js_message(e: &wasm_bindgen::JsValue) -> String {
    if let Some(err) = e.dyn_ref::<js_sys::Error>() {
        return String::from(err.message());
    }
    e.as_string().unwrap_or_else(|| format!("{e:?}"))
}

#[wasm_bindgen]
pub fn host_ready() -> bool {
    HOST.with(|h| h.borrow().inbox.is_some())
}

#[wasm_bindgen]
pub fn host_open(name: String, bytes: js_sys::Uint8Array) -> Result<(), wasm_bindgen::JsValue> {
    if bytes.length() > MAX_OPEN_BYTES {
        return Err(js_sys::Error::new("document too large for the browser").into());
    }
    let name = std::path::Path::new(&name).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "document.docx".into());
    let bytes = bytes.to_vec();
    let attached = HOST.with(|h| {
        let h = h.borrow();
        h.inbox.clone().zip(h.ctx.clone())
    });
    match attached {
        Some((inbox, ctx)) => {
            inbox.lock().unwrap_or_else(|e| e.into_inner()).push((name, bytes));
            ctx.request_repaint();
        }
        None => HOST.with(|h| h.borrow_mut().pending_open.push((name, bytes))),
    }
    Ok(())
}

#[wasm_bindgen]
pub fn host_set_save(cb: js_sys::Function) {
    HOST.with(|h| h.borrow_mut().save = Some(cb));
}

#[wasm_bindgen]
pub fn host_dirty() -> bool {
    HOST.with(|h| h.borrow().dirty.as_ref().is_some_and(|d| d.get()))
}

#[wasm_bindgen]
pub fn host_execute(id: String, params_json: String) -> js_sys::Promise {
    js_sys::Promise::new(&mut |resolve, reject| {
        if params_json.len() > MAX_PARAMS_BYTES {
            let _ = reject.call1(&wasm_bindgen::JsValue::NULL, &js_sys::Error::new("params too large"));
            return;
        }
        let params = if params_json.trim().is_empty() {
            serde_json::json!({})
        } else {
            match serde_json::from_str::<serde_json::Value>(&params_json) {
                Ok(v) => v,
                Err(e) => {
                    let _ = reject.call1(&wasm_bindgen::JsValue::NULL, &js_sys::Error::new(&format!("params are not JSON: {e}")));
                    return;
                }
            }
        };
        let queued = HOST.with(|h| {
            let mut h = h.borrow_mut();
            if h.commands.len() >= MAX_PENDING_COMMANDS {
                return false;
            }
            h.commands.push(PendingCommand { id: id.clone(), params, resolve: resolve.clone(), reject: reject.clone() });
            if let Some(ctx) = &h.ctx {
                ctx.request_repaint();
            }
            true
        });
        if !queued {
            let _ = reject.call1(&wasm_bindgen::JsValue::NULL, &js_sys::Error::new("too many commands waiting"));
        }
    })
}
