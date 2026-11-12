//! File I/O that works without paths. In the browser, File › Open is the browser's file picker and
//! Save / Export are downloads; on the desktop, the same calls open native dialogs and run the
//! path-based engine commands (`file.open`, `file.saveAs`, `file.exportTextures`).
//!
//! A browser pick is asynchronous: its bytes wait in an inbox until [`poll`] (called every frame)
//! turns them into the session.

use std::sync::{Mutex, PoisonError};

use texturecraft_engine::Session;
use texturecraft_engine::core::PROJECT_EXT;
use texturecraft_engine::formats::ExportedFile;
use texturecraft_engine::formats::export::sanitize_file_stem;
use texturecraft_engine::formats::mesh_import::MESH_EXTENSIONS;

use crate::TextureCraftApp;
use crate::i18n::{tr, trf};

/// What File › Open accepts: projects and meshes.
pub const OPEN_EXTS: &[&str] = &[PROJECT_EXT, "glb", "gltf", "fbx", "dae", "usd", "usda", "usdc", "usdz", "abc", "obj", "ply", "stl"];

/// Images File › Import resources accepts.
pub const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "tga", "tif", "tiff", "bmp", "webp", "exr", "hdr"];

/// Equirect HDR images Display settings › Import HDR accepts.
pub const ENV_EXTS: &[&str] = &["hdr", "exr"];

/// Materials File › Import material adds as layers: smart materials, MaterialCraft portable
/// materials and MaterialCraft packages.
pub const MATERIAL_EXTS: &[&str] = &["tcmat", "mcmat", "mcraft"];

/// Texture size of a project made from an opened mesh (smaller on the single-threaded web).
const MESH_RESOLUTION: u32 = if cfg!(target_arch = "wasm32") { 1024 } else { 2048 };

/// Texture size the host page asked for with its next mesh (0: [`MESH_RESOLUTION`]).
static HOST_RESOLUTION: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Texture size of the project made from the next opened mesh.
fn mesh_resolution() -> u32 {
    match HOST_RESOLUTION.load(std::sync::atomic::Ordering::Relaxed) {
        0 => MESH_RESOLUTION,
        size => size,
    }
}

/// What a picked file is for.
#[derive(Clone, Copy, PartialEq)]
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
enum Pick {
    /// File > Open / a dropped file: a project, a mesh or an image resource.
    Open,
    /// Display settings > Import HDR.
    Environment,
    /// Bake mode: a high-definition mesh.
    HighPoly,
    /// Bake mode: the cage mesh.
    Cage,
    /// Export dialog › Output templates: an export template (`.json`).
    ExportPreset,
}

/// Files picked in the browser, waiting for the next frame: (file name, bytes, purpose).
static INBOX: Mutex<Vec<(String, Vec<u8>, Pick)>> = Mutex::new(Vec::new());

/// Load files that arrived since the last frame. Call once per frame.
pub fn poll(app: &mut TextureCraftApp) {
    let picked = std::mem::take(&mut *INBOX.lock().unwrap_or_else(PoisonError::into_inner));
    for (name, bytes, pick) in picked {
        let done = match pick {
            Pick::Open => {
                load(app, &name, bytes);
                continue;
            }
            Pick::Environment => app.session.import_resource_bytes(&name, bytes).map(|res| use_environment(app, &res)),
            Pick::HighPoly => app.session.add_high_poly_bytes(&name, bytes).map(|n| app.toast(trf("High poly: {name}", &[("name", &n)]), false)),
            Pick::Cage => app.session.set_cage_bytes(&name, bytes).map(|n| app.toast(trf("Cage: {name}", &[("name", &n)]), false)),
            Pick::ExportPreset => {
                match texturecraft_engine::formats::preset_from_json(&String::from_utf8_lossy(&bytes)) {
                    Ok(p) => crate::export_templates::import(app, &p),
                    Err(e) => app.toast(trf("Couldn't import {name}: {error}", &[("name", &name), ("error", &e)]), true),
                }
                continue;
            }
        };
        if let Err(e) = done {
            app.toast(trf("Couldn't import {name}: {error}", &[("name", &name), ("error", &e)]), true);
        }
    }
}

/// Bake mode: add high-definition meshes (or load the cage) with the native file dialog. The
/// meshes load on a later frame, when the dialog answers.
#[cfg(not(target_arch = "wasm32"))]
pub fn import_bake_mesh(app: &mut TextureCraftApp, _ctx: &egui::Context, cage: bool) {
    use crate::file_dialogs::{Bound, FileDialog};
    let dialog = if cage { FileDialog::open() } else { FileDialog::open_many() };
    app.ask_file(dialog.filter(tr("Meshes"), &MESH_EXTENSIONS), Bound::Project, move |app, paths| {
        for p in paths {
            let cmd = if cage { "bake.cage.set" } else { "bake.highPoly.add" };
            app.run(cmd, serde_json::json!({ "path": p.display().to_string() }));
        }
    });
}

/// Display settings › Import HDR: pick an equirect `.hdr`/`.exr`, import it as a resource and
/// light the scene with it, on a later frame when the dialog answers.
#[cfg(not(target_arch = "wasm32"))]
pub fn import_environment(app: &mut TextureCraftApp, _ctx: &egui::Context) {
    use crate::file_dialogs::{Bound, FileDialog};
    app.ask_file(FileDialog::open().filter(tr("HDR environments"), ENV_EXTS), Bound::Project, |app, paths| {
        let Some(p) = paths.first() else { return };
        if let Some(r) = app.run("resource.import", serde_json::json!({ "path": p.display().to_string() }))
            && let Some(name) = r.get("name").and_then(|n| n.as_str())
        {
            use_environment(app, name);
        }
    });
}

/// Make image resource `name` the environment.
fn use_environment(app: &mut TextureCraftApp, name: &str) {
    let env = format!("{}{name}", texturecraft_engine::core::project::ENV_RESOURCE_PREFIX);
    if app.run("display.set", serde_json::json!({ "environment": env })).is_some() {
        app.toast(trf("Environment: {name}", &[("name", &name)]), false);
    }
}

/// Import an image as a project resource, or replace the session with a `.tcraft` project or a
/// mesh file's bytes, after asking to save unsaved changes (errors become toasts).
pub fn load(app: &mut TextureCraftApp, name: &str, bytes: Vec<u8>) {
    let path = std::path::Path::new(name);
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    if IMAGE_EXTS.contains(&ext.as_str()) {
        match app.session.import_resource_bytes(name, bytes) {
            Ok(res) => {
                app.ui.dock.focus(crate::docking::Panel::Assets);
                app.toast(trf("Imported {name}", &[("name", &res)]), false);
            }
            Err(e) => app.toast(trf("Couldn't import {name}: {error}", &[("name", &name), ("error", &e)]), true),
        }
        return;
    }
    if MATERIAL_EXTS.contains(&ext.as_str()) {
        if let Err(e) = app.session.add_smart_material_bytes(&bytes) {
            app.toast(trf("Couldn't import {name}: {error}", &[("name", &name), ("error", &e)]), true);
        }
        return;
    }
    let bytes = std::sync::Arc::new(bytes);
    if crate::windows::ask_to_save(app, crate::state::Unsaved::Load(name.into(), bytes.clone())) {
        return;
    }
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("Untitled");
    let next = if ext == PROJECT_EXT { Session::from_project_bytes(&bytes) } else { Session::from_mesh_bytes(name, &bytes, mesh_resolution(), stem) };
    match next {
        Ok(next) => {
            app.session.switch_to(next);
            app.toast(trf("Opened {name}", &[("name", &name)]), false);
            #[cfg(target_arch = "wasm32")]
            web::host_apply_textures(app);
        }
        Err(e) => app.toast(trf("Couldn't open {name}: {error}", &[("name", &name), ("error", &e)]), true),
    }
}

/// A file dropped on the window: images import as resources, anything else opens.
pub fn dropped(app: &mut TextureCraftApp, ctx: &egui::Context, file: &egui::DroppedFileHandle) {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = ctx;
        let path = file.path();
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
        let path = path.display().to_string();
        if MATERIAL_EXTS.contains(&ext.as_str()) {
            app.run("material.load", serde_json::json!({ "path": path }));
        } else if IMAGE_EXTS.contains(&ext.as_str()) {
            if let Some(r) = app.run("resource.import", serde_json::json!({ "path": path })) {
                let name = r.get("name").and_then(|n| n.as_str()).unwrap_or_default().to_string();
                app.ui.dock.focus(crate::docking::Panel::Assets);
                app.toast(trf("Imported {name}", &[("name", &name)]), false);
            }
        } else {
            crate::windows::open_path(app, &path);
        }
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = app;
        let (file, ctx) = (file.clone(), ctx.clone());
        wasm_bindgen_futures::spawn_local(async move {
            let name = file.path().display().to_string();
            match file.bytes_async().await {
                Ok(bytes) => INBOX.lock().unwrap_or_else(PoisonError::into_inner).push((name, bytes, Pick::Open)),
                Err(e) => log::warn!("couldn't read dropped file {name}: {e}"),
            }
            ctx.request_repaint();
        });
    }
}

/// The project's download / suggested file name.
pub fn project_file_name(app: &TextureCraftApp) -> String {
    format!("{}.{PROJECT_EXT}", sanitize_file_stem(&app.session.doc.project.name))
}

/// Pack exported textures into one zip archive.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
fn zip_files(files: &[ExportedFile]) -> Result<Vec<u8>, String> {
    use std::io::Write as _;
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated).large_file(true);
    for f in files {
        zip.start_file(f.name.as_str(), opts).map_err(|e| e.to_string())?;
        zip.write_all(&f.bytes).map_err(|e| e.to_string())?;
    }
    Ok(zip.finish().map_err(|e| e.to_string())?.into_inner())
}

#[cfg(target_arch = "wasm32")]
pub use web::{
    download, export_mesh, export_textures, host_bridge_start, host_poll, import_bake_mesh, import_environment, import_images, import_material, import_preset,
    open, save_project, sync_unsaved, unsaved,
};

#[cfg(target_arch = "wasm32")]
mod web {
    use super::*;
    use serde_json::Value;
    use wasm_bindgen::JsCast as _;

    /// Whether the project has unsaved changes, for the page's `beforeunload` listener.
    static UNSAVED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    /// Refresh [`unsaved`] (once per frame).
    pub fn sync_unsaved(app: &TextureCraftApp) {
        UNSAVED.store(app.session.modified, std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether closing or reloading the page would lose unsaved changes.
    pub fn unsaved() -> bool {
        UNSAVED.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// File › Open: the browser's file picker; the file loads on a later frame (see [`poll`]).
    pub fn open(ctx: &egui::Context) {
        pick(ctx, tr("Projects and meshes"), OPEN_EXTS, false, Pick::Open);
    }

    /// File › Import material: pick materials; they add as layers on a later frame.
    pub fn import_material(ctx: &egui::Context) {
        pick(ctx, tr("Materials"), MATERIAL_EXTS, true, Pick::Open);
    }

    /// File › Import resources: pick images; they import on a later frame.
    pub fn import_images(ctx: &egui::Context) {
        pick(ctx, tr("Images"), IMAGE_EXTS, true, Pick::Open);
    }

    /// Display settings › Import HDR: pick an equirect image; it becomes the environment on a
    /// later frame.
    pub fn import_environment(_app: &mut TextureCraftApp, ctx: &egui::Context) {
        pick(ctx, tr("HDR environments"), ENV_EXTS, false, Pick::Environment);
    }

    /// Bake mode: pick high-definition meshes (or the cage); they load on a later frame.
    pub fn import_bake_mesh(_app: &mut TextureCraftApp, ctx: &egui::Context, cage: bool) {
        pick(ctx, tr("Meshes"), &MESH_EXTENSIONS, !cage, if cage { Pick::Cage } else { Pick::HighPoly });
    }

    /// Export dialog › Output templates › Import: pick a template; it is added on a later frame.
    pub fn import_preset(ctx: &egui::Context) {
        pick(ctx, tr("Export templates"), &["json"], false, Pick::ExportPreset);
    }

    fn pick(ctx: &egui::Context, filter: &'static str, exts: &'static [&'static str], multiple: bool, kind: Pick) {
        let ctx = ctx.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let dialog = rfd::AsyncFileDialog::new().add_filter(filter, exts);
            let files = if multiple { dialog.pick_files().await.unwrap_or_default() } else { dialog.pick_file().await.into_iter().collect() };
            for file in files {
                let bytes = file.read().await;
                INBOX.lock().unwrap_or_else(PoisonError::into_inner).push((file.file_name(), bytes, kind));
            }
            ctx.request_repaint();
        });
    }

    /// File › Save: download the project as `<name>.tcraft`.
    pub fn save_project(app: &mut TextureCraftApp) {
        let name = project_file_name(app);
        match app.session.to_project_bytes().map_err(|e| e.to_string()).and_then(|b| download(&name, &b)) {
            Ok(()) => {
                app.session.modified = false;
                app.session.log.push(format!("Saved {name}"));
                app.toast(trf("Downloaded {name}", &[("name", &name)]), false);
            }
            Err(e) => app.toast(trf("Couldn't save {name}: {error}", &[("name", &name), ("error", &e)]), true),
        }
    }

    /// File › Export Mesh: download the working mesh as `ext`: `glb` or `usdz` with its
    /// textures inside, else OBJ.
    pub fn export_mesh(app: &mut TextureCraftApp, ext: &str) {
        let name = format!("{}.{ext}", sanitize_file_stem(&app.session.doc.project.name));
        let bytes = match ext {
            "glb" => app.session.mesh_glb_bytes(),
            "usdz" => app.session.mesh_usd(texturecraft_engine::formats::UsdFormat::Usdz).map(|u| u.bytes),
            _ => Ok(app.session.mesh_obj_bytes()),
        };
        match bytes.map_err(|e| e.to_string()).and_then(|b| download(&name, &b)) {
            Ok(()) => app.toast(trf("Downloaded {name}", &[("name", &name)]), false),
            Err(e) => app.toast(trf("Couldn't export the mesh: {error}", &[("error", &e)]), true),
        }
    }

    /// Export texture sets `sets` with `file.exportTextures` params (minus `dir` and `ts`): one
    /// file downloads as is, several as `<project>_textures.zip`.
    pub fn export_textures(app: &mut TextureCraftApp, params: &Value, sets: &[usize]) {
        match export(app, params, sets) {
            Ok(name) => app.toast(trf("Downloaded {name}", &[("name", &name)]), false),
            Err(e) => app.toast(trf("Couldn't export textures: {error}", &[("error", &e)]), true),
        }
    }

    fn export(app: &mut TextureCraftApp, params: &Value, sets: &[usize]) -> Result<String, String> {
        let opts = texturecraft_engine::file_cmds::export_options(&app.session, params, "file.exportTextures").map_err(|e| e.to_string())?;
        let files = app.session.export_files(sets, &opts).map_err(|e| e.to_string())?;
        app.session.log.extend(files.iter().flat_map(|f| f.warnings.iter().cloned()));
        let name = match files.as_slice() {
            [] => return Err("the preset produced no files".into()),
            [f] => {
                download(&f.name, &f.bytes)?;
                f.name.clone()
            }
            _ => {
                let name = format!("{}_textures.zip", sanitize_file_stem(&app.session.doc.project.name));
                download(&name, &zip_files(&files)?)?;
                name
            }
        };
        app.session.log.push(format!("Exported {} textures ({name})", files.len()));
        Ok(name)
    }

    // The host bridge: a page that embeds the app in an `<iframe>` (same origin) loads a mesh into it and asks for the
    // painted mesh back, through `window.postMessage`. Messages are objects with a `type`:
    //   host -> app   `texturecraft:load`       { name, bytes: ArrayBuffer, resolution?: 256..=4096,
    //                                             textures?: { baseColor?, normal?, roughness?, metallic?: ArrayBuffer } }
    //                 (`textures`: the mesh's own maps as image files, one per channel; they become the bottom fill
    //                 layer "Original texture", projected by UV, because opening a mesh does not read its textures)
    //                 `overlay?: { name?: string, textures: { ...same channels } }`: a second set of maps for the same
    //                 mesh and UVs (say an AI repaint of it). It becomes a fill layer with a white mask between
    //                 "Original texture" and the paint layer, so the host's brush modes can wipe it away and bring it back.
    //   host -> app   `texturecraft:brush`      { mode: "paint" | "hideOverlay" | "showOverlay" }
    //                 (paint: strokes go to the paint layer; hideOverlay / showOverlay: strokes paint the overlay's
    //                 mask black / white. Ignored for the two overlay modes when the load carried no overlay)
    //   host -> app   `texturecraft:overlay`    { opacity: 0..1 }        (how strongly the overlay covers the original)
    //   host -> app   `texturecraft:exportMesh` {}
    //   app  -> host  `texturecraft:ready`      {}                       (once, when the app runs)
    //   app  -> host  `texturecraft:loaded`     { overlay: bool }        (the host's mesh opened; whether it has an overlay layer)
    //   app  -> host  `texturecraft:dirty`      { dirty: bool }          (when unsaved changes appear or go)
    //   app  -> host  `texturecraft:mesh`       { name, bytes: ArrayBuffer }  (the textured GLB; clears dirty)
    //   app  -> host  `texturecraft:error`      { message }
    // Only messages from the parent window at this page's own origin are read; replies go to that origin only.

    /// The host asked for the painted mesh; answered on the next frame.
    static HOST_EXPORT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    /// Whether the bridge runs, and what it last told the host: 0 not started, 1 started, 2 ready sent.
    static HOST_STATE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
    /// The dirty flag the host was last told (2: nothing yet).
    static HOST_DIRTY: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(2);

    const HOST_SIZES: [u32; 5] = [256, 512, 1024, 2048, 4096];
    /// Channels a host may send a map for, by their command ids.
    const HOST_CHANNELS: [&str; 4] = ["baseColor", "normal", "roughness", "metallic"];
    /// The maps that came with the host's last `load`, for the mesh it carries: (channel, image file bytes).
    static HOST_TEXTURES: Mutex<Vec<(&'static str, Vec<u8>)>> = Mutex::new(Vec::new());
    /// The overlay that came with the host's last `load`: (layer name, maps).
    static HOST_OVERLAY: Mutex<Option<(String, Vec<(&'static str, Vec<u8>)>)>> = Mutex::new(None);
    /// The layers the last host load made: the paint layer and the overlay layer (0: none).
    static HOST_PAINT_LAYER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    static HOST_OVERLAY_LAYER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    /// A host load is waiting to open; `texturecraft:loaded` goes out once it has.
    static HOST_LOADING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    /// Brush mode and overlay opacity the host asked for, applied on the next frame (the last of each wins).
    static HOST_BRUSH: Mutex<Option<String>> = Mutex::new(None);
    static HOST_OVERLAY_OPACITY: Mutex<Option<f32>> = Mutex::new(None);
    const OVERLAY_NAME_MAX: usize = 40;

    /// One of the host's map sets as a UV-projected fill layer. `None` when the layer could not be made.
    fn host_fill_layer(app: &mut TextureCraftApp, name: &str, prefix: &str, maps: Vec<(&'static str, Vec<u8>)>) -> Option<u64> {
        let fill = app.run("layer.add.fill", serde_json::json!({ "name": name })).and_then(|v| v.get("layer").and_then(Value::as_u64))?;
        app.run("layer.fill.projection", serde_json::json!({ "layer": fill, "projection": "uv" }));
        for (channel, bytes) in maps {
            match app.session.import_resource_bytes(&format!("{prefix}_{channel}.png"), bytes) {
                Ok(image) => {
                    app.run("layer.fill.set", serde_json::json!({ "layer": fill, "channel": channel, "image": image }));
                }
                Err(e) => host_post("texturecraft:error", &[("message", format!("{channel} map: {e}").into())]),
            }
        }
        Some(fill)
    }

    /// After the host's mesh opened: its own maps as a UV-projected fill layer under the paint layer, and the overlay
    /// (when the load carried one) as a masked fill layer between the two. The project then counts as unmodified
    /// (this is the state the host gave us).
    pub(super) fn host_apply_textures(app: &mut TextureCraftApp) {
        use std::sync::atomic::Ordering::Relaxed;
        let maps = std::mem::take(&mut *HOST_TEXTURES.lock().unwrap_or_else(PoisonError::into_inner));
        let overlay = HOST_OVERLAY.lock().unwrap_or_else(PoisonError::into_inner).take();
        if !HOST_LOADING.swap(false, Relaxed) {
            return;
        }
        let ts = app.session.ts();
        let paint = app.session.selected.get(&ts).copied();
        HOST_PAINT_LAYER.store(paint.map_or(0, |id| id.0), Relaxed);
        HOST_OVERLAY_LAYER.store(0, Relaxed);
        let mut bottom = 0;
        if !maps.is_empty() {
            if let Some(fill) = host_fill_layer(app, "Original texture", "original", maps) {
                app.run("layer.move", serde_json::json!({ "layer": fill, "index": 0 }));
                bottom = 1;
            }
        }
        if let Some((name, maps)) = overlay.filter(|(_, maps)| !maps.is_empty()) {
            if let Some(fill) = host_fill_layer(app, &name, "overlay", maps) {
                app.run("layer.move", serde_json::json!({ "layer": fill, "index": bottom }));
                if app.run("mask.add.white", serde_json::json!({ "layer": fill })).is_some() {
                    HOST_OVERLAY_LAYER.store(fill, Relaxed);
                }
            }
        }
        if let Some(paint) = paint {
            app.run("layer.select", serde_json::json!({ "layer": paint.0 }));
        }
        // What the host loaded is the starting point: nothing to undo back past it.
        app.session.modified = false;
        host_post("texturecraft:loaded", &[("overlay", (HOST_OVERLAY_LAYER.load(Relaxed) != 0).into())]);
    }

    /// The brush mode the host asked for: where strokes go.
    fn host_set_brush(app: &mut TextureCraftApp, mode: &str) {
        use std::sync::atomic::Ordering::Relaxed;
        let (paint, overlay) = (HOST_PAINT_LAYER.load(Relaxed), HOST_OVERLAY_LAYER.load(Relaxed));
        let modified = app.session.modified;
        match mode {
            "paint" if paint != 0 => {
                app.run("layer.select", serde_json::json!({ "layer": paint, "target": "content" }));
                app.run("tool.paint", Value::Null);
            }
            "hideOverlay" | "showOverlay" if overlay != 0 => {
                if app.run("layer.select", serde_json::json!({ "layer": overlay, "target": "mask" })).is_some() {
                    app.run("tool.paint", Value::Null);
                    app.run("tool.material", serde_json::json!({ "maskValue": if mode == "showOverlay" { 1.0 } else { 0.0 } }));
                }
            }
            _ => {}
        }
        // Choosing where to paint changes nothing in the project.
        app.session.modified = modified;
    }

    fn host_origin() -> Option<String> {
        web_sys::window()?.location().origin().ok()
    }

    /// The parent window, when this page is framed.
    fn host_window() -> Option<web_sys::Window> {
        let window = web_sys::window()?;
        let parent = window.parent().ok()??;
        (!js_sys::Object::is(&parent, &window)).then_some(parent)
    }

    fn host_post(kind: &str, fields: &[(&str, wasm_bindgen::JsValue)]) {
        let (Some(parent), Some(origin)) = (host_window(), host_origin()) else { return };
        let message = js_sys::Object::new();
        js_sys::Reflect::set(&message, &"type".into(), &kind.into()).ok();
        for (key, value) in fields {
            js_sys::Reflect::set(&message, &(*key).into(), value).ok();
        }
        if let Err(e) = parent.post_message(&message, &origin) {
            log::warn!("couldn't post {kind} to the host page: {e:?}");
        }
    }

    /// Listen for the host page's messages (call once, only when the app is embedded).
    pub fn host_bridge_start(ctx: &egui::Context) {
        use std::sync::atomic::Ordering::Relaxed;
        let (Some(window), Some(parent), Some(origin)) = (web_sys::window(), host_window(), host_origin()) else { return };
        if HOST_STATE.swap(1, Relaxed) != 0 {
            return;
        }
        let ctx = ctx.clone();
        let listener = wasm_bindgen::closure::Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |event: web_sys::MessageEvent| {
            if event.origin() != origin || !event.source().is_some_and(|source| js_sys::Object::is(&source, &parent)) {
                return;
            }
            let data = event.data();
            let field = |key: &str| js_sys::Reflect::get(&data, &key.into()).unwrap_or(wasm_bindgen::JsValue::UNDEFINED);
            match field("type").as_string().as_deref() {
                Some("texturecraft:load") => {
                    let (Some(name), Ok(buffer)) = (field("name").as_string(), field("bytes").dyn_into::<js_sys::ArrayBuffer>()) else {
                        host_post("texturecraft:error", &[("message", "load needs a name and an ArrayBuffer of bytes".into())]);
                        return;
                    };
                    // Only the file name: the host's string never becomes a path.
                    let name = name.rsplit(['/', '\\']).next().unwrap_or_default().to_string();
                    let size = field("resolution").as_f64().map(|v| v as u32).filter(|v| HOST_SIZES.contains(v)).unwrap_or(0);
                    HOST_RESOLUTION.store(size, Relaxed);
                    let textures = field("textures");
                    let maps = HOST_CHANNELS
                        .iter()
                        .filter_map(|channel| {
                            let buffer = js_sys::Reflect::get(&textures, &(*channel).into()).ok()?.dyn_into::<js_sys::ArrayBuffer>().ok()?;
                            Some((*channel, js_sys::Uint8Array::new(&buffer).to_vec()))
                        })
                        .collect();
                    *HOST_TEXTURES.lock().unwrap_or_else(PoisonError::into_inner) = maps;
                    let overlay = field("overlay");
                    let overlay_maps: Vec<_> = js_sys::Reflect::get(&overlay, &"textures".into())
                        .ok()
                        .map(|textures| {
                            HOST_CHANNELS
                                .iter()
                                .filter_map(|channel| {
                                    let buffer = js_sys::Reflect::get(&textures, &(*channel).into()).ok()?.dyn_into::<js_sys::ArrayBuffer>().ok()?;
                                    Some((*channel, js_sys::Uint8Array::new(&buffer).to_vec()))
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    let overlay_name = js_sys::Reflect::get(&overlay, &"name".into()).ok().and_then(|v| v.as_string()).unwrap_or_default();
                    let overlay_name: String = overlay_name.trim().chars().filter(|c| !c.is_control()).take(OVERLAY_NAME_MAX).collect();
                    let overlay_name = if overlay_name.is_empty() { "Overlay".to_string() } else { overlay_name };
                    *HOST_OVERLAY.lock().unwrap_or_else(PoisonError::into_inner) = (!overlay_maps.is_empty()).then_some((overlay_name, overlay_maps));
                    HOST_LOADING.store(true, Relaxed);
                    INBOX.lock().unwrap_or_else(PoisonError::into_inner).push((name, js_sys::Uint8Array::new(&buffer).to_vec(), Pick::Open));
                }
                Some("texturecraft:exportMesh") => HOST_EXPORT.store(true, Relaxed),
                Some("texturecraft:brush") => {
                    let Some(mode) = field("mode").as_string().filter(|m| ["paint", "hideOverlay", "showOverlay"].contains(&m.as_str())) else {
                        host_post("texturecraft:error", &[("message", "brush needs a mode: paint, hideOverlay or showOverlay".into())]);
                        return;
                    };
                    *HOST_BRUSH.lock().unwrap_or_else(PoisonError::into_inner) = Some(mode);
                }
                Some("texturecraft:overlay") => {
                    let Some(opacity) = field("opacity").as_f64().filter(|v| v.is_finite()) else {
                        host_post("texturecraft:error", &[("message", "overlay needs an opacity from 0 to 1".into())]);
                        return;
                    };
                    *HOST_OVERLAY_OPACITY.lock().unwrap_or_else(PoisonError::into_inner) = Some((opacity as f32).clamp(0.0, 1.0));
                }
                _ => return,
            }
            ctx.request_repaint();
        });
        if window.add_event_listener_with_callback("message", listener.as_ref().unchecked_ref()).is_ok() {
            listener.forget(); // lives as long as the page
        }
    }

    /// The bridge's per-frame half: tell the host the app is ready and whether it has unsaved changes, and answer a
    /// pending export. Does nothing until [`host_bridge_start`] ran.
    pub fn host_poll(app: &mut TextureCraftApp) {
        use std::sync::atomic::Ordering::Relaxed;
        match HOST_STATE.load(Relaxed) {
            0 => return,
            1 => {
                HOST_STATE.store(2, Relaxed);
                host_post("texturecraft:ready", &[]);
            }
            _ => {}
        }
        if let Some(mode) = HOST_BRUSH.lock().unwrap_or_else(PoisonError::into_inner).take() {
            host_set_brush(app, &mode);
        }
        if let Some(opacity) = HOST_OVERLAY_OPACITY.lock().unwrap_or_else(PoisonError::into_inner).take() {
            let overlay = HOST_OVERLAY_LAYER.load(Relaxed);
            if overlay != 0 {
                app.run("layer.blend", serde_json::json!({ "layer": overlay, "channel": "all", "opacity": opacity }));
            }
        }
        if HOST_EXPORT.swap(false, Relaxed) {
            let name = format!("{}.glb", sanitize_file_stem(&app.session.doc.project.name));
            match app.session.mesh_glb_bytes() {
                Ok(bytes) => {
                    app.session.modified = false;
                    app.session.log.push(format!("Sent {name} to the host page"));
                    host_post("texturecraft:mesh", &[("name", name.as_str().into()), ("bytes", js_sys::Uint8Array::from(bytes.as_slice()).buffer().into())]);
                }
                Err(e) => host_post("texturecraft:error", &[("message", e.to_string().into())]),
            }
        }
        let dirty = u8::from(app.session.modified);
        if HOST_DIRTY.swap(dirty, Relaxed) != dirty {
            host_post("texturecraft:dirty", &[("dirty", (dirty == 1).into())]);
        }
    }

    /// Start a browser download of `bytes` named `name` (Blob + a temporary `<a download>`).
    pub fn download(name: &str, bytes: &[u8]) -> Result<(), String> {
        let js = |e: wasm_bindgen::JsValue| format!("{e:?}");
        let window = web_sys::window().ok_or("no window")?;
        let document = window.document().ok_or("no document")?;
        let parts = js_sys::Array::of1(&js_sys::Uint8Array::from(bytes));
        let opts = web_sys::BlobPropertyBag::new();
        opts.set_type(if name.ends_with(".zip") { "application/zip" } else { "application/octet-stream" });
        let blob = web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &opts).map_err(js)?;
        let url = web_sys::Url::create_object_url_with_blob(&blob).map_err(js)?;
        let a: web_sys::HtmlAnchorElement = document.create_element("a").map_err(js)?.dyn_into().map_err(|_| "not an anchor")?;
        a.set_href(&url);
        a.set_download(name);
        a.style().set_property("display", "none").map_err(js)?;
        document.body().ok_or("no body")?.append_child(&a).map_err(js)?;
        a.click();
        a.remove();
        // Revoke once the click has been dispatched; the download keeps its own reference.
        let revoke = wasm_bindgen::closure::Closure::once_into_js(move || {
            web_sys::Url::revoke_object_url(&url).ok();
        });
        window.set_timeout_with_callback_and_timeout_and_arguments_0(revoke.unchecked_ref(), 10_000).map_err(js)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zip_round_trips_every_export_file() {
        let file = |name: &str, bytes: &[u8]| ExportedFile { name: name.into(), bytes: bytes.to_vec(), width: 1, height: 1, warnings: Vec::new() };
        let files = [file("a_BaseColor.png", b"one"), file("a_Normal.png", &[7u8; 4096])];
        let bytes = zip_files(&files).unwrap();
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        assert_eq!(zip.len(), 2);
        for f in &files {
            let mut out = Vec::new();
            std::io::Read::read_to_end(&mut zip.by_name(&f.name).unwrap(), &mut out).unwrap();
            assert_eq!(out, f.bytes);
        }
    }
}
