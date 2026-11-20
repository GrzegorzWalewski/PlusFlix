//! PlusFlix – a Stremio-backed player. UI glue: Slint on the main thread, everything
//! else (network, failover sessions, mpv) on a tokio runtime.

// Release builds show only the graphic window; debug builds keep the console so
// logs remain visible during development.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

slint::include_modules!();

use plusflix_core::addon::{Addon, AddonClient};
use plusflix_core::genres::{self, CatalogRef, GenreBrowser};
use plusflix_core::home;
use plusflix_core::lang::Lang;
use plusflix_core::loader::SourceLoader;
use plusflix_core::model::{Meta, MetaPreview};
use plusflix_core::mpv::{MpvConfig, MpvPlayer};
use plusflix_core::player::{LoadRequest, Player, PlayerEvent};
use plusflix_core::pool::{PoolStats, SourcePool};
use plusflix_core::session::{self, Outcome, SessionConfig, SessionEvent};
use plusflix_core::settings::{LanguageMode, PlayerKind, Quality, Settings, SubtitleRequirement};
use plusflix_core::store::{
    self, Favorite, Profile, ProgressCard, Store, MAX_PROFILES, PROFILE_COLORS, PROFILE_ICONS,
};
use plusflix_core::stremio::StremioApi;
use plusflix_core::vlc::{VlcConfig, VlcPlayer};
use plusflix_core::Translations;
use slint::{
    ComponentHandle, Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel, Weak,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering::Relaxed, Ordering::SeqCst};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::runtime::Handle;
use tokio::sync::Semaphore;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------- dark title bar

/// Apply a dark title bar and the app icon to the main window (Windows only).
/// Best-effort: the window is created by winit when `run()` is called, so a
/// background thread polls for it and sets `DWMWA_USE_IMMERSIVE_DARK_MODE`
/// plus `WM_SETICON` (from the exe's icon resource) once it exists.
#[cfg(target_family = "windows")]
fn enable_dark_title_bar() {
    use std::os::raw::c_void;
    use std::ptr::null;

    extern "system" {
        fn EnumWindows(
            cb: Option<unsafe extern "system" fn(isize, isize) -> i32>,
            lp: isize,
        ) -> i32;
        fn GetWindowThreadProcessId(hwnd: isize, pid: *mut u32) -> u32;
        fn GetWindowTextLengthW(hwnd: isize) -> i32;
        fn IsWindowVisible(hwnd: isize) -> i32;
        fn DwmSetWindowAttribute(hwnd: isize, attr: u32, value: *const c_void, size: u32) -> i32;
        fn GetModuleHandleW(name: *const u16) -> isize;
        fn LoadIconW(instance: isize, name: *const u16) -> isize;
        fn SendMessageW(hwnd: isize, msg: u32, wparam: usize, lparam: isize) -> isize;
    }

    static TARGET_PID: AtomicU32 = AtomicU32::new(0);
    static FOUND_HWND: AtomicIsize = AtomicIsize::new(0);

    unsafe extern "system" fn enum_cb(hwnd: isize, _lp: isize) -> i32 {
        let mut wpid = 0u32;
        GetWindowThreadProcessId(hwnd, &mut wpid);
        if wpid == TARGET_PID.load(Relaxed)
            && IsWindowVisible(hwnd) != 0
            && GetWindowTextLengthW(hwnd) > 0
        {
            FOUND_HWND.store(hwnd, Relaxed);
            return 0;
        }
        1
    }

    TARGET_PID.store(std::process::id(), Relaxed);
    std::thread::spawn(|| {
        for _ in 0..100 {
            unsafe {
                EnumWindows(Some(enum_cb), 0);
            }
            let hwnd = FOUND_HWND.load(Relaxed);
            if hwnd != 0 {
                let on: i32 = 1; // DWMWA_USE_IMMERSIVE_DARK_MODE = 20
                unsafe {
                    DwmSetWindowAttribute(hwnd, 20, &on as *const i32 as *const c_void, 4);
                }
                // Title bar + taskbar icon: load the first icon resource from
                // the exe (the one embedded by winresource in build.rs) and
                // send WM_SETICON (small = 0, big = 1).
                unsafe {
                    let hinst = GetModuleHandleW(null());
                    if hinst != 0 {
                        let icon = LoadIconW(hinst, 1 as *const u16);
                        if icon != 0 {
                            SendMessageW(hwnd, 0x0080 /* WM_SETICON */, 0 /* ICON_SMALL */, icon);
                            SendMessageW(hwnd, 0x0080, 1 /* ICON_BIG */, icon);
                        }
                    }
                }
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    });
}

// ---------------------------------------------------------------- shared state

struct Decoded {
    rgba: Vec<u8>,
    w: u32,
    h: u32,
}

impl Decoded {
    fn to_image(&self) -> Image {
        Image::from_rgba8(SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
            &self.rgba, self.w, self.h,
        ))
    }
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Sources being collected for one video (movie or episode). Dropping it stops the loading.
struct Sources {
    video_id: String,
    pool: SourcePool,
    _loader: SourceLoader,
}

struct Detail {
    kind: String,
    meta: Meta,
    current: Mutex<Option<Arc<Sources>>>,
    stats: Mutex<Option<AbortOnDrop>>,
}

/// Cached per-item source resolution for the play pill on catalog cards:
/// Loading (gray), Ready (green — playable sources found), None (red — all
/// addons finished with no playable source).
#[derive(Clone, Copy, PartialEq, Eq)]
enum SourceStatus {
    Loading,
    Ready,
    None,
}

struct App {
    rt: Handle,
    ui: Weak<MainWindow>,
    store_path: PathBuf,
    store: Mutex<Store>,
    /// Loaded translations for the current interface language.
    translations: Mutex<Translations>,
    /// Set while `apply_language` syncs the language dropdown index
    /// programmatically, so the `language-changed` callback is ignored.
    syncing_language_selector: AtomicBool,
    api: StremioApi,
    client: AddonClient,
    addons: Mutex<Vec<Addon>>,
    profile: Mutex<Option<String>>,
    detail: Mutex<Option<Arc<Detail>>>,
    playing: AtomicBool,
    /// Set by the Cancel button while a playback task is running; the task
    /// watches it and tears the player down.
    cancel_playback: Mutex<Option<Arc<AtomicBool>>>,
    /// Set by the "Next source" button; makes the session fail over to the
    /// next source at once (reported to it as a `Skipped` event, not a
    /// failure).
    skip_playback: Mutex<Option<Arc<AtomicBool>>>,
    /// Key of the source that last actually played, per video id, so the
    /// next play of the same item starts from the same source.
    last_played: Mutex<HashMap<String, String>>,
    posters: Mutex<HashMap<String, Arc<Decoded>>>,
    poster_slots: Semaphore,
    /// Episode ids seen for each series (used for the "all episodes watched" badge).
    known_episodes: Mutex<HashMap<String, Vec<String>>>,
    /// Cached source resolution per item id (play pill colours).
    source_states: Mutex<HashMap<String, SourceStatus>>,
    /// Last rendered home/movies/series rows as (title, addon subtitle, items),
    /// so the account sync can patch the Continue Watching row without
    /// re-fetching catalogs.
    rows_cache: Mutex<Vec<(String, String, Vec<ItemData>)>>,
    /// Bumped on every page load; background catalog tasks only patch the UI
    /// while their generation is current.
    load_gen: std::sync::atomic::AtomicUsize,
    /// Active genre browse (Movies/Series), if any.
    genre: Mutex<Option<GenreState>>,
    /// Bumped on every new genre selection; page fetches compare it against
    /// the state's token and are dropped when the selection was replaced.
    genre_gen: std::sync::atomic::AtomicUsize,
    /// True while a genre page fetch is running; load-more triggers that
    /// arrive meanwhile set `genre_pending` instead.
    genre_fetching: AtomicBool,
    genre_pending: AtomicBool,
}

/// Plain-data twin of the Slint `Item` (Slint images are not `Send`).
#[derive(Clone)]
struct ItemData {
    id: String,
    kind: String,
    name: String,
    year: String,
    poster_url: Option<String>,
    /// Landscape art (backdrop / episode thumbnail) for continue-watching cards.
    landscape_url: Option<String>,
    /// Series/movie id a continue-watching card opens (empty otherwise).
    title_id: String,
    /// "S02E05" on continue-watching cards.
    episode_label: String,
    /// "42 min left" on continue-watching cards.
    time_left: String,
    /// Watch progress 0..1 (0 = no bar).
    progress: f32,
    /// Duration known, so the progress bar may be shown.
    has_progress: bool,
    watched: bool,
    fav: bool,
    is_cw: bool,
    /// Cached source resolution for the play pill (see `SourceStatus`).
    source_state: SourceStatus,
}

impl ItemData {
    fn from_preview(m: &MetaPreview) -> Self {
        Self {
            id: m.id.clone(),
            kind: m.kind.clone(),
            name: m.name.clone(),
            year: m.release_info.clone().unwrap_or_default(),
            poster_url: m.poster.clone(),
            landscape_url: None,
            title_id: String::new(),
            episode_label: String::new(),
            time_left: String::new(),
            progress: 0.0,
            has_progress: false,
            watched: false,
            fav: false,
            is_cw: false,
            source_state: SourceStatus::Loading,
        }
    }
    fn from_favorite(f: &Favorite) -> Self {
        Self {
            id: f.id.clone(),
            kind: f.kind.clone(),
            name: f.name.clone(),
            year: String::new(),
            poster_url: f.poster.clone(),
            landscape_url: None,
            title_id: String::new(),
            episode_label: String::new(),
            time_left: String::new(),
            progress: 0.0,
            has_progress: false,
            watched: false,
            fav: false,
            is_cw: false,
            source_state: SourceStatus::Loading,
        }
    }
    /// A Continue Watching card from a progress entry (must have card info).
    fn from_progress(pr: &plusflix_core::store::Progress) -> Option<Self> {
        let card = pr.card.as_ref()?;
        let video_id = card
            .episode_id
            .clone()
            .unwrap_or_else(|| card.title_id.clone());
        let frac = pr
            .duration_secs
            .filter(|d| *d > 0.0)
            .map(|d| (pr.position_secs / d) as f32)
            .unwrap_or(0.0);
        // "42 min left" (or hours + minutes for long entries).
        let time_left = pr.duration_secs.and_then(|d| {
            let mins = ((d - pr.position_secs) / 60.0).ceil();
            (mins > 0.0).then(|| {
                if mins >= 60.0 {
                    format!(
                        "{} h {} min left",
                        (mins / 60.0) as u64,
                        (mins % 60.0) as u64
                    )
                } else {
                    format!("{mins} min left")
                }
            })
        });
        Some(Self {
            id: video_id,
            kind: card.kind.clone(),
            name: card.title.clone(),
            year: String::new(),
            poster_url: card.poster.clone(),
            landscape_url: card.background.clone(),
            title_id: card.title_id.clone(),
            episode_label: card.episode_label.clone().unwrap_or_default(),
            time_left: time_left.unwrap_or_default(),
            progress: frac,
            has_progress: pr.duration_secs.is_some_and(|d| d > 0.0),
            watched: false,
            fav: false,
            is_cw: true,
            source_state: SourceStatus::Loading,
        })
    }
    fn to_slint(&self) -> Item {
        Item {
            id: self.id.clone().into(),
            kind: self.kind.clone().into(),
            name: self.name.clone().into(),
            year: self.year.clone().into(),
            title_id: self.title_id.clone().into(),
            episode_label: self.episode_label.clone().into(),
            time_left: self.time_left.clone().into(),
            progress: self.progress,
            has_progress: self.has_progress,
            watched: self.watched,
            fav: self.fav,
            is_cw: self.is_cw,
            poster_url: self.poster_url.clone().unwrap_or_default().into(),
            source_state: self.source_state as i32,
            ..Default::default()
        }
    }
}

/// One active genre browse (Movies or Series with a chosen genre): the pager
/// plus the flat item list the UI grid re-rows.
struct GenreState {
    kind: String,
    genre: String,
    /// Every catalog serving this genre; the chips narrow it down.
    catalogs: Vec<CatalogRef>,
    browser: GenreBrowser,
    items: Vec<ItemData>,
    /// False once every selected catalog is exhausted.
    has_more: bool,
    /// `genre_gen` value at creation: page fetches check it to drop results
    /// that belong to a replaced selection.
    token: usize,
}

fn items_model(items: &[ItemData]) -> ModelRc<Item> {
    ModelRc::new(VecModel::from(
        items.iter().map(ItemData::to_slint).collect::<Vec<_>>(),
    ))
}

/// Builds row models, filling in posters that were already decoded (and thus
/// live in `app.posters`). Catalog rows are re-rendered from plain `ItemData`
/// on every catalog completion, which would otherwise wipe posters patched
/// into the previous model by `apply_poster`.
fn rows_with_posters(
    snapshot: &[(String, String, Vec<ItemData>)],
    posters: &HashMap<String, Arc<Decoded>>,
) -> Vec<RowData> {
    snapshot
        .iter()
        .map(|(title, subtitle, items)| RowData {
            title: title.clone().into(),
            subtitle: subtitle.clone().into(),
            items: ModelRc::new(VecModel::from(
                items
                    .iter()
                    .map(|d| {
                        let mut it = d.to_slint();
                        if let Some(url) = &d.poster_url {
                            if let Some(dec) = posters.get(url) {
                                it.poster = dec.to_image();
                                it.has_poster = true;
                            }
                        }
                        if let Some(url) = &d.landscape_url {
                            if let Some(dec) = posters.get(url) {
                                it.landscape = dec.to_image();
                                it.has_landscape = true;
                            }
                        }
                        it
                    })
                    .collect::<Vec<_>>(),
            )),
        })
        .collect()
}

fn addon_rows(addons: &[Addon], waits: &[String]) -> Vec<AddonData> {
    addons
        .iter()
        .map(|a| {
            let m = &a.manifest;
            AddonData {
                name: a.name().to_owned().into(),
                kinds: m.types.join(", ").into(),
                version: m.version.clone().into(),
                // "Wait for this addon" switch state (persisted settings).
                waited: waits.iter().any(|w| w == a.name()).into(),
            }
        })
        .collect()
}

/// (watched, favorite) badges for one item, judged by the active profile.
fn item_flags(app: &App, id: &str, kind: &str) -> (bool, bool) {
    let Some(pid) = app.profile_id() else {
        return (false, false);
    };
    let st = lock(&app.store);
    let Ok(p) = st.profile(&pid) else {
        return (false, false);
    };
    let fav = p.is_favorite(id);
    let watched = if kind == "series" {
        lock(&app.known_episodes)
            .get(id)
            .filter(|eps| !eps.is_empty())
            .is_some_and(|eps| eps.iter().all(|e| p.is_watched(e)))
    } else {
        p.is_watched(id)
    };
    (watched, fav)
}

fn apply_flags(app: &App, items: &mut [ItemData]) {
    for it in items.iter_mut() {
        let (watched, fav) = item_flags(app, &it.id, &it.kind);
        it.watched = watched;
        it.fav = fav;
        it.source_state = lock(&app.source_states)
            .get(&it.id)
            .copied()
            .unwrap_or(SourceStatus::Loading);
    }
}

/// MPV or VLC, chosen per playback and remembered in the profile settings.
enum AnyPlayer {
    Mpv(Box<MpvPlayer>),
    Vlc(VlcPlayer),
}

impl Player for AnyPlayer {
    async fn load(&mut self, req: LoadRequest) -> plusflix_core::Result<()> {
        match self {
            AnyPlayer::Mpv(p) => p.load(req).await,
            AnyPlayer::Vlc(p) => p.load(req).await,
        }
    }

    async fn next_event(&mut self) -> Option<PlayerEvent> {
        match self {
            AnyPlayer::Mpv(p) => p.next_event().await,
            AnyPlayer::Vlc(p) => p.next_event().await,
        }
    }

    async fn notify(&mut self, text: &str) {
        match self {
            AnyPlayer::Mpv(p) => p.notify(text).await,
            AnyPlayer::Vlc(p) => p.notify(text).await,
        }
    }

    async fn stop(&mut self) {
        match self {
            AnyPlayer::Mpv(p) => p.stop().await,
            AnyPlayer::Vlc(p) => p.stop().await,
        }
    }
}

impl App {
    fn ui(&self, f: impl FnOnce(&MainWindow) + Send + 'static) {
        let _ = self.ui.upgrade_in_event_loop(move |ui| f(&ui));
    }

    fn save(&self) {
        if let Err(e) = lock(&self.store).save(&self.store_path) {
            tracing::warn!("could not save store: {e}");
        }
    }

    fn profile_id(&self) -> Option<String> {
        lock(&self.profile).clone()
    }

    fn profile_settings(&self) -> Settings {
        let id = self.profile_id();
        let st = lock(&self.store);
        id.and_then(|id| st.profile(&id).ok().map(|p| p.settings.clone()))
            .unwrap_or_default()
    }

    fn flash(self: &Arc<Self>, text: impl Into<String>) {
        let text: String = text.into();
        let shown = text.clone();
        self.ui(move |ui| ui.set_status(shown.into()));
        let app = self.clone();
        self.rt.spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            app.ui(move |ui| {
                if ui.get_status().as_str() == text {
                    ui.set_status("".into());
                }
            });
        });
    }

    async fn poster(&self, url: &str) -> Option<Arc<Decoded>> {
        if let Some(d) = lock(&self.posters).get(url) {
            return Some(d.clone());
        }
        let _slot = self.poster_slots.acquire().await.ok()?;
        let bytes = self
            .client
            .http()
            .get(url)
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?
            .bytes()
            .await
            .ok()?;
        let decoded = tokio::task::spawn_blocking(move || {
            let img = image::load_from_memory(&bytes)
                .ok()?
                .thumbnail(300, 450)
                .to_rgba8();
            let (w, h) = (img.width(), img.height());
            Some(Decoded {
                rgba: img.into_raw(),
                w,
                h,
            })
        })
        .await
        .ok()??;
        let decoded = Arc::new(decoded);
        lock(&self.posters).insert(url.to_owned(), decoded.clone());
        Some(decoded)
    }

    fn fetch_posters(self: &Arc<Self>, wanted: Vec<(String, String, bool)>) {
        for (id, url, landscape) in wanted {
            let app = self.clone();
            self.rt.spawn(async move {
                if let Some(dec) = app.poster(&url).await {
                    app.ui(move |ui| apply_poster(ui, &id, &dec, landscape));
                }
            });
        }
    }

    /// Same as `fetch_posters` but patches episode thumbnails in the detail popup.
    fn fetch_episode_thumbs(self: &Arc<Self>, wanted: Vec<(String, String)>) {
        for (id, url) in wanted {
            let app = self.clone();
            self.rt.spawn(async move {
                if let Some(dec) = app.poster(&url).await {
                    app.ui(move |ui| patch_episode_thumb(ui, &id, &dec));
                }
            });
        }
    }
}

fn patch_items(items: &ModelRc<Item>, id: &str, img: &Image, landscape: bool) {
    for i in 0..items.row_count() {
        if let Some(mut it) = items.row_data(i) {
            if it.id.as_str() == id {
                if landscape {
                    it.landscape = img.clone();
                    it.has_landscape = true;
                } else {
                    it.poster = img.clone();
                    it.has_poster = true;
                }
                items.set_row_data(i, it);
            }
        }
    }
}

fn apply_poster(ui: &MainWindow, id: &str, dec: &Decoded, landscape: bool) {
    let img = dec.to_image();
    let rows = ui.get_rows();
    for r in 0..rows.row_count() {
        if let Some(row) = rows.row_data(r) {
            patch_items(&row.items, id, &img, landscape);
        }
    }
    patch_items(&ui.get_favorites(), id, &img, landscape);
    patch_items(&ui.get_search_results(), id, &img, landscape);
    patch_items(&ui.get_genre_items(), id, &img, landscape);
    if !landscape && ui.get_detail_id().as_str() == id {
        ui.set_detail_poster(img);
        ui.set_detail_has_poster(true);
    }
}

/// Image URLs to fetch: (item id, url, is landscape art).
fn poster_wanted(rows: &[(String, Vec<ItemData>)]) -> Vec<(String, String, bool)> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for (_, items) in rows {
        for it in items {
            if let Some(url) = &it.poster_url {
                if seen.insert((it.id.clone(), false)) {
                    out.push((it.id.clone(), url.clone(), false));
                }
            }
            if let Some(url) = &it.landscape_url {
                if seen.insert((it.id.clone(), true)) {
                    out.push((it.id.clone(), url.clone(), true));
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------- screens

fn hex_color(hex: &str) -> slint::Color {
    let h = hex.trim_start_matches('#');
    if h.len() == 6 {
        if let Ok(v) = u32::from_str_radix(h, 16) {
            return slint::Color::from_rgb_u8((v >> 16) as u8, (v >> 8) as u8, v as u8);
        }
    }
    slint::Color::from_rgb_u8(110, 110, 120)
}

fn show_profiles(app: &Arc<App>) {
    tracing::info!("show_profiles");
    let (profiles, can_add) = {
        let st = lock(&app.store);
        (
            st.profiles
                .iter()
                .map(|p| {
                    (
                        p.id.clone(),
                        p.name.clone(),
                        p.icon.clone(),
                        p.color.clone(),
                    )
                })
                .collect::<Vec<_>>(),
            st.profiles.len() < MAX_PROFILES,
        )
    };
    *lock(&app.profile) = None;
    *lock(&app.detail) = None;
    *lock(&app.genre) = None;
    app.ui(move |ui| {
        let model: Vec<ProfileData> = profiles
            .into_iter()
            .map(|(id, name, icon, color)| ProfileData {
                id: id.into(),
                name: name.into(),
                icon: icon.into(),
                color: hex_color(&color),
            })
            .collect();
        ui.set_profiles(ModelRc::new(VecModel::from(model)));
        ui.set_can_add_profile(can_add);
        ui.set_busy(false);
        ui.set_detail_open(false);
        ui.set_playing(false);
        ui.set_manage_profiles(false);
        ui.set_edit_open(false);
        // Any genre browse is off once we leave the main screen.
        ui.set_movies_genre("".into());
        ui.set_series_genre("".into());
        ui.set_movies_genre_t("".into());
        ui.set_series_genre_t("".into());
        ui.set_genre_catalogs(ModelRc::default());
        ui.set_genre_catalog_idx(0);
        ui.set_genre_items(ModelRc::default());
        ui.set_genre_has_more(false);
        ui.set_genre_busy(false);
        ui.set_genre_loading(false);
        ui.set_icon_choices(ModelRc::new(VecModel::from(
            PROFILE_ICONS
                .iter()
                .map(|i| (*i).into())
                .collect::<Vec<_>>(),
        )));
        ui.set_color_choices(ModelRc::new(VecModel::from(
            PROFILE_COLORS
                .iter()
                .map(|c| (*c).into())
                .collect::<Vec<_>>(),
        )));
        ui.set_color_values(ModelRc::new(VecModel::from(
            PROFILE_COLORS
                .iter()
                .map(|c| hex_color(c))
                .collect::<Vec<_>>(),
        )));
        ui.set_screen("profiles".into());
    });
}

fn select_profile(app: &Arc<App>, id: String) {
    let (name, icon, color) = {
        let mut st = lock(&app.store);
        let Ok(p) = st.profile(&id) else { return };
        let vals = (p.name.clone(), p.icon.clone(), p.color.clone());
        st.last_profile = Some(id.clone());
        vals
    };
    app.save();
    *lock(&app.profile) = Some(id);
    app.ui(move |ui| {
        ui.set_profile_name(name.into());
        ui.set_profile_icon(icon.into());
        ui.set_profile_color(hex_color(&color));
        ui.set_screen("main".into());
    });
    load_page(app, "home");
    sync_account_cw(app);
}

/// Backfills the landscape art of Continue Watching entries that have no
/// `background` URL yet (older stores): fetches the title's meta once, picks
/// the episode thumbnail (series) or backdrop (movie), persists it and
/// refreshes the row if the home page is showing.
fn backfill_cw_art(app: &Arc<App>) {
    let Some(pid) = app.profile_id() else { return };
    // (video id, title id, kind, episode id) of entries missing art.
    let wanted: Vec<(String, String, String, Option<String>)> = {
        let st = lock(&app.store);
        let Ok(p) = st.profile(&pid) else { return };
        p.progress
            .iter()
            .filter(|(_, pr)| pr.card.as_ref().is_some_and(|c| c.background.is_none()))
            .map(|(vid, pr)| {
                let c = pr.card.as_ref().unwrap();
                (
                    vid.clone(),
                    c.title_id.clone(),
                    c.kind.clone(),
                    c.episode_id.clone(),
                )
            })
            .collect()
    };
    if wanted.is_empty() {
        return;
    }
    let app = app.clone();
    app.rt.clone().spawn(async move {
        let addons = lock(&app.addons).clone();
        for (video_id, title_id, kind, episode_id) in wanted {
            let Some(addon) = addons.iter().find(|a| a.supports("meta", &kind, &title_id)) else {
                continue;
            };
            let Ok(meta) = app.client.meta(addon, &kind, &title_id).await else {
                continue;
            };
            // Series: the episode's thumbnail; movies: the backdrop. Fall
            // back to whichever landscape image exists.
            let background = if kind == "series" {
                episode_id
                    .as_deref()
                    .and_then(|vid| {
                        meta.videos
                            .iter()
                            .find(|v| v.id == vid)
                            .and_then(|v| v.thumbnail.clone())
                    })
                    .or_else(|| meta.background.clone())
            } else {
                meta.background.clone().or_else(|| meta.poster.clone())
            };
            let Some(background) = background else {
                continue;
            };
            let changed = {
                let mut st = lock(&app.store);
                let Ok(p) = st.profile_mut(&pid) else {
                    continue;
                };
                if let Some(pr) = p.progress.get_mut(&video_id) {
                    if let Some(c) = pr.card.as_mut() {
                        if c.background.is_none() {
                            c.background = Some(background);
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                } else {
                    false
                }
            };
            if changed {
                app.save();
            }
        }
        // Refresh the Continue Watching row so the new art shows up.
        if app
            .ui
            .upgrade()
            .is_some_and(|ui| ui.get_page().as_str() == "home")
        {
            let cw_items = cw_items_for(&app);
            let mut data = lock(&app.rows_cache).clone();
            match data.first_mut() {
                Some(row) if row.0 == "Continue watching" => row.2 = cw_items,
                _ => {
                    if !cw_items.is_empty() {
                        data.insert(0, ("Continue watching".to_owned(), String::new(), cw_items));
                    }
                }
            }
            let wanted = poster_wanted(
                &data
                    .iter()
                    .map(|(t, _, items)| (t.clone(), items.clone()))
                    .collect::<Vec<_>>(),
            );
            *lock(&app.rows_cache) = data.clone();
            let app2 = app.clone();
            app.ui(move |ui| {
                if ui.get_page().as_str() == "home" {
                    let model = rows_with_posters(&data, &lock(&app2.posters));
                    ui.set_rows(ModelRc::new(VecModel::from(model)));
                }
            });
            app.fetch_posters(wanted);
        }
    });
}

/// Pull Continue Watching entries from the Stremio account library
/// (`datastoreGet` on `libraryItem`) and merge them into the current
/// profile's progress. Local entries always win; the row refreshes when done.
fn sync_account_cw(app: &Arc<App>) {
    let Some(auth) = lock(&app.store).auth.clone() else {
        return;
    };
    let app = app.clone();
    app.rt.clone().spawn(async move {
        let Some(pid) = app.profile_id() else { return };
        let items = match app.api.library(&auth.auth_key).await {
            Ok(i) => i,
            Err(e) => {
                tracing::warn!("could not fetch account library: {e}");
                return;
            }
        };
        // Newest first by lastWatched (ISO strings order correctly).
        let mut cw: Vec<&plusflix_core::stremio::LibraryItem> =
            items.iter().filter(|i| i.in_continue_watching()).collect();
        cw.sort_by(|a, b| b.state.last_watched.cmp(&a.state.last_watched));
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        {
            let mut st = lock(&app.store);
            let Ok(p) = st.profile_mut(&pid) else {
                return;
            };
            for (idx, item) in cw.iter().enumerate() {
                let video_id = item
                    .state
                    .video_id
                    .clone()
                    .filter(|v| v != &item.id)
                    .unwrap_or_else(|| item.id.clone());
                let pos = item.state.time_offset as f64 / 1000.0;
                let dur = (item.state.duration as f64 / 1000.0 > 0.0)
                    .then_some(item.state.duration as f64 / 1000.0);
                // Same validity rules as the local Continue Watching row.
                if pos <= 10.0 || dur.is_some_and(|d| d - pos <= 120.0) {
                    continue;
                }
                if p.progress.contains_key(&video_id) {
                    continue; // local state wins
                }
                p.progress.insert(
                    video_id.clone(),
                    store::Progress {
                        position_secs: pos,
                        duration_secs: dur,
                        updated_unix: now.saturating_sub(idx as u64),
                        card: Some(ProgressCard {
                            kind: item.kind.clone(),
                            title_id: item.id.clone(),
                            title: item.name.clone(),
                            poster: item.poster.clone(),
                            background: None, // backfilled lazily
                            episode_id: (item.kind == "series").then(|| video_id.clone()),
                            episode_label: None,
                        }),
                    },
                );
            }
        }
        app.save();
        // Newly synced entries have no 16:9 card art yet; fetch it in the
        // background and let the backfill refresh the row when done.
        backfill_cw_art(&app);
        // Patch the Continue Watching row in place instead of re-fetching the
        // whole page: the catalog rows are already on screen.
        let on_home = app
            .ui
            .upgrade()
            .is_some_and(|ui| ui.get_page().as_str() == "home");
        if !on_home {
            return;
        }
        let cw_items = cw_items_for(&app);
        if cw_items.is_empty() {
            return;
        }
        let mut data = lock(&app.rows_cache).clone();
        match data.first_mut() {
            Some(row) if row.0 == "Continue watching" => row.2 = cw_items,
            _ => data.insert(0, ("Continue watching".to_owned(), String::new(), cw_items)),
        }
        let wanted = poster_wanted(
            &data
                .iter()
                .map(|(t, _, items)| (t.clone(), items.clone()))
                .collect::<Vec<_>>(),
        );
        *lock(&app.rows_cache) = data.clone();
        app.ui(move |ui| {
            if ui.get_page().as_str() == "home" {
                let model: Vec<RowData> = data
                    .iter()
                    .map(|(title, subtitle, items)| RowData {
                        title: title.clone().into(),
                        subtitle: subtitle.clone().into(),
                        items: items_model(items),
                    })
                    .collect();
                ui.set_rows(ModelRc::new(VecModel::from(model)));
            }
        });
        app.fetch_posters(wanted);
    });
}

fn lang_names() -> Vec<slint::SharedString> {
    std::iter::once("Any / none".into())
        .chain(Lang::ALL.iter().map(|l| l.name().into()))
        .collect()
}

/// Continue Watching cards from the current profile's progress (with badges).
fn cw_items_for(app: &Arc<App>) -> Vec<ItemData> {
    // Collect entries under the store lock, then drop it before calling
    // `item_flags` (it locks the store itself — never nest).
    let mut items: Vec<ItemData> = app
        .profile_id()
        .and_then(|pid| {
            let st = lock(&app.store);
            let p = st.profile(&pid).ok()?;
            Some(
                p.continue_watching()
                    .iter()
                    .filter_map(|pr| ItemData::from_progress(pr))
                    .collect::<Vec<_>>(),
            )
        })
        .unwrap_or_default();
    for it in items.iter_mut() {
        let (w, f) = item_flags(app, &it.title_id, &it.kind);
        it.watched = w;
        it.fav = f;
    }
    items
}

fn lang_index(l: Option<Lang>) -> i32 {
    l.and_then(|l| Lang::ALL.iter().position(|x| *x == l))
        .map(|i| i as i32 + 1)
        .unwrap_or(0)
}

fn lang_from_index(i: i32) -> Option<Lang> {
    usize::try_from(i - 1)
        .ok()
        .and_then(|i| Lang::ALL.get(i).copied())
}

const SUBREQ: [SubtitleRequirement; 3] = [
    SubtitleRequirement::Required,
    SubtitleRequirement::RequiredIfAudioNotPreferred,
    SubtitleRequirement::NotRequired,
];

fn fill_settings(ui: &MainWindow, s: &Settings) {
    // The UI auto-saves on every control change; suppress that while filling.
    ui.set_settings_loading(true);
    ui.set_lang_names(ModelRc::new(VecModel::from(lang_names())));
    ui.set_quality_names(ModelRc::new(VecModel::from(
        Quality::CHOICES
            .iter()
            .map(|q| slint::SharedString::from(q.label()))
            .collect::<Vec<_>>(),
    )));
    ui.set_audio_idx(lang_index(s.audio_language));
    ui.set_audio_mode_idx(match s.language_mode {
        LanguageMode::Must => 1,
        LanguageMode::Prefer => 0,
    });
    ui.set_sub_idx(lang_index(s.subtitle_language));
    ui.set_subreq_idx(
        SUBREQ
            .iter()
            .position(|r| *r == s.subtitle_requirement)
            .unwrap_or(2) as i32,
    );
    ui.set_minq_idx(
        Quality::CHOICES
            .iter()
            .position(|q| *q == s.min_quality)
            .unwrap_or(0) as i32,
    );
    ui.set_max_size(
        s.max_filesize_gb
            .map(|g| g.to_string())
            .unwrap_or_default()
            .into(),
    );
    ui.set_player_idx(match s.player {
        PlayerKind::Mpv => 0,
        PlayerKind::Vlc => 1,
    });
    ui.set_settings_loading(false);
}

/// REEL_DEBUG_FIXTURE=1: synthetic poster rows (short / 40-char / 120-char
/// titles plus a missing-poster row) with procedurally drawn posters, so
/// poster sizing can be checked without the network.
fn fixture_rows(app: &Arc<App>) -> Vec<(String, String, Vec<ItemData>)> {
    let long40 = "A Title Of Forty Characters Exactly Padded X".to_string();
    let long120 =
        "The Ridiculously Long Continuing Catalog Title Which Goes On And On And Should Be \
        Elided After Exactly Two Lines Of Thirteen Pixel Text Rendering Here"
            .to_string();
    let mk = |i: usize, name: &str, poster: Option<String>| ItemData {
        id: format!("fx{i}"),
        kind: "movie".into(),
        name: name.to_owned(),
        year: "2026".into(),
        poster_url: poster,
        landscape_url: None,
        title_id: format!("fx{i}"),
        episode_label: String::new(),
        time_left: String::new(),
        progress: 0.0,
        has_progress: false,
        watched: false,
        fav: false,
        is_cw: false,
        source_state: SourceStatus::Loading,
    };
    let mut rows: Vec<(String, String, Vec<ItemData>)> = Vec::new();
    let mut idx = 0usize;
    for (label, name) in [
        ("Short titles", "Short".to_string()),
        ("40-char titles", long40.clone()),
        ("120-char titles", long120.clone()),
    ] {
        let items: Vec<ItemData> = (0..8)
            .map(|_| {
                idx += 1;
                mk(idx, &name, Some(format!("fixture://p{idx}")))
            })
            .collect();
        rows.push((label.to_owned(), String::new(), items));
    }
    rows.push((
        "Missing posters".to_owned(),
        String::new(),
        (0..6)
            .map(|_| {
                idx += 1;
                mk(idx, &long40, None)
            })
            .collect(),
    ));
    // Flat-coloured placeholder posters in the cache.
    let mut posters = lock(&app.posters);
    for i in 1..=idx {
        let (w, h) = (30u32, 45u32);
        let (r, g, b) = match i % 3 {
            0 => (229u8, 72u8, 77u8),
            1 => (62u8, 99u8, 221u8),
            _ => (48u8, 164u8, 108u8),
        };
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let shade = if (x / 3 + y / 3) % 2 == 0 {
                    255u32
                } else {
                    205
                };
                rgba.extend_from_slice(&[
                    (r as u32 * shade / 255) as u8,
                    (g as u32 * shade / 255) as u8,
                    (b as u32 * shade / 255) as u8,
                    255,
                ]);
            }
        }
        posters.insert(format!("fixture://p{i}"), Arc::new(Decoded { rgba, w, h }));
    }
    rows
}

fn load_page(app: &Arc<App>, page: &str) {
    if page == "home" && std::env::var("REEL_DEBUG_FIXTURE").is_ok_and(|v| v == "1") {
        let data = fixture_rows(app);
        *lock(&app.rows_cache) = data.clone();
        let app2 = app.clone();
        app.ui(move |ui| {
            ui.set_page("home".into());
            let model = rows_with_posters(&data, &lock(&app2.posters));
            ui.set_rows(ModelRc::new(VecModel::from(model)));
        });
        return;
    }
    let page = page.to_owned();
    {
        let page = page.clone();
        app.ui(move |ui| {
            ui.set_page(page.into());
            ui.set_rows(ModelRc::default());
            ui.set_favorites(ModelRc::default());
        });
    }
    match page.as_str() {
        "home" | "movies" | "series" => {
            let kind = match page.as_str() {
                "movies" => Some("movie"),
                "series" => Some("series"),
                _ => None,
            };
            let app = app.clone();
            app.rt.clone().spawn(async move {
                app.ui(|ui| ui.set_busy(true));
                // Continue Watching cards + badges come from the local store.
                let cw_items = cw_items_for(&app);
                let addons = lock(&app.addons).clone();
                let jobs = home::catalog_jobs(&addons, kind, if kind.is_none() { 10 } else { 14 });
                let gen = app
                    .load_gen
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                    + 1;
                let mut data: Vec<(String, String, Vec<ItemData>)> = Vec::new();
                let has_cw = page == "home" && !cw_items.is_empty();
                if has_cw {
                    data.push(("Continue watching".to_owned(), String::new(), cw_items));
                }
                *lock(&app.rows_cache) = data.clone();
                // Render Continue Watching immediately; catalogs stream in.
                {
                    let data = data.clone();
                    let page = page.clone();
                    let app2 = app.clone();
                    app.ui(move |ui| {
                        if ui.get_page().as_str() == page {
                            let model = rows_with_posters(&data, &lock(&app2.posters));
                            ui.set_rows(ModelRc::new(VecModel::from(model)));
                        }
                    });
                }
                if let Some((_, _, cw)) = data.first() {
                    let wanted = poster_wanted(&[(String::new(), cw.clone())]);
                    app.fetch_posters(wanted);
                }
                // Old progress entries may lack the 16:9 card art; fetch it once.
                if has_cw {
                    backfill_cw_art(&app);
                }

                // One task per catalog: each appends its row to the UI as soon
                // as it finishes, so a slow addon doesn't hold up the page.
                let remaining = Arc::new(std::sync::atomic::AtomicUsize::new(jobs.len()));
                for job in jobs {
                    let app = app.clone();
                    let client = app.client.clone();
                    let page = page.clone();
                    let remaining = remaining.clone();
                    app.rt.clone().spawn(async move {
                        let busy_page = page.clone();
                        let items = home::fetch_job(&client, &job).await;
                        let cur = app.load_gen.load(std::sync::atomic::Ordering::SeqCst);
                        if cur == gen {
                            if let Some(items) = items {
                                let mut items: Vec<ItemData> =
                                    items.iter().map(ItemData::from_preview).collect();
                                apply_flags(&app, &mut items);
                                let wanted = poster_wanted(&[(String::new(), items.clone())]);
                                let snapshot = {
                                    let mut cache = lock(&app.rows_cache);
                                    // Unique row title: on a duplicate catalog name
                                    // (same addon exposing one per type), append the type.
                                    let base = job.title.clone();
                                    let title = if cache.iter().any(|(t, _, _)| *t == base) {
                                        let kinded = format!("{base} ({})", job.kind);
                                        let n =
                                            cache.iter().filter(|(t, _, _)| *t == kinded).count();
                                        if n == 0 {
                                            kinded
                                        } else {
                                            format!("{kinded} {}", n + 1)
                                        }
                                    } else {
                                        base
                                    };
                                    cache.push((title, job.addon_name.clone(), items.clone()));
                                    cache.clone()
                                };
                                let app2 = app.clone();
                                app.ui(move |ui| {
                                    if ui.get_page().as_str() == page {
                                        let model =
                                            rows_with_posters(&snapshot, &lock(&app2.posters));
                                        ui.set_rows(ModelRc::new(VecModel::from(model)));
                                    }
                                });
                                app.fetch_posters(wanted);
                            }
                        }
                        if remaining.fetch_sub(1, std::sync::atomic::Ordering::SeqCst) == 1 {
                            let cur = app.load_gen.load(std::sync::atomic::Ordering::SeqCst);
                            if cur == gen {
                                app.ui(move |ui| {
                                    if ui.get_page().as_str() == busy_page {
                                        ui.set_busy(false);
                                    }
                                });
                            }
                        }
                    });
                }
            });
        }
        "favorites" => {
            let mut items: Vec<ItemData> = {
                let id = app.profile_id();
                let st = lock(&app.store);
                id.and_then(|id| {
                    st.profile(&id)
                        .ok()
                        .map(|p| p.favorites.iter().map(ItemData::from_favorite).collect())
                })
                .unwrap_or_default()
            };
            apply_flags(app, &mut items);
            let wanted = poster_wanted(&[(String::new(), items.clone())]);
            app.ui(move |ui| ui.set_favorites(items_model(&items)));
            app.fetch_posters(wanted);
        }
        "settings" => {
            let s = app.profile_settings();
            let addons = addon_rows(&lock(&app.addons), &s.wait_for_addons);
            app.ui(move |ui| {
                fill_settings(ui, &s);
                ui.set_addons(ModelRc::new(VecModel::from(addons)));
                ui.set_refreshing_addons(false);
            });
        }
        _ => {}
    }
}

// ---------------------------------------------------------------- detail + sources

fn summary(stats: &PoolStats) -> String {
    let hidden: usize = stats.rejected.values().sum();
    let searching = stats.pending_tasks > 0;
    if stats.total == 0 {
        return if searching {
            "Searching sources…".into()
        } else {
            "Your addons returned no playable sources.".into()
        };
    }
    if stats.usable == 0 && !searching {
        return format!("No source matches this profile's settings ({hidden} hidden).");
    }
    let mut s = format!(
        "{} usable source{}",
        stats.usable,
        if stats.usable == 1 { "" } else { "s" }
    );
    if hidden > 0 {
        s += &format!(", {hidden} hidden by your settings");
    }
    if searching {
        s += " – still searching…";
    }
    s
}

/// Seasons of a series as (season number, display name), specials (0) last.
fn seasons_of(meta: &Meta) -> Vec<(u32, String)> {
    let mut seasons: Vec<u32> = Vec::new();
    for v in &meta.videos {
        if let Some(s) = v.season {
            if !seasons.contains(&s) {
                seasons.push(s);
            }
        }
    }
    seasons.sort_by_key(|s| (*s == 0, *s));
    seasons
        .into_iter()
        .map(|s| {
            let name = if s == 0 {
                "Specials".to_owned()
            } else {
                format!("Season {s}")
            };
            (s, name)
        })
        .collect()
}

/// Episode rows (id, "n. Title" label, thumbnail url) of one season.
fn episodes_of(meta: &Meta, season: u32) -> Vec<(String, String, Option<String>)> {
    let mut eps: Vec<_> = meta
        .videos
        .iter()
        .filter(|v| v.season == Some(season) && v.episode.is_some())
        .collect();
    eps.sort_by_key(|v| v.episode);
    eps.iter()
        .map(|v| {
            let label = format!(
                "{}. {}",
                v.episode.unwrap_or(0),
                v.title.clone().unwrap_or_default()
            );
            (
                v.id.clone(),
                label.trim_end().to_owned(),
                v.thumbnail.clone(),
            )
        })
        .collect()
}

/// Watched set and progress fractions for one season's episodes. Reads the
/// store once; never call store-locking helpers while holding the lock.
fn episode_flags(
    app: &Arc<App>,
    rows: &[(String, String, Option<String>)],
) -> (std::collections::HashSet<String>, HashMap<String, f32>) {
    let ids: Vec<&str> = rows.iter().map(|(id, _, _)| id.as_str()).collect();
    let pid = app.profile_id();
    let st = lock(&app.store);
    match pid.and_then(|p| st.profile(&p).ok()) {
        Some(p) => {
            let watched = ids
                .iter()
                .filter(|i| p.is_watched(i))
                .map(|i| (*i).to_owned())
                .collect();
            let progress = ids
                .iter()
                .filter_map(|i| {
                    p.progress.get(*i).and_then(|pr| {
                        pr.duration_secs
                            .filter(|d| *d > 0.0)
                            .map(|d| ((*i).to_owned(), (pr.position_secs / d) as f32))
                    })
                })
                .collect::<HashMap<_, _>>();
            (watched, progress)
        }
        None => Default::default(),
    }
}

fn episode_model(
    rows: &[(String, String, Option<String>)],
    watched: &std::collections::HashSet<String>,
    progress: &HashMap<String, f32>,
) -> Vec<EpisodeData> {
    rows.iter()
        .map(|(id, label, _)| EpisodeData {
            id: id.into(),
            label: label.into(),
            thumb: Image::default(),
            has_thumb: false,
            progress: progress.get(id).copied().unwrap_or(0.0),
            watched: watched.contains(id),
        })
        .collect()
}

fn patch_episode_thumb(ui: &MainWindow, id: &str, dec: &Decoded) {
    let eps = ui.get_episodes();
    for i in 0..eps.row_count() {
        if let Some(mut e) = eps.row_data(i) {
            if e.id.as_str() == id {
                e.thumb = dec.to_image();
                e.has_thumb = true;
                eps.set_row_data(i, e);
            }
        }
    }
}

/// Patches every visible card for `id` (rows, favorites and search grids)
/// in place, without rebuilding the models.
fn patch_item(ui: &MainWindow, id: &str, f: impl Fn(&mut Item)) {
    for model in [
        ui.get_favorites(),
        ui.get_search_results(),
        ui.get_genre_items(),
    ] {
        for i in 0..model.row_count() {
            let Some(mut it) = model.row_data(i) else {
                continue;
            };
            if it.id.as_str() == id {
                f(&mut it);
                model.set_row_data(i, it);
            }
        }
    }
    let rows = ui.get_rows();
    for r in 0..rows.row_count() {
        let Some(row) = rows.row_data(r) else {
            continue;
        };
        let items = row.items.clone();
        for i in 0..items.row_count() {
            let Some(mut it) = items.row_data(i) else {
                continue;
            };
            if it.id.as_str() == id {
                f(&mut it);
                items.set_row_data(i, it);
            }
        }
    }
}

/// Starts (or restarts) background source loading for `video_id`. Must run inside the tokio runtime.
fn start_sources(app: &Arc<App>, detail: &Arc<Detail>, video_id: String) {
    let pool = SourcePool::new();
    let addons = lock(&app.addons).clone();
    let loader = SourceLoader::start(
        app.client.clone(),
        &addons,
        &detail.kind,
        &video_id,
        pool.clone(),
    );
    *lock(&detail.current) = Some(Arc::new(Sources {
        video_id,
        pool: pool.clone(),
        _loader: loader,
    }));

    let a = app.clone();
    // Cache key for the play pill state: the catalog item's own id.
    let item_id = detail.meta.id.clone();
    let handle = app.rt.spawn(async move {
        loop {
            let settings = a.profile_settings();
            let stats = pool.stats(&settings);
            let rows: Vec<SourceRowData> = pool
                .rows(&settings)
                .into_iter()
                .map(|r| {
                    let status = if let Some(rej) = r.rejected {
                        format!("Hidden – {}", rej.label())
                    } else if r.tried {
                        "Tried".to_owned()
                    } else {
                        "Ready".to_owned()
                    };
                    SourceRowData {
                        key: r.key.into(),
                        label: r.label.into(),
                        status: status.into(),
                        ok: r.rejected.is_none(),
                    }
                })
                .collect();
            let text = summary(&stats);
            // "Wait for this addon": while any addon the profile waits for is
            // still loading, the state stays Loading even when other addons
            // already delivered usable sources — the green/Ready light (and
            // the popup play button colour) only turns on once they finish.
            // Only the state is gated this way; a manual Play click still
            // starts playback immediately.
            let waited_pending = settings
                .wait_for_addons
                .iter()
                .any(|name| pool.addon_pending(name));
            let state = if stats.usable > 0 && !waited_pending {
                "ready"
            } else if stats.pending_tasks > 0 {
                "loading"
            } else {
                "none"
            };
            let state = state.to_owned();
            // Mirror the resolution state onto the catalog card play pill.
            let status = if stats.usable > 0 && !waited_pending {
                SourceStatus::Ready
            } else if stats.pending_tasks > 0 {
                SourceStatus::Loading
            } else {
                SourceStatus::None
            };
            let id = item_id.clone();
            if lock(&a.source_states).get(&id).copied() != Some(status) {
                lock(&a.source_states).insert(id.clone(), status);
                let a = a.clone();
                a.ui(move |ui| {
                    patch_item(ui, &id, |it| it.source_state = status as i32);
                });
            }
            a.ui(move |ui| {
                ui.set_sources_summary(text.into());
                ui.set_sources_state(state.into());
                ui.set_source_rows(ModelRc::new(VecModel::from(rows)));
            });
            tokio::time::sleep(Duration::from_millis(800)).await;
        }
    });
    *lock(&detail.stats) = Some(AbortOnDrop(handle));
}

fn open_item(app: &Arc<App>, id: String, kind: String) {
    let app = app.clone();
    app.rt.clone().spawn(async move {
        let addons = lock(&app.addons).clone();
        let fallback = || Meta {
            id: id.clone(),
            kind: kind.clone(),
            name: id.clone(),
            ..Default::default()
        };
        let meta = match addons.iter().find(|a| a.supports("meta", &kind, &id)) {
            Some(a) => match app.client.meta(a, &kind, &id).await {
                Ok(m) if !m.id.is_empty() => m,
                Ok(_) => fallback(),
                Err(e) => {
                    tracing::warn!("meta failed: {e}");
                    fallback()
                }
            },
            None => fallback(),
        };

        // Episodes (series only), ordered with specials (season 0) last.
        let is_series = kind == "series" && !meta.videos.is_empty();
        let mut all_eps: Vec<_> = meta
            .videos
            .iter()
            .filter(|v| v.season.is_some() && v.episode.is_some())
            .collect();
        all_eps.sort_by_key(|v| (v.season == Some(0), v.season, v.episode));
        let all_ids: Vec<String> = all_eps.iter().map(|v| v.id.clone()).collect();
        if is_series {
            lock(&app.known_episodes).insert(meta.id.clone(), all_ids.clone());
        }

        let (is_fav, last_ep, player_kind) = {
            let pid = app.profile_id();
            let st = lock(&app.store);
            match pid.and_then(|p| st.profile(&p).ok()) {
                Some(p) => (
                    p.is_favorite(&meta.id),
                    last_watched(p, &meta.id),
                    p.settings.player,
                ),
                None => (false, None, PlayerKind::default()),
            }
        };
        let video_id = if is_series {
            last_ep
                .filter(|e| all_ids.iter().any(|id| id == e))
                .or_else(|| all_ids.first().cloned())
        } else {
            Some(meta.id.clone())
        };

        // Resume position and watched state of the selected video.
        let (resume_pos, episode_watched) = {
            let pid = app.profile_id();
            let st = lock(&app.store);
            match pid.and_then(|p| st.profile(&p).ok()) {
                Some(p) => {
                    let r = video_id
                        .as_deref()
                        .map(|v| p.resume_position(v))
                        .unwrap_or(0.0);
                    let w = video_id
                        .as_deref()
                        .map(|v| p.is_watched(v))
                        .unwrap_or(false);
                    (r, w)
                }
                None => (0.0, false),
            }
        };

        let info = [meta.release_info.clone(), meta.runtime.clone()]
            .into_iter()
            .flatten()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("  ·  ");
        // Dedupe genre names that differ only by case/whitespace/punctuation
        // (e.g. after an addon language change), keeping the first-seen spelling.
        let mut seen_genres = std::collections::HashSet::new();
        let genres: Vec<String> = meta
            .genres
            .iter()
            .filter(|g| seen_genres.insert(plusflix_core::genres::normalize_genre_key(g)))
            .cloned()
            .collect();
        // Display translations of the canonical (English) meta genre names.
        let genres_t: Vec<slint::SharedString> = genres
            .iter()
            .map(|g| slint::SharedString::from(genre_label(&app, g)))
            .collect();
        let long_title = meta.name.chars().count() > 40;
        let poster = meta.poster.clone();
        let detail = Arc::new(Detail {
            kind: kind.clone(),
            meta: meta.clone(),
            current: Mutex::new(None),
            stats: Mutex::new(None),
        });
        *lock(&app.detail) = Some(detail.clone());

        {
            let (title, desc, id, selected) = (
                meta.name.clone(),
                meta.description.clone().unwrap_or_default(),
                meta.id.clone(),
                video_id.clone().unwrap_or_default(),
            );
            // Season the popup opens on: the one containing the selected episode.
            let seasons = seasons_of(&meta);
            let sel_season = video_id.as_deref().and_then(|v| {
                meta.videos
                    .iter()
                    .find(|x| x.id == v)
                    .and_then(|x| x.season)
            });
            let season_idx = seasons
                .iter()
                .position(|(s, _)| Some(*s) == sel_season)
                .unwrap_or(1.min(seasons.len().saturating_sub(1)));
            let rows = sel_season
                .map(|s| episodes_of(&meta, s))
                .unwrap_or_default();
            let (watched, progress) = episode_flags(&app, &rows);
            let thumb_wanted: Vec<(String, String)> = rows
                .iter()
                .filter_map(|(id, _, url)| url.clone().map(|u| (id.clone(), u)))
                .collect();
            app.ui(move |ui| {
                ui.set_detail_id(id.into());
                ui.set_detail_title(title.into());
                ui.set_detail_long_title(long_title);
                ui.set_detail_info(info.into());
                ui.set_detail_genres(ModelRc::new(VecModel::from(
                    genres
                        .iter()
                        .map(|g| slint::SharedString::from(g.as_str()))
                        .collect::<Vec<_>>(),
                )));
                ui.set_detail_genres_t(ModelRc::new(VecModel::from(genres_t)));
                ui.set_detail_desc(desc.into());
                ui.set_detail_poster(Image::default());
                ui.set_detail_has_poster(false);
                ui.set_detail_is_series(is_series);
                ui.set_detail_fav(is_fav);
                ui.set_detail_watched(episode_watched);
                ui.set_can_resume(resume_pos > 0.0);
                ui.set_play_player(match player_kind {
                    PlayerKind::Vlc => "vlc".into(),
                    PlayerKind::Mpv => "mpv".into(),
                });
                ui.set_season_names(ModelRc::new(VecModel::from(
                    seasons
                        .iter()
                        .map(|(_, name)| slint::SharedString::from(name.as_str()))
                        .collect::<Vec<_>>(),
                )));
                ui.set_season_idx(season_idx as i32);
                ui.set_episodes(ModelRc::new(VecModel::from(episode_model(
                    &rows, &watched, &progress,
                ))));
                ui.set_selected_episode(selected.into());
                ui.set_sources_summary("Searching sources…".into());
                ui.set_sources_state("loading".into());
                ui.set_source_rows(ModelRc::default());
                ui.set_sources_open(false);
                ui.set_detail_open(true);
            });
            app.fetch_episode_thumbs(thumb_wanted);
        }
        if let Some(url) = poster {
            app.fetch_posters(vec![(meta.id.clone(), url, false)]);
        }
        match video_id {
            Some(v) => start_sources(&app, &detail, v),
            None => app.flash("This title has no episodes."),
        }
    });
}

/// The episode of `series_id` this profile watched most recently.
fn last_watched(p: &Profile, series_id: &str) -> Option<String> {
    let prefix = format!("{series_id}:");
    p.progress
        .iter()
        .filter(|(k, _)| k.starts_with(&prefix))
        .max_by_key(|(_, v)| v.updated_unix)
        .map(|(k, _)| k.clone())
}

fn select_episode(app: &Arc<App>, id: String) {
    let Some(detail) = lock(&app.detail).clone() else {
        return;
    };
    let app = app.clone();
    app.rt.clone().spawn(async move {
        let shown = id.clone();
        let (watched, resume) = {
            let pid = app.profile_id();
            let st = lock(&app.store);
            match pid.and_then(|p| st.profile(&p).ok()) {
                Some(p) => (p.is_watched(&id), p.resume_position(&id)),
                None => (false, 0.0),
            }
        };
        app.ui(move |ui| {
            ui.set_selected_episode(shown.into());
            ui.set_detail_watched(watched);
            ui.set_can_resume(resume > 0.0);
            ui.set_sources_summary("Searching sources…".into());
            ui.set_sources_state("loading".into());
        });
        start_sources(&app, &detail, id);
    });
}

/// Season selector change in the detail popup: rebuild the episode list.
fn season_changed(app: &Arc<App>, ui: &MainWindow, idx: i32) {
    let Some(detail) = lock(&app.detail).clone() else {
        return;
    };
    let seasons = seasons_of(&detail.meta);
    let Some((season, _)) = seasons.get(idx.max(0) as usize) else {
        return;
    };
    let rows = episodes_of(&detail.meta, *season);
    let (watched, progress) = episode_flags(app, &rows);
    let thumb_wanted: Vec<(String, String)> = rows
        .iter()
        .filter_map(|(id, _, url)| url.clone().map(|u| (id.clone(), u)))
        .collect();
    // Keep the selection if it is in this season, otherwise select the first.
    let keep = ui.get_selected_episode().to_string();
    let selected = if rows.iter().any(|(id, _, _)| id == &keep) {
        keep
    } else {
        rows.first()
            .map(|(id, _, _)| id.clone())
            .unwrap_or_default()
    };
    let sel = selected.clone();
    let is_watched = {
        let pid = app.profile_id();
        let st = lock(&app.store);
        pid.and_then(|p| st.profile(&p).ok())
            .map(|p| p.is_watched(&selected))
            .unwrap_or(false)
    };
    let resume = {
        let pid = app.profile_id();
        let st = lock(&app.store);
        pid.and_then(|p| st.profile(&p).ok())
            .map(|p| p.resume_position(&selected))
            .unwrap_or(0.0)
    };
    ui.set_episodes(ModelRc::new(VecModel::from(episode_model(
        &rows, &watched, &progress,
    ))));
    ui.set_selected_episode(sel.into());
    ui.set_detail_watched(is_watched);
    ui.set_can_resume(resume > 0.0);
    app.fetch_episode_thumbs(thumb_wanted);
}

fn toggle_favorite(app: &Arc<App>) {
    let Some(detail) = lock(&app.detail).clone() else {
        return;
    };
    let Some(pid) = app.profile_id() else { return };
    let now_fav = {
        let mut st = lock(&app.store);
        let Ok(p) = st.profile_mut(&pid) else { return };
        p.toggle_favorite(Favorite {
            id: detail.meta.id.clone(),
            kind: detail.kind.clone(),
            name: detail.meta.name.clone(),
            poster: detail.meta.poster.clone(),
        })
    };
    app.save();
    app.ui(move |ui| ui.set_detail_fav(now_fav));
    let on_favorites_page = app
        .ui
        .upgrade()
        .is_some_and(|ui| ui.get_page().as_str() == "favorites");
    if on_favorites_page {
        load_page(app, "favorites");
    }
}

/// Manual watched toggle from the detail popup: the selected episode for
/// series, the movie itself otherwise.
fn toggle_watched(app: &Arc<App>, ui: &MainWindow) {
    let Some(detail) = lock(&app.detail).clone() else {
        return;
    };
    let Some(pid) = app.profile_id() else { return };
    let video_id = if detail.kind == "series" {
        let ep = ui.get_selected_episode().to_string();
        if ep.is_empty() {
            return;
        }
        ep
    } else {
        detail.meta.id.clone()
    };
    let now_watched = {
        let mut st = lock(&app.store);
        let Ok(p) = st.profile_mut(&pid) else { return };
        if p.is_watched(&video_id) {
            p.unmark_watched(&video_id);
            false
        } else {
            p.mark_watched(&video_id);
            true
        }
    };
    app.save();
    let vid = video_id.clone();
    app.ui(move |ui| {
        ui.set_detail_watched(now_watched);
        // Patch the per-episode checks in place.
        let episodes = ui.get_episodes();
        for i in 0..episodes.row_count() {
            let row = episodes.row_data(i);
            if row.as_ref().is_some_and(|e| e.id.as_str() == vid.as_str()) {
                if let Some(mut e) = row {
                    e.watched = now_watched;
                    episodes.set_row_data(i, e);
                }
            }
        }
    });
    load_page(app, ui.get_page().as_str());
}

/// Toggle favorite straight from a catalog card (no detail popup). Only the
/// favorites page rebuilds (the card appears/disappears); every other page
/// just flips the badge in place.
fn card_toggle_favorite(app: &Arc<App>, id: String, kind: String, name: String, poster: String) {
    let Some(pid) = app.profile_id() else { return };
    let poster = (!poster.is_empty()).then(|| poster);
    let now_fav = {
        let mut st = lock(&app.store);
        let Ok(p) = st.profile_mut(&pid) else { return };
        p.toggle_favorite(Favorite {
            id: id.clone(),
            kind,
            name,
            poster,
        })
    };
    app.save();
    let on_favorites_page = app
        .ui
        .upgrade()
        .is_some_and(|ui| ui.get_page().as_str() == "favorites");
    if on_favorites_page {
        load_page(app, "favorites");
    } else {
        let id = id;
        app.ui(move |ui| {
            patch_item(ui, &id, |it| it.fav = now_fav);
        });
    }
}

/// Toggle watched straight from a catalog card: like the detail's series
/// logic, all known episodes of a series (the card badge means "all
/// episodes watched"), the movie itself otherwise.
fn card_toggle_watched(app: &Arc<App>, id: String, kind: String) {
    let Some(pid) = app.profile_id() else { return };
    let now_watched = {
        let mut st = lock(&app.store);
        let Ok(p) = st.profile_mut(&pid) else { return };
        if kind == "series" {
            let eps = lock(&app.known_episodes)
                .get(&id)
                .cloned()
                .unwrap_or_default();
            if eps.is_empty() {
                return;
            }
            let all = eps.iter().all(|e| p.is_watched(e));
            if all {
                for e in &eps {
                    p.unmark_watched(e);
                }
            } else {
                for e in &eps {
                    p.mark_watched(e);
                }
            }
            !all
        } else {
            let was = p.is_watched(&id);
            if was {
                p.unmark_watched(&id);
            } else {
                p.mark_watched(&id);
            }
            !was
        }
    };
    app.save();
    let id = id;
    app.ui(move |ui| {
        patch_item(ui, &id, |it| it.watched = now_watched);
    });
}

/// Remove one Continue Watching entry.
fn remove_progress(app: &Arc<App>, video_id: String) {
    let Some(pid) = app.profile_id() else { return };
    if let Ok(p) = lock(&app.store).profile_mut(&pid) {
        p.progress.remove(&video_id);
    }
    app.save();
    let page = app
        .ui
        .upgrade()
        .map(|ui| ui.get_page().to_string())
        .unwrap_or_else(|| "home".into());
    load_page(app, &page);
}

// ---------------------------------------------------------------- playback

fn save_progress(
    app: &App,
    pid: &str,
    video_id: &str,
    pos: f64,
    dur: Option<f64>,
    card: &ProgressCard,
) {
    if let Ok(p) = lock(&app.store).profile_mut(pid) {
        p.set_progress(video_id, pos, dur, Some(card.clone()));
    }
    app.save();
}

fn play(app: &Arc<App>, kind: String, forced: Option<String>) {
    let Some(detail) = lock(&app.detail).clone() else {
        return;
    };
    let Some(sources) = lock(&detail.current).clone() else {
        app.flash("Pick an episode first.");
        return;
    };
    let Some(pid) = app.profile_id() else { return };
    if app.playing.swap(true, SeqCst) {
        return;
    }
    let player_kind = match kind.as_str() {
        "vlc" => PlayerKind::Vlc,
        _ => PlayerKind::Mpv,
    };
    // Remember the choice per profile.
    let (settings, resume, player_changed) = {
        let mut st = lock(&app.store);
        let (settings, resume) = match st.profile(&pid) {
            Ok(p) => (p.settings.clone(), p.resume_position(&sources.video_id)),
            Err(_) => (Settings::default(), 0.0),
        };
        let mut player_changed = false;
        if let Ok(p) = st.profile_mut(&pid) {
            if p.settings.player != player_kind {
                p.settings.player = player_kind;
                player_changed = true;
            }
        }
        (settings, resume, player_changed)
    };
    // Save only after the store lock above is released (save locks it again;
    // this runs on the UI thread, so a nested lock would freeze the app).
    if player_changed {
        app.save();
    }
    let mut settings = settings;
    settings.player = player_kind;
    // Card info so the entry shows up in Continue Watching. The 16:9 card art
    // is the episode thumbnail (series) or the backdrop (movie).
    let episode = detail.meta.videos.iter().find(|v| v.id == sources.video_id);
    let card = ProgressCard {
        kind: detail.kind.clone(),
        title_id: detail.meta.id.clone(),
        title: detail.meta.name.clone(),
        poster: detail.meta.poster.clone(),
        background: match &episode {
            Some(v) => v
                .thumbnail
                .clone()
                .or_else(|| detail.meta.background.clone()),
            None => detail.meta.background.clone(),
        },
        episode_id: (detail.kind == "series").then(|| sources.video_id.clone()),
        episode_label: episode.and_then(|v| {
            v.season
                .zip(v.episode)
                .map(|(s, e)| format!("S{s:02}E{e:02}"))
        }),
    };
    let app = app.clone();
    app.rt.clone().spawn(async move {
        app.ui(move |ui| {
            ui.set_playing(true);
            ui.set_play_player(match player_kind {
                PlayerKind::Vlc => "vlc".into(),
                PlayerKind::Mpv => "mpv".into(),
            });
            ui.set_play_status("Starting player…".into());
        });
        // Cancel flag for the overlay's Cancel button. Dropping `run_playback`
        // mid-await tears the player down (mpv: kill-on-drop; VLC: the poll
        // loop kills the process once the event channel closes).
        let cancel = Arc::new(AtomicBool::new(false));
        *lock(&app.cancel_playback) = Some(cancel.clone());
        // `detail` and `sources` stay alive until here, so the loaders keep filling the backup pool.
        let wait_cancel = async {
            while !cancel.load(SeqCst) {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        };
        let msg = tokio::select! {
            msg = run_playback(&app, &sources, &pid, settings, resume, card, forced) => msg,
            _ = wait_cancel => Some("Playback cancelled.".to_owned()),
        };
        *lock(&app.cancel_playback) = None;
        drop(detail);
        app.playing.store(false, SeqCst);
        app.ui(|ui| ui.set_playing(false));
        if let Some(m) = msg {
            app.flash(m);
        }
    });
}

/// Wraps the real player so the "Next source" button can break the session's
/// `next_event` wait: the flag turns into a `Skipped` event, which makes the
/// session move the current source to the back of the failover queue (it is
/// *not* marked failed) and load the next best one.
struct Skippable<P> {
    inner: P,
    skip: Arc<AtomicBool>,
}

impl<P: Player> Player for Skippable<P> {
    async fn load(&mut self, req: LoadRequest) -> plusflix_core::error::Result<()> {
        self.inner.load(req).await
    }
    async fn next_event(&mut self) -> Option<PlayerEvent> {
        let wait_skip = async {
            while !self.skip.load(SeqCst) {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        };
        tokio::select! {
            ev = self.inner.next_event() => ev,
            _ = wait_skip => {
                // Consume the flag: one click skips one source.
                self.skip.store(false, SeqCst);
                Some(PlayerEvent::Skipped)
            }
        }
    }
    async fn notify(&mut self, text: &str) {
        self.inner.notify(text).await
    }
    async fn stop(&mut self) {
        self.inner.stop().await
    }
}

async fn run_playback(
    app: &Arc<App>,
    sources: &Arc<Sources>,
    pid: &str,
    settings: Settings,
    resume: f64,
    card: ProgressCard,
    forced: Option<String>,
) -> Option<String> {
    let inner = match settings.player {
        PlayerKind::Vlc => match VlcPlayer::spawn(&VlcConfig::default()).await {
            Ok(p) => AnyPlayer::Vlc(p),
            Err(e) => return Some(format!("{e}")),
        },
        PlayerKind::Mpv => match MpvPlayer::spawn(&MpvConfig::default()).await {
            Ok(p) => AnyPlayer::Mpv(Box::new(p)),
            Err(e) => return Some(format!("{e} – install mpv and make sure it is on PATH.")),
        },
    };
    let skip = Arc::new(AtomicBool::new(false));
    *lock(&app.skip_playback) = Some(skip.clone());
    let mut player = Skippable { inner, skip };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let last = Arc::new(Mutex::new((resume, None::<f64>)));

    let forwarder = {
        let (app, last) = (app.clone(), last.clone());
        let (pid, video_id) = (pid.to_owned(), sources.video_id.clone());
        let card = card.clone();
        tokio::spawn(async move {
            let mut last_save = Instant::now();
            while let Some(ev) = rx.recv().await {
                let status = match ev {
                    SessionEvent::Trying { label, attempt: 1 } => {
                        Some(format!("Starting: {label}"))
                    }
                    SessionEvent::Trying { label, attempt } => {
                        Some(format!("Switching source (attempt {attempt}): {label}"))
                    }
                    SessionEvent::Playing { label, key } => {
                        // Remember which source actually played, so the next
                        // play of this item starts from it.
                        lock(&app.last_played).insert(video_id.clone(), key);
                        Some(format!("Playing: {label}"))
                    }
                    SessionEvent::Skipped { label } => {
                        Some(format!("Skipped {label} – switching…"))
                    }
                    SessionEvent::Failed { label, reason } => {
                        tracing::info!("source failed: {label}: {reason}");
                        Some(format!("{label} failed ({reason})"))
                    }
                    SessionEvent::WaitingForSources => Some("Waiting for more sources…".into()),
                    SessionEvent::Position { secs, duration } => {
                        *lock(&last) = (secs, duration);
                        if last_save.elapsed() > Duration::from_secs(15) {
                            last_save = Instant::now();
                            save_progress(&app, &pid, &video_id, secs, duration, &card);
                        }
                        None
                    }
                };
                if let Some(s) = status {
                    app.ui(move |ui| ui.set_play_status(s.into()));
                }
            }
        })
    };

    let outcome = session::run(
        &mut player,
        &sources.pool,
        &settings,
        &SessionConfig {
            // A manual pick wins; otherwise prefer the source that last
            // actually played for this item.
            first_key: forced.or_else(|| {
                lock(&app.last_played)
                    .get(&sources.video_id)
                    .cloned()
            }),
            ..Default::default()
        },
        resume,
        Some(tx),
    )
    .await;
    *lock(&app.skip_playback) = None;
    player.stop().await;
    let _ = forwarder.await;
    let (_, dur) = *lock(&last);

    match outcome {
        Outcome::Finished { .. } => {
            if let Ok(p) = lock(&app.store).profile_mut(pid) {
                p.mark_watched(&sources.video_id);
            }
            app.save();
            None
        }
        Outcome::Closed { position } => {
            save_progress(app, pid, &sources.video_id, position, dur, &card);
            None
        }
        Outcome::NoSources => {
            Some("No source matches this profile's settings. Relax them in Settings.".into())
        }
        Outcome::Exhausted { position } => {
            save_progress(app, pid, &sources.video_id, position, dur, &card);
            Some("All available sources failed.".into())
        }
    }
}

// ---------------------------------------------------------------- search

fn do_search(app: &Arc<App>, query: String) {
    let q = query.trim().to_owned();
    if q.is_empty() {
        return;
    }
    let app = app.clone();
    app.rt.clone().spawn(async move {
        let shown_q = q.clone();
        app.ui(move |ui| {
            ui.set_searching(true);
            ui.set_page("search".into());
            ui.set_rows(ModelRc::default());
            ui.set_favorites(ModelRc::default());
            ui.set_search_results(ModelRc::default());
            ui.set_status(format!("Searching for “{shown_q}”…").into());
        });
        let addons = lock(&app.addons).clone();
        let results = home::search(&app.client, &addons, &q).await;
        let mut items: Vec<ItemData> = results.iter().map(ItemData::from_preview).collect();
        apply_flags(&app, &mut items);
        let wanted = poster_wanted(&[(String::new(), items.clone())]);
        app.ui(move |ui| {
            ui.set_searching(false);
            ui.set_status("".into());
            if ui.get_page().as_str() == "search" {
                ui.set_search_results(items_model(&items));
            }
        });
        app.fetch_posters(wanted);
    });
}

// ---------------------------------------------------------------- account

// ---------------------------------------------------------------- genres

/// Locale key for a genre name: lowercase, non-alphanumerics folded into a
/// single underscore (e.g. "Sci-Fi" -> `genre_scifi`).
fn genre_key(name: &str) -> String {
    let mut key = String::from("genre_");
    let mut prev_us = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            key.push(ch.to_ascii_lowercase());
            prev_us = false;
        } else if !prev_us && !key.ends_with('_') {
            key.push('_');
            prev_us = true;
        }
    }
    let trimmed = key.trim_end_matches('_').to_owned();
    trimmed
}

/// Display name for a canonical (English) genre: the interface language's
/// translation from the locale files, or the canonical name itself when the
/// locale has no entry for it (no warning — addon lists are open-ended).
fn genre_label(app: &App, name: &str) -> String {
    lock(&app.translations)
        .t_opt(&genre_key(name))
        .unwrap_or_else(|| name.to_owned())
}

/// Genre dropdown rows for one kind, merged by *translated label*: options
/// whose display labels collide are the same genre to the user — e.g. the
/// canonical English "Romance" translates to Polish "Romans" while a
/// Polish-configured addon natively declares "Romans" — so they are one row.
/// Each row is `(canonical name, translated label)`; the canonical name (of
/// the first option, in `genre_options` order) is what selection routes back.
fn merged_genre_rows(app: &App, addons: &[Addon], kind: &str) -> Vec<(String, String)> {
    let mut rows: Vec<(String, String)> = Vec::new(); // (label key, canonical)
    let mut labels: Vec<String> = Vec::new();
    for option in genres::genre_options(addons, kind) {
        let label = genre_label(app, &option.name);
        let key = genres::normalize_genre_key(&label);
        if rows.iter().any(|(k, _)| *k == key) {
            continue; // same label: covered by an earlier canonical name
        }
        rows.push((key, option.name.clone()));
        labels.push(label);
    }
    rows.into_iter()
        .zip(labels)
        .map(|((_, name), label)| (name, label))
        .collect()
}

/// Genre list for one kind: "All genres" + the merged genre rows' canonical
/// names (they go straight back into the addon URLs on selection).
fn genre_names(app: &App, addons: &[Addon], kind: &str) -> Vec<slint::SharedString> {
    let mut out = vec![slint::SharedString::from("All genres")];
    out.extend(
        merged_genre_rows(app, addons, kind)
            .into_iter()
            .map(|(name, _)| slint::SharedString::from(name.as_str())),
    );
    out
}

/// Display translations parallel to [`genre_names`]: same order, entry 0 is the
/// translated "All genres", every other entry the merged rows' translated
/// label (or the canonical name when the locale has no entry).
fn genre_names_t(app: &App, addons: &[Addon], kind: &str) -> Vec<slint::SharedString> {
    let mut out = vec![slint::SharedString::from(
        lock(&app.translations).t("all_genres"),
    )];
    out.extend(
        merged_genre_rows(app, addons, kind)
            .into_iter()
            .map(|(_, label)| slint::SharedString::from(label.as_str())),
    );
    out
}

/// Rebuilds the Movies/Series genre lists from the current addon manifests
/// (pure manifest data, no requests). Runs on every addon refresh.
fn update_genre_options(app: &Arc<App>) {
    let addons = lock(&app.addons).clone();
    for kind in ["movie", "series"] {
        let catalogs = genres::genre_catalogs(&addons, kind);
        tracing::info!(
            "[genres] {kind}: catalogs with genre support: {}",
            catalogs
                .iter()
                .map(|c| c.catalog_name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
        let options = genres::genre_options(&addons, kind);
        tracing::info!(
            "[genres] {kind}: {} genre options: {}",
            options.len(),
            options
                .iter()
                .map(|o| o.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let movie = genre_names(app, &addons, "movie");
    let series = genre_names(app, &addons, "series");
    let movie_t = genre_names_t(app, &addons, "movie");
    let series_t = genre_names_t(app, &addons, "series");
    app.ui(move |ui| {
        ui.set_movie_genres(ModelRc::new(VecModel::from(movie)));
        ui.set_series_genres(ModelRc::new(VecModel::from(series)));
        ui.set_movie_genres_t(ModelRc::new(VecModel::from(movie_t)));
        ui.set_series_genres_t(ModelRc::new(VecModel::from(series_t)));
    });
}

/// Kind of the current catalog page ("movie" / "series"), or None off it.
fn page_kind(app: &App) -> Option<&'static str> {
    let page = app.ui.upgrade().map(|ui| ui.get_page());
    match page.as_deref() {
        Some("movies") => Some("movie"),
        Some("series") => Some("series"),
        _ => None,
    }
}

/// Starts browsing a genre on the current page ("" = back to the plain rows).
fn genre_chosen(app: &Arc<App>, genre: &str) {
    let Some(kind) = page_kind(app) else {
        tracing::warn!("[genres] genre-chosen fired off the movies/series pages");
        return;
    };
    genre_chosen_kind(app, kind, genre);
}

/// Same, but with the catalog kind passed explicitly — for callers that just
/// switched pages (`load_page` updates `page` via a queued ui closure, so
/// `page_kind` would read the stale page and silently drop the filter).
fn genre_chosen_kind(app: &Arc<App>, kind: &'static str, genre: &str) {
    let is_movie = kind == "movie";
    if genre.is_empty() {
        tracing::info!("[genres] {kind}: genre filter cleared");
        *lock(&app.genre) = None;
        app.ui(move |ui| {
            if is_movie {
                ui.set_movies_genre("".into());
                ui.set_movies_genre_t("".into());
            } else {
                ui.set_series_genre("".into());
                ui.set_series_genre_t("".into());
            }
            ui.set_genre_catalogs(ModelRc::default());
            ui.set_genre_catalog_idx(0);
            ui.set_genre_items(ModelRc::default());
            ui.set_genre_has_more(false);
            ui.set_genre_busy(false);
            ui.set_genre_loading(false);
        });
        return;
    }
    let addons = lock(&app.addons).clone();
    // The chosen row routes back its canonical name, but the dropdown merges
    // by translated label: every canonical genre name sharing the chosen
    // name's label (e.g. "Romance" and a Polish addon's native "Romans") must
    // contribute its catalogs, deduped by (manifest id, catalog id).
    let label = genre_label(app, genre);
    let label_key = genres::normalize_genre_key(&label);
    let names: Vec<String> = genres::genre_options(&addons, kind)
        .into_iter()
        .map(|o| o.name)
        .filter(|name| genres::normalize_genre_key(&genre_label(app, name)) == label_key)
        .collect();
    let mut catalogs: Vec<genres::CatalogRef> = Vec::new();
    for name in &names {
        // `catalogs_for_genre` covers catalogs declaring the name AND catalogs
        // with no options (they accept any genre).
        for catalog in genres::catalogs_for_genre(&addons, kind, name) {
            let seen = catalogs
                .iter()
                .any(|c| c.identity() == catalog.identity());
            if !seen {
                catalogs.push(catalog);
            }
        }
    }
    tracing::info!(
        "[genres] {kind} / \"{genre}\": catalogs: {}",
        catalogs
            .iter()
            .map(|c| c.catalog_name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    let chips: Vec<slint::SharedString> = std::iter::once(slint::SharedString::from(
        lock(&app.translations).t("genre_all_catalogs"),
    ))
    .chain(
        catalogs
            .iter()
            .map(|c| slint::SharedString::from(c.catalog_name.as_str())),
    )
    .collect();
    let token = app.genre_gen.fetch_add(1, SeqCst) + 1;
    *lock(&app.genre) = Some(GenreState {
        kind: String::from(kind),
        genre: String::from(genre),
        browser: GenreBrowser::new(kind, genre, catalogs.clone()),
        catalogs,
        items: Vec::new(),
        has_more: true,
        token,
    });
    let g = String::from(genre);
    let g_t = genre_label(app, genre);
    app.ui(move |ui| {
        if is_movie {
            ui.set_movies_genre(g.into());
            ui.set_movies_genre_t(g_t.into());
        } else {
            ui.set_series_genre(g.into());
            ui.set_series_genre_t(g_t.into());
        }
        ui.set_genre_catalogs(ModelRc::new(VecModel::from(chips)));
        ui.set_genre_catalog_idx(0);
        ui.set_genre_items(ModelRc::default());
        ui.set_genre_has_more(false);
        ui.set_genre_busy(true);
        ui.set_genre_loading(false);
    });
    genre_fetch_page(app);
}

/// Narrows the active genre browse to one catalog (index 0 = "All").
fn genre_catalog_chosen(app: &Arc<App>, idx: i32) {
    let Some(kind) = page_kind(app) else { return };
    let sel = {
        let mut state = lock(&app.genre);
        let Some(st) = state.as_mut() else { return };
        if st.kind != kind {
            return;
        }
        if idx <= 0 {
            st.catalogs.clone()
        } else {
            match st.catalogs.get(idx as usize - 1) {
                Some(c) => vec![c.clone()],
                None => return,
            }
        }
    };
    {
        let mut state = lock(&app.genre);
        let Some(st) = state.as_mut() else { return };
        st.browser.set_selection(&st.genre, sel);
        st.items.clear();
        st.has_more = true;
    }
    app.ui(move |ui| {
        ui.set_genre_catalog_idx(idx);
        ui.set_genre_items(ModelRc::default());
        ui.set_genre_has_more(false);
        ui.set_genre_busy(true);
        ui.set_genre_loading(false);
    });
    genre_fetch_page(app);
}

/// Genre chip clicked in the detail overlay: close the detail, switch to the
/// matching catalog page and start browsing that genre there. `genre` is the
/// canonical (English) name, straight from the meta.
fn detail_genre_chosen(app: &Arc<App>, genre: &str) {
    if genre.is_empty() {
        return;
    }
    // Off the detail overlay there is nothing to navigate from.
    let Some(kind) = lock(&app.detail).as_ref().map(|d| d.kind.clone()) else {
        return;
    };
    let (page, kind): (&str, &'static str) = match kind.as_str() {
        "movie" => ("movies", "movie"),
        "series" => ("series", "series"),
        _ => return,
    };
    // Close the detail first (same as close-detail), then load the target page
    // and start the genre browse there. `load_page` updates `page` via a
    // queued ui closure, so the kind is passed to `genre_chosen_kind`
    // directly — `genre_chosen` would re-read the stale page and bail.
    *lock(&app.detail) = None;
    app.ui(|ui| ui.set_detail_open(false));
    tracing::info!("[genres] detail chip: {kind} / \"{genre}\"");
    load_page(app, page);
    genre_chosen_kind(app, kind, genre);
}

/// Genre-grid model: plain items with already-decoded posters filled in
/// (mirror of `rows_with_posters` for the flat grid).
fn genre_items_with_posters(
    items: &[ItemData],
    posters: &HashMap<String, Arc<Decoded>>,
) -> ModelRc<Item> {
    ModelRc::new(VecModel::from(
        items
            .iter()
            .map(|d| {
                let mut it = d.to_slint();
                if let Some(url) = &d.poster_url {
                    if let Some(dec) = posters.get(url) {
                        it.poster = dec.to_image();
                        it.has_poster = true;
                    }
                }
                it
            })
            .collect::<Vec<_>>(),
    ))
}

/// Fetches the next page of the active genre browse and merges it into the
/// grid. Concurrent load-more triggers coalesce: while a page is in flight
/// they set `genre_pending`, and one more page runs afterwards.
fn genre_fetch_page(app: &Arc<App>) {
    let app = app.clone();
    app.rt.clone().spawn(async move {
        if app.genre_fetching.swap(true, SeqCst) {
            app.genre_pending.store(true, SeqCst);
            return;
        }
        loop {
            // Plan the next page (or stop: nothing active / fully loaded).
            let (plan, token) = {
                let mut state = lock(&app.genre);
                let Some(st) = state.as_mut() else { break };
                if !st.items.is_empty() && !st.has_more {
                    break;
                }
                (st.browser.plan(), st.token)
            };
            app.ui(|ui| ui.set_genre_loading(true));
            let responses = genres::fetch_page(&app.client, &plan).await;
            // Whole selection replaced while this page was in flight?
            if app.genre_gen.load(SeqCst) != token {
                break;
            }
            let page = {
                let mut state = lock(&app.genre);
                let Some(st) = state.as_mut() else { break };
                st.browser.absorb(&plan, &responses)
            };
            let Some(page) = page else { break };
            let mut items: Vec<ItemData> = page.items.iter().map(ItemData::from_preview).collect();
            apply_flags(&app, &mut items);
            let wanted = poster_wanted(&[(String::new(), items.clone())]);
            let has_more = page.has_more;
            let failed = page.failed;
            let queried = page.queried;
            let snapshot = {
                let mut state = lock(&app.genre);
                let Some(st) = state.as_mut() else { break };
                st.items.extend(items);
                st.has_more = has_more;
                st.items.clone()
            };
            tracing::info!(
                "[genres] page: +{} new ({} total), has_more: {}, failed {}/{}",
                page.items.len(),
                snapshot.len(),
                has_more,
                failed,
                queried
            );
            let app2 = app.clone();
            app.ui(move |ui| {
                // ModelRc is not Send: build it here, from the plain snapshot.
                let model = genre_items_with_posters(&snapshot, &lock(&app2.posters));
                ui.set_genre_items(model);
                ui.set_genre_has_more(has_more);
                ui.set_genre_busy(false);
                ui.set_genre_loading(false);
            });
            if failed > 0 {
                app.flash(format!("{failed} of {queried} catalogs didn't respond"));
            }
            if !wanted.is_empty() {
                app.fetch_posters(wanted);
            }
            if !has_more || !app.genre_pending.swap(false, SeqCst) {
                break;
            }
        }
        app.genre_fetching.store(false, SeqCst);
        if app.genre_pending.swap(false, SeqCst) {
            genre_fetch_page(&app);
        }
    });
}

async fn refresh_addons(app: &Arc<App>, auth_key: &str) -> plusflix_core::Result<()> {
    let list = app.api.addon_collection(auth_key).await?;
    *lock(&app.addons) = list;
    // Genre lists are pure manifest data: refresh them with the addons.
    update_genre_options(app);
    Ok(())
}

fn login(app: &Arc<App>, email: String, password: String) {
    if email.trim().is_empty() || password.is_empty() {
        app.ui(|ui| ui.set_status("Enter your e-mail and password.".into()));
        return;
    }
    let app = app.clone();
    app.rt.clone().spawn(async move {
        app.ui(|ui| {
            ui.set_busy(true);
            ui.set_status("".into());
        });
        let result = async {
            let auth = app.api.login(email.trim(), &password).await?;
            refresh_addons(&app, &auth.auth_key).await?;
            plusflix_core::Result::Ok(auth)
        }
        .await;
        match result {
            Ok(auth) => {
                lock(&app.store).auth = Some(auth);
                app.save();
                show_profiles(&app);
            }
            Err(e) => app.ui(move |ui| {
                ui.set_busy(false);
                ui.set_status(format!("Login failed: {e}").into());
            }),
        }
    });
}

fn refresh_addons_ui(app: &Arc<App>) {
    let app = app.clone();
    app.rt.clone().spawn(async move {
        let Some(auth) = lock(&app.store).auth.clone() else {
            app.flash("Not signed in.");
            return;
        };
        app.ui(|ui| ui.set_refreshing_addons(true));
        match refresh_addons(&app, &auth.auth_key).await {
            Ok(()) => {
                let waits = app.profile_settings().wait_for_addons;
                let addons = addon_rows(&lock(&app.addons), &waits);
                app.ui(move |ui| {
                    ui.set_addons(ModelRc::new(VecModel::from(addons)));
                    ui.set_refreshing_addons(false);
                });
                app.flash("Addon list refreshed.");
            }
            Err(e) => {
                app.ui(|ui| ui.set_refreshing_addons(false));
                app.flash(format!("Refresh failed: {e}"));
            }
        }
    });
}

fn logout(app: &Arc<App>) {
    let key = lock(&app.store).auth.take().map(|a| a.auth_key);
    app.save();
    lock(&app.addons).clear();
    let app = app.clone();
    app.rt.clone().spawn(async move {
        if let Some(key) = key {
            let _ = app.api.logout(&key).await;
        }
        app.ui(|ui| {
            ui.set_status("".into());
            ui.set_screen("login".into());
        });
    });
}

fn save_settings(app: &Arc<App>, ui: &MainWindow) {
    let Some(pid) = app.profile_id() else { return };
    let max = ui.get_max_size().trim().replace(',', ".");
    let max_filesize_gb = if max.is_empty() {
        None
    } else {
        max.parse::<f64>().ok().filter(|g| *g > 0.0)
    };
    if !max.is_empty() && max_filesize_gb.is_none() {
        app.flash("Max file size must be a positive number of GB.");
        return;
    }
    // Set from the per-addon "Wait for this addon" switches
    // (toggle-addon-wait); keep whatever the switches last persisted.
    let wait_for_addons = app.profile_settings().wait_for_addons;
    let settings = Settings {
        audio_language: lang_from_index(ui.get_audio_idx()),
        language_mode: if ui.get_audio_mode_idx() == 1 {
            LanguageMode::Must
        } else {
            LanguageMode::Prefer
        },
        subtitle_language: lang_from_index(ui.get_sub_idx()),
        subtitle_requirement: SUBREQ
            .get(ui.get_subreq_idx().max(0) as usize)
            .copied()
            .unwrap_or_default(),
        min_quality: Quality::CHOICES
            .get(ui.get_minq_idx().max(0) as usize)
            .copied()
            .unwrap_or_default(),
        max_filesize_gb,
        wait_for_addons,
        player: match ui.get_player_idx() {
            1 => PlayerKind::Vlc,
            _ => PlayerKind::Mpv,
        },
    };
    if let Ok(p) = lock(&app.store).profile_mut(&pid) {
        p.settings = settings;
    }
    app.save();
    app.flash("Settings saved.");
}

// ---------------------------------------------------------------- language

/// The single supported-languages table: (locale code, display name).
/// Drives the language dropdown model and the locale<->index mapping.
const LANGUAGES: &[(&str, &str)] = &[("en", "English"), ("pl", "Polski")];

/// Embedded English locale JSON, used as the fallback for every locale.
const EN_LOCALE_JSON: &str = include_str!("../locales/en.json");

/// Embedded locale JSON for a supported code; `None` for unknown codes.
/// Adding a locale = one `include_str!` arm here + one row in `LANGUAGES`.
fn locale_json(code: &str) -> Option<&'static str> {
    match code {
        "en" => Some(EN_LOCALE_JSON),
        "pl" => Some(include_str!("../locales/pl.json")),
        _ => None,
    }
}

/// Load translations for `locale` from the embedded locale JSONs, falling
/// back to English for unknown or unparsable locales.
fn load_translations(locale: &str) -> Translations {
    match locale_json(locale).map(|json| Translations::from_json(json, EN_LOCALE_JSON)) {
        Some(Ok(t)) => t,
        Some(Err(e)) => {
            tracing::warn!("could not parse translations for {locale}: {e}");
            Translations::from_json(EN_LOCALE_JSON, EN_LOCALE_JSON)
                .expect("bundled locales/en.json must parse")
        }
        None => {
            tracing::warn!("unknown locale {locale:?}, falling back to English");
            Translations::from_json(EN_LOCALE_JSON, EN_LOCALE_JSON)
                .expect("bundled locales/en.json must parse")
        }
    }
}

/// Display name of a locale code for the language dropdown.
fn language_display_name(code: &str) -> &'static str {
    LANGUAGES
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, name)| *name)
        .unwrap_or("English")
}

/// Language dropdown index for a locale code; unknown codes map to English (0).
fn locale_to_index(code: &str) -> i32 {
    LANGUAGES
        .iter()
        .position(|(c, _)| *c == code)
        .map(|i| i as i32)
        .unwrap_or(0)
}

/// Locale code for a language dropdown index; out-of-range maps to "en".
fn index_to_locale(idx: usize) -> String {
    LANGUAGES
        .get(idx)
        .map(|(code, _)| (*code).to_owned())
        .unwrap_or_else(|| "en".to_owned())
}

/// Translated string for `key` under the currently loaded translations.
fn tr(app: &App, key: &str) -> String {
    lock(&app.translations).t(key)
}

/// Pushes the locale and every translated UI string into the Slint properties.
/// Called at startup and after each language switch.
fn apply_language(app: &Arc<App>) {
    let locale = lock(&app.store).locale.clone();
    let name = language_display_name(&locale).to_owned();
    let idx = locale_to_index(&locale);
    let names: Vec<slint::SharedString> =
        LANGUAGES.iter().map(|(_, name)| (*name).into()).collect();
    let nav = [
        ("nav_home", "home"),
        ("nav_movies", "movies"),
        ("nav_series", "series"),
        ("nav_favorites", "favorites"),
    ]
    .map(|(key, page)| NavTab {
        label: tr(app, key).into(),
        page: page.into(),
    })
    .to_vec();
    let app2 = app.clone();
    app.ui(move |ui| {
        ui.set_app_language(locale.into());
        ui.set_app_language_name(name.into());
        // Programmatic index sync: guard so it cannot re-enter
        // `language_changed` through the `changed language-idx` handler.
        app2.syncing_language_selector
            .store(true, std::sync::atomic::Ordering::SeqCst);
        ui.set_language_idx(idx);
        app2.syncing_language_selector
            .store(false, std::sync::atomic::Ordering::SeqCst);
        ui.set_language_names(ModelRc::new(VecModel::from(names)));
        ui.set_nav_tabs(ModelRc::new(VecModel::from(nav)));
        ui.set_t_playback_prefix(tr(&app2, "playback_settings_prefix").into());
        ui.set_t_audio(tr(&app2, "audio_section").into());
        ui.set_t_pref_audio_label(tr(&app2, "preferred_audio_language_label").into());
        ui.set_t_pref_audio_hint(tr(&app2, "preferred_audio_language_hint").into());
        ui.set_t_audio_req_label(tr(&app2, "audio_requirement_label").into());
        ui.set_t_audio_req_hint(tr(&app2, "audio_requirement_hint").into());
        ui.set_audio_mode_names(ModelRc::new(VecModel::from(vec![
            tr(&app2, "audio_prefer").into(),
            tr(&app2, "audio_must").into(),
        ])));
        ui.set_t_subtitles(tr(&app2, "subtitles_section").into());
        ui.set_t_sub_lang_label(tr(&app2, "subtitle_language_label").into());
        ui.set_t_sub_req_label(tr(&app2, "subtitle_requirement_label").into());
        ui.set_t_sub_req_hint(tr(&app2, "subtitle_requirement_hint").into());
        ui.set_subreq_cards(ModelRc::new(VecModel::from(vec![
            tr(&app2, "subtitle_req_required").into(),
            tr(&app2, "subtitle_req_if_audio_differs").into(),
            tr(&app2, "subtitle_req_not_required").into(),
        ])));
        ui.set_t_quality(tr(&app2, "quality_section").into());
        ui.set_t_minq_label(tr(&app2, "min_quality_label").into());
        ui.set_t_minq_hint(tr(&app2, "min_quality_hint").into());
        ui.set_t_maxsize_label(tr(&app2, "max_filesize_label").into());
        ui.set_t_maxsize_hint(tr(&app2, "max_filesize_hint").into());
        ui.set_t_maxsize_placeholder(tr(&app2, "max_filesize_placeholder").into());
        ui.set_t_player_label(tr(&app2, "preferred_player_label").into());
        ui.set_t_player_hint(tr(&app2, "preferred_player_hint").into());
        ui.set_t_language(tr(&app2, "language_section").into());
        ui.set_t_iface_lang_label(tr(&app2, "interface_language_label").into());
        ui.set_t_iface_lang_hint(tr(&app2, "interface_language_hint").into());
        ui.set_t_addons_prefix(tr(&app2, "addons_section_prefix").into());
        ui.set_t_addons_disclaimer(tr(&app2, "addons_disclaimer").into());
        ui.set_t_addon_wait_tooltip(tr(&app2, "addon_wait_tooltip").into());
        ui.set_t_refresh(tr(&app2, "refresh").into());
        ui.set_t_refreshing(tr(&app2, "refreshing").into());
        // Genre browsing strings (locale files carry the genre-name table).
        ui.set_t_genres(tr(&app2, "genres_label").into());
        ui.set_t_all_genres(tr(&app2, "all_genres").into());
        ui.set_t_movies_title(tr(&app2, "movies_title").into());
        ui.set_t_series_title(tr(&app2, "series_title").into());
        ui.set_t_clear_genre(tr(&app2, "clear_genre_filter").into());
        ui.set_t_no_genre_prefix(tr(&app2, "no_genre_results_prefix").into());
        ui.set_t_no_genre_suffix(tr(&app2, "no_genre_results_suffix").into());
    });
    // Rebuild the translated genre-name lists for the new locale (the
    // canonical lists stay untouched — only their display twins change).
    update_genre_options(app);
}

/// `language-changed` from the Settings dropdown: persist the locale in the
/// store, reload the translations and re-apply every translated string.
/// `idx` is the new language dropdown index.
fn language_changed(app: &Arc<App>, idx: usize) {
    // Ignore callbacks fired by `apply_language`'s programmatic index sync.
    if app
        .syncing_language_selector
        .load(std::sync::atomic::Ordering::SeqCst)
    {
        return;
    }
    let locale = index_to_locale(idx);
    // Same-locale check against the active app state, not just the store file.
    if lock(&app.store).locale == locale {
        return;
    }
    lock(&app.store).locale = locale.clone();
    app.save();
    *lock(&app.translations) = load_translations(&locale);
    apply_language(app);
}

// ---------------------------------------------------------------- wiring

fn wire(ui: &MainWindow, app: &Arc<App>) {
    let a = app.clone();
    ui.on_login(move |email, pw| login(&a, email.to_string(), pw.to_string()));
    let a = app.clone();
    ui.on_logout(move || logout(&a));
    let a = app.clone();
    ui.on_select_profile(move |id| select_profile(&a, id.to_string()));
    let a = app.clone();
    ui.on_open_profile_edit(move |id| {
        let id = id.to_string();
        let (name, icon, color) = if id.is_empty() {
            (
                String::new(),
                PROFILE_ICONS[0].to_owned(),
                PROFILE_COLORS[0].to_owned(),
            )
        } else {
            match lock(&a.store).profile(&id) {
                Ok(p) => (p.name.clone(), p.icon.clone(), p.color.clone()),
                Err(_) => return,
            }
        };
        a.ui(move |ui| {
            ui.set_edit_open(true);
            ui.set_edit_id(id.into());
            ui.set_edit_name(name.into());
            ui.set_edit_icon(icon.into());
            ui.set_edit_color(color.into());
            ui.set_confirm_del(false);
        });
    });
    let a = app.clone();
    ui.on_save_profile(move |id, name, icon, color| {
        let id = id.to_string();
        let name = name.trim().to_owned();
        if name.is_empty() {
            a.ui(|ui| ui.set_status("Profile name cannot be empty.".into()));
            return;
        }
        let (icon, color) = (icon.to_string(), color.to_string());
        let mut failed = None;
        {
            let mut st = lock(&a.store);
            if id.is_empty() {
                match st.add_profile(&name).map(|p| p.id.clone()) {
                    Ok(pid) => {
                        if let Ok(p) = st.profile_mut(&pid) {
                            p.icon = icon.clone();
                            p.color = color.clone();
                        }
                    }
                    Err(e) => failed = Some(e.to_string()),
                }
            } else if let Ok(p) = st.profile_mut(&id) {
                p.name = name;
                p.icon = icon;
                p.color = color;
            }
        }
        if let Some(e) = failed {
            a.ui(move |ui| ui.set_status(e.into()));
            return;
        }
        a.save();
        show_profiles(&a);
    });
    let a = app.clone();
    ui.on_delete_profile(move |id| {
        lock(&a.store).remove_profile(id.as_str());
        a.save();
        show_profiles(&a);
    });
    let a = app.clone();
    ui.on_switch_profile(move || {
        tracing::info!("switch-profile");
        let a = a.clone();
        a.rt.clone().spawn(async move { show_profiles(&a) });
    });
    let a = app.clone();
    // "Wait for this addon" switch on a Settings -> Addons row: add/remove
    // the addon name in the profile's persisted wait_for_addons list.
    ui.on_toggle_addon_wait(move |name, checked| {
        let name = name.to_string();
        let Some(pid) = a.profile_id() else { return };
        let mut waits = a.profile_settings().wait_for_addons;
        if checked {
            if !waits.iter().any(|w| w == &name) {
                waits.push(name.clone());
            }
        } else {
            waits.retain(|w| w != &name);
        }
        if let Ok(p) = lock(&a.store).profile_mut(pid.as_str()) {
            p.settings.wait_for_addons = waits.clone();
        }
        a.save();
        let addons = addon_rows(&lock(&a.addons), &waits);
        a.ui(move |ui| {
            ui.set_addons(ModelRc::new(VecModel::from(addons)));
        });
        a.flash("Settings saved.");
    });
    let a = app.clone();
    ui.on_navigate(move |page| load_page(&a, page.as_str()));
    let a = app.clone();
    ui.on_open_item(move |id, kind| open_item(&a, id.to_string(), kind.to_string()));
    let a = app.clone();
    ui.on_close_detail(move || {
        *lock(&a.detail) = None;
        a.ui(|ui| ui.set_detail_open(false));
    });
    let a = app.clone();
    ui.on_toggle_favorite(move || toggle_favorite(&a));
    let a = app.clone();
    ui.on_select_episode(move |id| select_episode(&a, id.to_string()));
    let a = app.clone();
    {
        let weak = ui.as_weak();
        ui.on_season_changed(move |idx| {
            if let Some(ui) = weak.upgrade() {
                season_changed(&a, &ui, idx);
            }
        });
    }
    let a = app.clone();
    {
        let weak = ui.as_weak();
        ui.on_toggle_watched(move || {
            if let Some(ui) = weak.upgrade() {
                toggle_watched(&a, &ui);
            }
        });
    }
    let a = app.clone();
    ui.on_remove_progress(move |id| remove_progress(&a, id.to_string()));
    let a = app.clone();
    ui.on_toggle_favorite_card(move |id, kind, name, poster| {
        card_toggle_favorite(
            &a,
            id.to_string(),
            kind.to_string(),
            name.to_string(),
            poster.to_string(),
        );
    });
    let a = app.clone();
    ui.on_toggle_watched_card(move |id, kind| {
        card_toggle_watched(&a, id.to_string(), kind.to_string());
    });
    let a = app.clone();
    ui.on_play(move |kind| play(&a, kind.to_string(), None));
    let a = app.clone();
    let weak = ui.as_weak();
    ui.on_play_source(move |key| {
        let kind = weak.upgrade().map(|ui| ui.get_play_player().to_string());
        play(&a, kind.unwrap_or_default(), Some(key.to_string()));
    });
    let a = app.clone();
    ui.on_cancel_playback(move || {
        if let Some(cancel) = lock(&a.cancel_playback).clone() {
            cancel.store(true, SeqCst);
        }
    });
    let a = app.clone();
    ui.on_next_source(move || {
        if let Some(skip) = lock(&a.skip_playback).clone() {
            skip.store(true, SeqCst);
        }
    });
    let a = app.clone();
    let weak = ui.as_weak();
    ui.on_save_settings(move || {
        if let Some(ui) = weak.upgrade() {
            save_settings(&a, &ui);
        }
    });
    let a = app.clone();
    ui.on_refresh_addons(move || refresh_addons_ui(&a));
    let a = app.clone();
    ui.on_search(move |q| do_search(&a, q.to_string()));
    let a = app.clone();
    ui.on_language_changed(move |idx| language_changed(&a, idx as usize));
    let a = app.clone();
    ui.on_genre_chosen(move |genre| genre_chosen(&a, genre.as_str()));
    let a = app.clone();
    ui.on_detail_genre_chosen(move |genre| detail_genre_chosen(&a, genre.as_str()));
    let a = app.clone();
    ui.on_genre_catalog_chosen(move |idx| genre_catalog_chosen(&a, idx));
    let a = app.clone();
    ui.on_genre_load_more(move || genre_fetch_page(&a));
}

fn startup(app: &Arc<App>) {
    let auth = lock(&app.store).auth.clone();
    match auth {
        None => app.ui(|ui| ui.set_screen("login".into())),
        Some(auth) => {
            let app = app.clone();
            app.rt.clone().spawn(async move {
                if let Err(e) = refresh_addons(&app, &auth.auth_key).await {
                    tracing::warn!("could not load addons: {e}");
                }
                show_profiles(&app);
                // Stored session expired or was revoked: back to the login screen.
                if lock(&app.addons).is_empty() {
                    app.flash("Could not load your Stremio addons – check your connection or sign in again.");
                }
            });
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let client = AddonClient::new()?;
    let store_path = store::default_path();
    let store = Store::load(&store_path).unwrap_or_else(|e| {
        tracing::warn!("ignoring unreadable store {}: {e}", store_path.display());
        Store::default()
    });
    // Interface translations: the locale from the store, falling back to
    // English if unknown/broken (all locale files are embedded at compile time).
    let translations = load_translations(&store.locale);

    let ui = MainWindow::new()?;
    #[cfg(target_family = "windows")]
    enable_dark_title_bar();
    let app = Arc::new(App {
        rt: rt.handle().clone(),
        ui: ui.as_weak(),
        store_path,
        store: Mutex::new(store),
        translations: Mutex::new(translations),
        syncing_language_selector: AtomicBool::new(false),
        api: StremioApi::new(client.http().clone()),
        client,
        addons: Mutex::new(Vec::new()),
        profile: Mutex::new(None),
        detail: Mutex::new(None),
        known_episodes: Mutex::new(HashMap::new()),
        source_states: Mutex::new(HashMap::new()),
        rows_cache: Mutex::new(Vec::new()),
        load_gen: std::sync::atomic::AtomicUsize::new(0),
        genre: Mutex::new(None),
        genre_gen: std::sync::atomic::AtomicUsize::new(0),
        genre_fetching: AtomicBool::new(false),
        genre_pending: AtomicBool::new(false),
        playing: AtomicBool::new(false),
        cancel_playback: Mutex::new(None),
        skip_playback: Mutex::new(None),
        last_played: Mutex::new(HashMap::new()),
        posters: Mutex::new(HashMap::new()),
        poster_slots: Semaphore::new(8),
    });
    wire(&ui, &app);
    apply_language(&app);
    // REEL_DEBUG_LAYOUT=1: draw outlines around buttons/inputs and their icons
    // so centering offsets are visible in screenshots.
    if std::env::var("REEL_DEBUG_LAYOUT").is_ok_and(|v| v == "1") {
        ui.global::<Theme>().set_debug_layout(true);
    }
    ui.set_player_names(ModelRc::new(VecModel::from(vec![
        slint::SharedString::from("mpv"),
        "VLC".into(),
    ])));
    startup(&app);
    ui.run()?;
    Ok(())
}
