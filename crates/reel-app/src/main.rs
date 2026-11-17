//! Reel – a Stremio-backed player. UI glue: Slint on the main thread, everything
//! else (network, failover sessions, mpv) on a tokio runtime.

slint::include_modules!();

use reel_core::addon::{Addon, AddonClient};
use reel_core::home;
use reel_core::lang::Lang;
use reel_core::loader::SourceLoader;
use reel_core::model::{Meta, MetaPreview};
use reel_core::mpv::{MpvConfig, MpvPlayer};
use reel_core::player::{LoadRequest, Player, PlayerEvent};
use reel_core::pool::{PoolStats, SourcePool};
use reel_core::session::{self, Outcome, SessionConfig, SessionEvent};
use reel_core::settings::{LanguageMode, PlayerKind, Quality, Settings, SubtitleRequirement};
use reel_core::store::{
    self, Favorite, Profile, ProgressCard, Store, MAX_PROFILES, PROFILE_COLORS, PROFILE_ICONS,
};
use reel_core::stremio::StremioApi;
use reel_core::vlc::{VlcConfig, VlcPlayer};
use slint::{
    ComponentHandle, Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel, Weak,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::runtime::Handle;
use tokio::sync::Semaphore;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
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

struct App {
    rt: Handle,
    ui: Weak<MainWindow>,
    store_path: PathBuf,
    store: Mutex<Store>,
    api: StremioApi,
    client: AddonClient,
    addons: Mutex<Vec<Addon>>,
    profile: Mutex<Option<String>>,
    detail: Mutex<Option<Arc<Detail>>>,
    playing: AtomicBool,
    /// Set by the Cancel button while a playback task is running; the task
    /// watches it and tears the player down.
    cancel_playback: Mutex<Option<Arc<AtomicBool>>>,
    posters: Mutex<HashMap<String, Arc<Decoded>>>,
    poster_slots: Semaphore,
    /// Episode ids seen for each series (used for the "all episodes watched" badge).
    known_episodes: Mutex<HashMap<String, Vec<String>>>,
    /// Last rendered home/movies/series rows, so the account sync can patch
    /// the Continue Watching row without re-fetching catalogs.
    rows_cache: Mutex<Vec<(String, Vec<ItemData>)>>,
    /// Bumped on every page load; background catalog tasks only patch the UI
    /// while their generation is current.
    load_gen: std::sync::atomic::AtomicUsize,
}

/// Plain-data twin of the Slint `Item` (Slint images are not `Send`).
#[derive(Clone)]
struct ItemData {
    id: String,
    kind: String,
    name: String,
    year: String,
    poster_url: Option<String>,
    /// Series/movie id a continue-watching card opens (empty otherwise).
    title_id: String,
    /// "S02E05  Title" on continue-watching cards.
    episode_label: String,
    /// Watch progress 0..1 (0 = no bar).
    progress: f32,
    watched: bool,
    fav: bool,
    is_cw: bool,
}

impl ItemData {
    fn from_preview(m: &MetaPreview) -> Self {
        Self {
            id: m.id.clone(),
            kind: m.kind.clone(),
            name: m.name.clone(),
            year: m.release_info.clone().unwrap_or_default(),
            poster_url: m.poster.clone(),
            title_id: String::new(),
            episode_label: String::new(),
            progress: 0.0,
            watched: false,
            fav: false,
            is_cw: false,
        }
    }
    fn from_favorite(f: &Favorite) -> Self {
        Self {
            id: f.id.clone(),
            kind: f.kind.clone(),
            name: f.name.clone(),
            year: String::new(),
            poster_url: f.poster.clone(),
            title_id: String::new(),
            episode_label: String::new(),
            progress: 0.0,
            watched: false,
            fav: false,
            is_cw: false,
        }
    }
    /// A Continue Watching card from a progress entry (must have card info).
    fn from_progress(pr: &reel_core::store::Progress) -> Option<Self> {
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
        Some(Self {
            id: video_id,
            kind: card.kind.clone(),
            name: card.title.clone(),
            year: String::new(),
            poster_url: card.poster.clone(),
            title_id: card.title_id.clone(),
            episode_label: card.episode_label.clone().unwrap_or_default(),
            progress: frac,
            watched: false,
            fav: false,
            is_cw: true,
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
            progress: self.progress,
            watched: self.watched,
            fav: self.fav,
            is_cw: self.is_cw,
            ..Default::default()
        }
    }
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
    snapshot: &[(String, Vec<ItemData>)],
    posters: &HashMap<String, Arc<Decoded>>,
) -> Vec<RowData> {
    snapshot
        .iter()
        .map(|(title, items)| RowData {
            title: title.clone().into(),
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
                        it
                    })
                    .collect::<Vec<_>>(),
            )),
        })
        .collect()
}

fn addon_rows(addons: &[Addon]) -> Vec<AddonData> {
    addons
        .iter()
        .map(|a| {
            let m = &a.manifest;
            AddonData {
                name: a.name().to_owned().into(),
                kinds: m.types.join(", ").into(),
                version: m.version.clone().into(),
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
    }
}

/// MPV or VLC, chosen per playback and remembered in the profile settings.
enum AnyPlayer {
    Mpv(Box<MpvPlayer>),
    Vlc(VlcPlayer),
}

impl Player for AnyPlayer {
    async fn load(&mut self, req: LoadRequest) -> reel_core::Result<()> {
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

    fn fetch_posters(self: &Arc<Self>, wanted: Vec<(String, String)>) {
        for (id, url) in wanted {
            let app = self.clone();
            self.rt.spawn(async move {
                if let Some(dec) = app.poster(&url).await {
                    app.ui(move |ui| apply_poster(ui, &id, &dec));
                }
            });
        }
    }
}

fn patch_items(items: &ModelRc<Item>, id: &str, img: &Image) {
    for i in 0..items.row_count() {
        if let Some(mut it) = items.row_data(i) {
            if it.id.as_str() == id {
                it.poster = img.clone();
                it.has_poster = true;
                items.set_row_data(i, it);
            }
        }
    }
}

fn apply_poster(ui: &MainWindow, id: &str, dec: &Decoded) {
    let img = dec.to_image();
    let rows = ui.get_rows();
    for r in 0..rows.row_count() {
        if let Some(row) = rows.row_data(r) {
            patch_items(&row.items, id, &img);
        }
    }
    patch_items(&ui.get_favorites(), id, &img);
    patch_items(&ui.get_search_results(), id, &img);
    if ui.get_detail_id().as_str() == id {
        ui.set_detail_poster(img);
        ui.set_detail_has_poster(true);
    }
}

fn poster_wanted(rows: &[(String, Vec<ItemData>)]) -> Vec<(String, String)> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for (_, items) in rows {
        for it in items {
            if let Some(url) = &it.poster_url {
                if seen.insert(it.id.clone()) {
                    out.push((it.id.clone(), url.clone()));
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
    let name = {
        let mut st = lock(&app.store);
        let Ok(p) = st.profile(&id) else { return };
        let name = p.name.clone();
        st.last_profile = Some(id.clone());
        name
    };
    app.save();
    *lock(&app.profile) = Some(id);
    app.ui(move |ui| {
        ui.set_profile_name(name.into());
        ui.set_screen("main".into());
    });
    load_page(app, "home");
    sync_account_cw(app);
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
        let mut cw: Vec<&reel_core::stremio::LibraryItem> =
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
                            episode_id: (item.kind == "series").then(|| video_id.clone()),
                            episode_label: None,
                        }),
                    },
                );
            }
        }
        app.save();
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
            Some(row) if row.0 == "Continue watching" => row.1 = cw_items,
            _ => data.insert(0, ("Continue watching".to_owned(), cw_items)),
        }
        let wanted = poster_wanted(&data);
        *lock(&app.rows_cache) = data.clone();
        app.ui(move |ui| {
            if ui.get_page().as_str() == "home" {
                let model: Vec<RowData> = data
                    .iter()
                    .map(|(title, items)| RowData {
                        title: title.clone().into(),
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
    ui.set_lang_names(ModelRc::new(VecModel::from(lang_names())));
    ui.set_quality_names(ModelRc::new(VecModel::from(
        Quality::CHOICES
            .iter()
            .map(|q| slint::SharedString::from(q.label()))
            .collect::<Vec<_>>(),
    )));
    ui.set_subreq_names(ModelRc::new(VecModel::from(vec![
        slint::SharedString::from("Required"),
        "Required only if the audio isn't my language".into(),
        "Not required".into(),
    ])));
    ui.set_audio_idx(lang_index(s.audio_language));
    ui.set_audio_must(s.language_mode == LanguageMode::Must);
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
}

fn load_page(app: &Arc<App>, page: &str) {
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
                let mut data: Vec<(String, Vec<ItemData>)> = Vec::new();
                if page == "home" && !cw_items.is_empty() {
                    data.push(("Continue watching".to_owned(), cw_items));
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
                if let Some((_, cw)) = data.first() {
                    let wanted = poster_wanted(&[(String::new(), cw.clone())]);
                    app.fetch_posters(wanted);
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
                                    cache.push((job.title.clone(), items.clone()));
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
            let addons = addon_rows(&lock(&app.addons));
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
                        label: r.label.into(),
                        status: status.into(),
                        ok: r.rejected.is_none(),
                    }
                })
                .collect();
            let text = summary(&stats);
            a.ui(move |ui| {
                ui.set_sources_summary(text.into());
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
        let mut episodes: Vec<_> = meta
            .videos
            .iter()
            .filter(|v| v.season.is_some() && v.episode.is_some())
            .collect();
        episodes.sort_by_key(|v| (v.season == Some(0), v.season, v.episode));
        let episode_rows: Vec<(String, String)> = episodes
            .iter()
            .map(|v| {
                let label = format!(
                    "S{:02}E{:02}  {}",
                    v.season.unwrap_or(0),
                    v.episode.unwrap_or(0),
                    v.title.clone().unwrap_or_default()
                );
                (v.id.clone(), label.trim_end().to_owned())
            })
            .collect();
        if is_series {
            lock(&app.known_episodes).insert(
                meta.id.clone(),
                episode_rows.iter().map(|(id, _)| id.clone()).collect(),
            );
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
                .filter(|e| episode_rows.iter().any(|(id, _)| id == e))
                .or_else(|| episode_rows.first().map(|(id, _)| id.clone()))
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
        // Which episodes of this series are watched (for the per-episode checks).
        let watched_eps: std::collections::HashSet<String> = {
            let pid = app.profile_id();
            let st = lock(&app.store);
            match pid.and_then(|p| st.profile(&p).ok()) {
                Some(p) => p
                    .watched
                    .iter()
                    .filter(|k| k.starts_with(&format!("{}:", meta.id)))
                    .cloned()
                    .collect(),
                None => Default::default(),
            }
        };

        let info = [
            meta.release_info.clone(),
            meta.runtime.clone(),
            Some(meta.genres.join(", ")),
        ]
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("  ·  ");
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
            app.ui(move |ui| {
                ui.set_detail_id(id.into());
                ui.set_detail_title(title.into());
                ui.set_detail_info(info.into());
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
                ui.set_episodes(ModelRc::new(VecModel::from(
                    episode_rows
                        .into_iter()
                        .map(|(id, label)| EpisodeData {
                            watched: watched_eps.contains(&id),
                            id: id.into(),
                            label: label.into(),
                        })
                        .collect::<Vec<_>>(),
                )));
                ui.set_selected_episode(selected.into());
                ui.set_sources_summary("Searching sources…".into());
                ui.set_source_rows(ModelRc::default());
                ui.set_sources_open(false);
                ui.set_detail_open(true);
            });
        }
        if let Some(url) = poster {
            app.fetch_posters(vec![(meta.id.clone(), url)]);
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
        });
        start_sources(&app, &detail, id);
    });
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

fn play(app: &Arc<App>, kind: String) {
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
    let (settings, resume) = {
        let mut st = lock(&app.store);
        let (settings, resume) = match st.profile(&pid) {
            Ok(p) => (p.settings.clone(), p.resume_position(&sources.video_id)),
            Err(_) => (Settings::default(), 0.0),
        };
        if let Ok(p) = st.profile_mut(&pid) {
            if p.settings.player != player_kind {
                p.settings.player = player_kind;
                app.save();
            }
        }
        (settings, resume)
    };
    let mut settings = settings;
    settings.player = player_kind;
    // Card info so the entry shows up in Continue Watching.
    let card = ProgressCard {
        kind: detail.kind.clone(),
        title_id: detail.meta.id.clone(),
        title: detail.meta.name.clone(),
        poster: detail.meta.poster.clone(),
        episode_id: (detail.kind == "series").then(|| sources.video_id.clone()),
        episode_label: detail
            .meta
            .videos
            .iter()
            .find(|v| v.id == sources.video_id)
            .and_then(|v| {
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
            msg = run_playback(&app, &sources, &pid, settings, resume, card) => msg,
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

async fn run_playback(
    app: &Arc<App>,
    sources: &Arc<Sources>,
    pid: &str,
    settings: Settings,
    resume: f64,
    card: ProgressCard,
) -> Option<String> {
    let mut player = match settings.player {
        PlayerKind::Vlc => match VlcPlayer::spawn(&VlcConfig::default()).await {
            Ok(p) => AnyPlayer::Vlc(p),
            Err(e) => return Some(format!("{e}")),
        },
        PlayerKind::Mpv => match MpvPlayer::spawn(&MpvConfig::default()).await {
            Ok(p) => AnyPlayer::Mpv(Box::new(p)),
            Err(e) => return Some(format!("{e} – install mpv and make sure it is on PATH.")),
        },
    };
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
                    SessionEvent::Playing { label } => Some(format!("Playing: {label}")),
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
        &SessionConfig::default(),
        resume,
        Some(tx),
    )
    .await;
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

async fn refresh_addons(app: &Arc<App>, auth_key: &str) -> reel_core::Result<()> {
    let list = app.api.addon_collection(auth_key).await?;
    *lock(&app.addons) = list;
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
            reel_core::Result::Ok(auth)
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
                let addons = addon_rows(&lock(&app.addons));
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
    let settings = Settings {
        audio_language: lang_from_index(ui.get_audio_idx()),
        language_mode: if ui.get_audio_must() {
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
    ui.on_switch_profile(move || show_profiles(&a));
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
        ui.on_toggle_watched(move || {
            if let Some(ui) = weak.upgrade() {
                toggle_watched(&a, &ui);
            }
        });
    }
    let a = app.clone();
    ui.on_remove_progress(move |id| remove_progress(&a, id.to_string()));
    let a = app.clone();
    ui.on_play(move |kind| play(&a, kind.to_string()));
    let a = app.clone();
    ui.on_cancel_playback(move || {
        if let Some(cancel) = lock(&a.cancel_playback).clone() {
            cancel.store(true, SeqCst);
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

    let ui = MainWindow::new()?;
    let app = Arc::new(App {
        rt: rt.handle().clone(),
        ui: ui.as_weak(),
        store_path,
        store: Mutex::new(store),
        api: StremioApi::new(client.http().clone()),
        client,
        addons: Mutex::new(Vec::new()),
        profile: Mutex::new(None),
        detail: Mutex::new(None),
        known_episodes: Mutex::new(HashMap::new()),
        rows_cache: Mutex::new(Vec::new()),
        load_gen: std::sync::atomic::AtomicUsize::new(0),
        playing: AtomicBool::new(false),
        cancel_playback: Mutex::new(None),
        posters: Mutex::new(HashMap::new()),
        poster_slots: Semaphore::new(8),
    });
    wire(&ui, &app);
    ui.set_player_names(ModelRc::new(VecModel::from(vec![
        slint::SharedString::from("mpv"),
        "VLC".into(),
    ])));
    startup(&app);
    ui.run()?;
    Ok(())
}
