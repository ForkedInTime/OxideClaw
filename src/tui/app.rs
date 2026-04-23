/// App state — all data the render loop needs.
use crate::permissions::PermissionDecision;
use crate::tui::events::AppEvent;
use dirs;
use rand::seq::IndexedRandom;
use ratatui::text::Line;
use serde_json;
use std::time::Instant;
use tokio::sync::oneshot;

// ── Spinner verbs (shown while loading) ──────────────────────────────────────
// Custom themed: video games · medicine · cycling · general whimsy

pub const SPINNER_VERBS: &[&str] = &[
    // ── Video Games ──
    "Respawning",
    "Speed-running",
    "Boss-fighting",
    "Level-grinding",
    "Quest-logging",
    "Power-upping",
    "Side-questing",
    "Combo-breaking",
    "Achievement-hunting",
    "Pixel-pushing",
    "Glitch-hunting",
    "Save-scumming",
    "Min-maxing",
    "Aggro-pulling",
    "Mob-clearing",
    "Buff-stacking",
    "Debuffing",
    "Kiting",
    "GG-ing",
    "Warp-piping",
    "Barrel-rolling",
    "Falcon-punching",
    "Hadoukening",
    "Mushroom-eating",
    "Star-collecting",
    "Coin-farming",
    "XP-grinding",
    "Mana-pooling",
    "Critical-hitting",
    "Dodge-rolling",
    "Parrying",
    "Riposting",
    "Backstabbing",
    "Checkpoint-saving",
    "Fast-traveling",
    "Inventory-managing",
    "Potion-brewing",
    "Raid-leading",
    "Mount-riding",
    "Dungeon-crawling",
    "Elo-climbing",
    "Meta-gaming",
    "Theory-crafting",
    "Noclipping",
    "Wall-jumping",
    "Rocket-jumping",
    "Bunny-hopping",
    "Double-jumping",
    "Grapple-hooking",
    "Zip-lining",
    "Gliding",
    "Loot-dropping",
    "Rage-quitting",
    "Tea-bagging",
    "Noob-tubing",
    "360-noscoping",
    "Camping",
    "Ganking",
    "Farming",
    "Grinding",
    "Pwning",
    "Fragging",
    "Headshot-landing",
    "Killstreak-building",
    "Ulting",
    "Turret-placing",
    "Sniping",
    "Flanking",
    "Rezzing",
    "Tank-swapping",
    "Healer-maining",
    "DPS-parsing",
    "Speedhacking",
    "Emote-spamming",
    "Lore-reading",
    "Easter-egg-hunting",
    "Prestige-ranking",
    "Platinum-hunting",
    "Ironman-moding",
    "Hardcore-surviving",
    "Crafting",
    "Enchanting",
    "Smelting",
    "Mining",
    "Fishing",
    "Woodcutting",
    "Smithing",
    "Fletching",
    "Runecrafting",
    "Thieving",
    "Pickpocketing",
    "Bossing",
    "Raiding",
    "Wiping",
    "Pulling",
    "Kiting",
    "Juking",
    "Flashing",
    "Warding",
    "Last-hitting",
    "Denying",
    "Freezing-lane",
    "Split-pushing",
    "Backdooring",
    "Baron-sneaking",
    "Dragon-stealing",
    "Pentakill-chasing",
    "Skill-shotting",
    "Map-hacking",
    // ── Medicine ──
    "Diagnosing",
    "Prescribing",
    "Auscultating",
    "Palpating",
    "Intubating",
    "Defibrillating",
    "Resuscitating",
    "Suturing",
    "Cauterizing",
    "Anesthetizing",
    "Irrigating",
    "Aspirating",
    "Catheterizing",
    "Biopsying",
    "Transfusing",
    "Inoculating",
    "Vaccinating",
    "Sterilizing",
    "Triaging",
    "Splinting",
    "Bandaging",
    "Debridementing",
    "Dialyzing",
    "Ventilating",
    "Oxygenating",
    "Rehabilitating",
    "Stethoscoping",
    "Operating",
    "Transplanting",
    "Grafting",
    "Excising",
    "Monitoring",
    "Assessing",
    "Consulting",
    "Sedating",
    "Imaging",
    "Scanning",
    "X-raying",
    "MRI-ing",
    "CT-scanning",
    "Ultrasounding",
    "Phlebotomizing",
    "Perfusing",
    "Titrating",
    "Intubating",
    "Extubating",
    "Decompressing",
    "Lavaging",
    "Infusing",
    "Bolusing",
    "Nebulizing",
    "Suctioning",
    "Cannulating",
    "Amputating",
    "Ligating",
    "Dissecting",
    "Ablating",
    "Cryotherapizing",
    "Electroconvulsing",
    "Chemotherapizing",
    "Radiating",
    "Immunosuppressing",
    "Autoclaving",
    "Culturing",
    "Centrifuging",
    "Micropipetting",
    "Incubating",
    "Culturing",
    "Staining",
    "Plating",
    "Swabbing",
    "Tourniquet-ing",
    // ── Cycling ──
    "Pedaling",
    "Sprinting",
    "Drafting",
    "Peloton-ing",
    "Climbing",
    "Descending",
    "Cadence-matching",
    "Time-trialing",
    "Breakaway-ing",
    "Bonking",
    "Hammering",
    "Bridging",
    "Chasing",
    "Attacking",
    "Counter-attacking",
    "Lead-out-ing",
    "Dropping",
    "Surging",
    "Freewheeling",
    "Coasting",
    "Chain-ganging",
    "Echelon-forming",
    "Paceline-riding",
    "Hill-repeating",
    "Interval-training",
    "Tempo-riding",
    "KOM-hunting",
    "Strava-ing",
    "Segment-crushing",
    "Gear-shifting",
    "Cadence-spinning",
    "Aero-tucking",
    "Criterium-racing",
    "Grand-touring",
    "Stage-racing",
    "Yellow-jerseying",
    "Polka-dotting",
    "Green-jerseying",
    "Domestique-ing",
    "GC-contending",
    "Palmares-building",
    "Bunny-hopping",
    "Wheelie-popping",
    "Track-standing",
    "Fixie-skidding",
    "Century-riding",
    "Fondo-finishing",
    "Waterbottle-tossing",
    "Bidon-grabbing",
    "Feed-zone-sprinting",
    "Musette-snatching",
    "Pavé-rattling",
    "Cobblestone-bouncing",
    "Switchback-climbing",
    "Hairpin-cornering",
    "Tarmac-shredding",
    // ── General whimsy ──
    "Thinking",
    "Cogitating",
    "Pondering",
    "Contemplating",
    "Musing",
    "Ruminating",
    "Deliberating",
    "Noodling",
    "Brainstorming",
    "Scheming",
    "Plotting",
    "Architecting",
    "Forging",
    "Concocting",
    "Brewing",
    "Cooking",
    "Simmering",
    "Fermenting",
    "Percolating",
    "Distilling",
    "Synthesizing",
    "Harmonizing",
    "Orchestrating",
    "Composing",
    "Bootstrapping",
    "Compiling",
    "Transpiling",
    "Deploying",
    "Refactoring",
    "Optimizing",
    "Benchmarking",
    "Profiling",
    "Fuzzing",
    "Linting",
    "Formatting",
    "Documenting",
    "Rebasing",
    "Cherry-picking",
    "Bisecting",
    "Stashing",
    "Yak-shaving",
    "Rubber-ducking",
    "Stack-overflowing",
    "Cargo-culting",
    "Bikeshedding",
    "Ship-it-ing",
];

pub const TURN_COMPLETION_VERBS: &[&str] = &[
    "Baked",
    "Brewed",
    "Churned",
    "Cogitated",
    "Cooked",
    "Crunched",
    "Sautéed",
    "Worked",
    "Crafted",
    "Forged",
    "Pedaled",
    "Sprinted",
    "Diagnosed",
    "Prescribed",
    "Operated",
    "Grinded",
    "Speed-ran",
    "Boss-fought",
    "Respawned",
    "Climbed",
    "Hammered",
];

/// Format a token count for display: "1.2k tokens", "3.5k tokens", or "847 tokens".
pub fn format_tokens(n: u64) -> String {
    if n >= 1000 {
        format!("{:.1}k tokens", n as f64 / 1000.0)
    } else {
        format!("{n} tokens")
    }
}

/// Format milliseconds into a human-readable duration string like "3m 44s" or "5s".
pub fn format_duration_ms(ms: u128) -> String {
    if ms < 1000 {
        return format!("{}ms", ms);
    }
    let total_secs = (ms / 1000) as u64;
    if total_secs < 60 {
        return format!("{}s", total_secs);
    }
    let mins = total_secs / 60;
    let secs = total_secs % 60;
    if mins < 60 {
        if secs == 0 {
            return format!("{}m", mins);
        }
        return format!("{}m {}s", mins, secs);
    }
    let hours = mins / 60;
    let remaining_mins = mins % 60;
    if remaining_mins == 0 {
        return format!("{}h", hours);
    }
    format!("{}h {}m", hours, remaining_mins)
}

// ── Chat entries ──────────────────────────────────────────────────────────────

#[derive(Clone)]
pub enum EntryKind {
    User,
    Assistant,
    Thinking, // Model's extended thinking/reasoning (shown when show_thinking_summaries=true)
    ToolCall,
    ToolStream, // Live streaming output from a running tool
    ToolResult,
    Error,
    System,        // Short status messages — dim italic (e.g. "TTS stopped.")
    CommandOutput, // Readable multi-line command output — /doctor, /voice, /help, etc.
}

#[derive(Clone)]
pub struct ChatEntry {
    pub kind: EntryKind,
    pub text: String,
}

impl ChatEntry {
    pub fn user(t: impl Into<String>) -> Self {
        Self {
            kind: EntryKind::User,
            text: t.into(),
        }
    }
    pub fn assistant(t: impl Into<String>) -> Self {
        Self {
            kind: EntryKind::Assistant,
            text: t.into(),
        }
    }
    pub fn tool_call(t: impl Into<String>) -> Self {
        Self {
            kind: EntryKind::ToolCall,
            text: t.into(),
        }
    }
    pub fn tool_result(t: impl Into<String>) -> Self {
        Self {
            kind: EntryKind::ToolResult,
            text: t.into(),
        }
    }
    pub fn error(t: impl Into<String>) -> Self {
        Self {
            kind: EntryKind::Error,
            text: t.into(),
        }
    }
    pub fn system(t: impl Into<String>) -> Self {
        Self {
            kind: EntryKind::System,
            text: t.into(),
        }
    }
    pub fn command_output(t: impl Into<String>) -> Self {
        Self {
            kind: EntryKind::CommandOutput,
            text: t.into(),
        }
    }
    pub fn thinking(t: impl Into<String>) -> Self {
        Self {
            kind: EntryKind::Thinking,
            text: t.into(),
        }
    }
}

// ── Overlay panel (slash command output) ─────────────────────────────────────

pub struct Overlay {
    pub title: String,
    #[allow(dead_code)] // retained for future overlay text search/copy
    pub text: String,
    pub scroll: usize,
    /// Markdown pre-rendered to ratatui Lines — computed once in new(), reused every frame.
    pub rendered: Vec<Line<'static>>,
    /// Selectable items — e.g. session IDs for the session list overlay.
    /// When non-empty, the overlay is interactive: number keys or Enter selects.
    pub selectable_ids: Vec<String>,
    /// Currently highlighted item index (0-based) for arrow key navigation.
    pub selected: usize,
}

impl Overlay {
    pub fn new(title: impl Into<String>, text: impl Into<String>) -> Self {
        let text = text.into();
        let rendered = crate::tui::markdown::render(&text);
        Self {
            title: title.into(),
            text,
            scroll: 0,
            rendered,
            selectable_ids: Vec::new(),
            selected: 0,
        }
    }
    /// Create an interactive overlay with selectable items (e.g. session list).
    pub fn with_items(title: impl Into<String>, text: impl Into<String>, ids: Vec<String>) -> Self {
        let text = text.into();
        let rendered = crate::tui::markdown::render(&text);
        Self {
            title: title.into(),
            text,
            scroll: 0,
            rendered,
            selectable_ids: ids,
            selected: 0,
        }
    }
    pub fn scroll_up(&mut self) {
        self.scroll = self.scroll.saturating_sub(5);
    }
    #[allow(dead_code)]
    pub fn scroll_down(&mut self, total: usize, visible: usize) {
        self.scroll = (self.scroll + 5).min(total.saturating_sub(visible));
    }
    pub fn select_up(&mut self) {
        if !self.selectable_ids.is_empty() {
            self.selected = self.selected.saturating_sub(1);
        }
    }
    pub fn select_down(&mut self) {
        if !self.selectable_ids.is_empty() {
            self.selected = (self.selected + 1).min(self.selectable_ids.len() - 1);
        }
    }
    pub fn is_interactive(&self) -> bool {
        !self.selectable_ids.is_empty()
    }
}

// ── Permission dialog ─────────────────────────────────────────────────────────

pub struct PendingPermission {
    pub tool_name: String,
    pub description: String,
    pub reply: oneshot::Sender<PermissionDecision>,
}

// ── AskUser dialog ────────────────────────────────────────────────────────────

pub struct PendingUserQuestion {
    pub question: String,
    pub reply: oneshot::Sender<String>,
    /// User's answer (typed in the dialog)
    pub input: Vec<char>,
    pub cursor: usize,
}

// ── Watcher handle ────────────────────────────────────────────────────────────

/// Handle to an active file watcher. Owns the `RecommendedWatcher` (drop = stop)
/// and the abort handle for the background task that forwards watch events into
/// the TUI. Dropping drops both, so `app.watcher = None` fully tears it down.
///
/// Field order is deliberate: `forwarder` first so it aborts before `watcher`
/// drops (which closes the notify channel). Combined with the explicit abort
/// in `Drop::drop`, the forwarder terminates promptly regardless of whether
/// the channel close or the abort fires first.
pub struct WatcherHandle {
    pub forwarder: tokio::task::AbortHandle,
    #[allow(dead_code)] // kept alive for its drop side-effect
    pub watcher: notify::RecommendedWatcher,
}

impl Drop for WatcherHandle {
    fn drop(&mut self) {
        self.forwarder.abort();
    }
}

// ── App ───────────────────────────────────────────────────────────────────────

pub struct App {
    pub entries: Vec<ChatEntry>,
    /// Text currently being streamed (incomplete assistant message)
    pub streaming: String,
    pub is_loading: bool,

    // Input line (stored as chars for safe unicode indexing)
    pub input: Vec<char>,
    pub cursor: usize,

    // Chat scroll (number of rendered lines from top to skip)
    pub scroll: usize,
    /// When true, draw_chat always shows the latest content (auto-follow)
    pub follow_bottom: bool,

    pub should_quit: bool,
    pub pending_screen_clear: bool,

    pub input_history: Vec<String>,
    pub history_idx: Option<usize>,
    pub saved_input: Vec<char>,

    pub pending_permission: Option<PendingPermission>,

    /// AskUserQuestion dialog — Claude is waiting for user text input
    pub pending_user_question: Option<PendingUserQuestion>,

    /// Image file to attach to the next user message (set by /image command)
    pub pending_image: Option<String>,

    /// Plan mode: read-only, destructive tools are blocked
    pub plan_mode: bool,

    /// Brief mode: append concise-response instruction to system prompt
    pub brief_mode: bool,

    /// "By the way" note — prepended to the next user message, then cleared
    pub btw_note: Option<String>,

    /// Overlay panel for slash command output (dismissed by any key).
    /// When Some, renders as a scrollable popup over the chat; does NOT go into entries.
    pub overlay: Option<Overlay>,

    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,

    pub username: String,
    pub model: String,
    /// model with "claude-" prefix and version suffix stripped — cached to avoid per-frame alloc
    pub model_short: String,
    /// Thinking/effort level shown in banner ("low", "medium", "high", etc.)
    pub effort: Option<String>,
    /// Current working directory with ~ substitution — computed once (CWD doesn't change mid-session)
    pub cached_cwd: String,
    /// Optional label shown in the banner (from bannerOrgDisplay in ~/.claude/config.json)
    pub banner_label: Option<String>,
    /// Current session name (displayed in title bar)
    pub session_name: String,

    /// Recent sessions for the welcome screen: (name, id_prefix, preview)
    pub recent_sessions: Vec<(String, String, String)>,

    /// True until the first user message is sent (shows welcome screen)
    pub show_welcome: bool,

    /// True when vim editing mode is enabled
    pub vim_enabled: bool,
    /// True when in vim normal mode; false = insert mode (only relevant when vim_enabled)
    pub vim_normal: bool,
    /// Pending first char of a two-char vim command (e.g. 'd' waiting for 'd')
    pub vim_pending: Option<char>,
    /// Accumulates live bash output lines for the current tool call
    pub tool_stream_buf: String,
    /// Handle to the current API task — used to abort it on Escape
    pub api_task: Option<tokio::task::AbortHandle>,

    /// Active UI theme ("dark", "light", "solarized")
    pub theme: String,

    /// True while a voice recording is in progress
    pub voice_recording: bool,

    /// Handle to the voice recording task
    pub voice_task: Option<tokio::task::AbortHandle>,

    /// Graceful-stop channel for the active recording (sends SIGINT to recorder process)
    pub voice_stop_tx: Option<tokio::sync::oneshot::Sender<()>>,

    /// Stop channel for active TTS playback — send () to interrupt mid-speech.
    pub tts_stop_tx: Option<tokio::sync::oneshot::Sender<()>>,
    /// Pending package-manager install command — handled in run_loop (needs terminal access).
    pub pending_install: Option<String>,

    /// Per-turn token usage: (tokens_in, tokens_out) for each completed turn.
    pub turn_costs: Vec<(u64, u64)>,

    /// Whimsical verb shown in the spinner while loading (e.g. "Combobulating")
    pub spinner_verb: String,
    /// Spinner style: "themed" (fun verbs), "minimal" ("Working"), "silent" (no verb)
    pub spinner_style: String,
    /// When the current loading turn started — used to compute elapsed time
    pub turn_start: Option<Instant>,

    /// Session ID to resume — set by the interactive session picker overlay.
    pub pending_resume: Option<String>,
    /// Session ID to delete — set by pressing 'd' or Delete in the session picker.
    pub pending_delete: Option<String>,
    /// Model to switch to — set by the interactive model picker overlay.
    pub pending_model: Option<String>,
    /// Help category index to show — set by the interactive help picker overlay.
    pub pending_help_category: Option<usize>,
    /// Help command to populate input with — set by selecting a command in help submenu.
    pub pending_help_command: Option<String>,
    /// Voice model path to preview — set by the interactive voice model picker.
    pub pending_voice_model: Option<String>,
    /// Voice clone tier — set when user starts a clone recording session.
    pub pending_clone_tier: Option<crate::voice::CloneTier>,

    /// When the /undo picker is open, maps overlay row index → target
    /// position in `session.meta.auto_commits`.
    pub pending_undo_positions: Option<Vec<usize>>,
    /// When the /redo picker is open, maps overlay row index → target
    /// position in `session.meta.auto_commits`.
    pub pending_redo_positions: Option<Vec<usize>>,

    /// Smart model router configuration.
    pub router: crate::router::RouterConfig,
    /// Session cost tracker with per-model breakdown.
    pub cost_tracker: crate::cost::CostTracker,

    /// Shared browser session — populated from `SharedToolState::browser_session`
    /// so `/browser`, `/browse`, `/screenshot` and the `browser_*` tools all
    /// drive the same Chrome instance. `None` when `browser_enabled == false`.
    pub browser_session:
        Option<std::sync::Arc<tokio::sync::Mutex<crate::browser::BrowserSession>>>,

    /// Active file-watch state. `None` when no watcher is running.
    /// Dropping this stops watching.
    pub watcher: Option<WatcherHandle>,

    /// Active browse approval prompt awaiting user input (A=approve, D=deny).
    pub browse_approval: Option<crate::browser::approval_gate::ApprovalPrompt>,
    /// Receiver for browse progress events (active during a /browse run).
    pub browse_progress_rx: Option<tokio::sync::mpsc::Receiver<crate::browser::browse_loop::BrowseProgress>>,
    /// Receiver for browse approval prompts.
    pub browse_approval_rx: Option<tokio::sync::mpsc::Receiver<crate::browser::approval_gate::ApprovalPrompt>>,
}

/// Format a raw model ID into a human-readable name like "Sonnet 4.6".
/// Handles both new format (claude-sonnet-4-6) and old (claude-3-5-sonnet-20241022).
/// Non-claude models (Ollama) are returned as-is.
pub fn pretty_model_name(model: &str) -> String {
    if !model.starts_with("claude-") {
        return model.to_string();
    }
    // Strip "claude-" prefix
    let s = &model[7..];
    // Strip trailing 8-digit date suffix (e.g. -20251001)
    let s = if let Some(pos) = s.rfind('-') {
        let suffix = &s[pos + 1..];
        if suffix.len() == 8 && suffix.chars().all(|c| c.is_ascii_digit()) {
            &s[..pos]
        } else {
            s
        }
    } else {
        s
    };
    let parts: Vec<&str> = s.split('-').collect();
    if parts.is_empty() {
        return s.to_string();
    }

    // Old format: "3-5-sonnet" (first part is numeric)
    if parts[0].chars().all(|c| c.is_ascii_digit()) {
        let family = parts.last().copied().unwrap_or(s);
        let nums: Vec<&str> = parts
            .iter()
            .take_while(|p| p.chars().all(|c| c.is_ascii_digit()))
            .copied()
            .collect();
        let version = nums.join(".");
        let mut f = family.to_string();
        if let Some(c) = f.get_mut(0..1) {
            c.make_ascii_uppercase()
        }
        if version.is_empty() {
            f
        } else {
            format!("{f} {version}")
        }
    } else {
        // New format: "sonnet-4-6"
        let mut f = parts[0].to_string();
        if let Some(c) = f.get_mut(0..1) {
            c.make_ascii_uppercase()
        }
        let version = parts[1..].join(".");
        if version.is_empty() {
            f
        } else {
            format!("{f} {version}")
        }
    }
}

impl App {
    pub fn new(model: &str, _cwd: &std::path::Path) -> Self {
        let username = std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_else(|_| "there".to_string());

        let model_short = pretty_model_name(model);
        let cached_cwd = compute_cwd_display();

        Self {
            entries: Vec::new(),
            streaming: String::new(),
            is_loading: false,
            input: Vec::new(),
            cursor: 0,
            scroll: 0,
            follow_bottom: true,
            should_quit: false,
            pending_screen_clear: false,
            input_history: Vec::new(),
            history_idx: None,
            saved_input: Vec::new(),
            pending_permission: None,
            pending_user_question: None,
            pending_image: None,
            plan_mode: false,
            brief_mode: false,
            btw_note: None,
            overlay: None,
            tokens_in: 0,
            tokens_out: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            username,
            model: model.to_string(),
            model_short,
            effort: None,
            cached_cwd,
            banner_label: crate::config::Config::get_banner_label(),
            session_name: String::new(),
            recent_sessions: Vec::new(),
            show_welcome: true,
            vim_enabled: false,
            vim_normal: false,
            vim_pending: None,
            tool_stream_buf: String::new(),
            api_task: None,
            theme: "dark".to_string(),
            voice_recording: false,
            voice_task: None,
            voice_stop_tx: None,
            tts_stop_tx: None,
            pending_install: None,
            turn_costs: Vec::new(),
            spinner_verb: "Thinking".to_string(),
            spinner_style: "themed".to_string(),
            turn_start: None,
            pending_resume: None,
            pending_delete: None,
            pending_model: None,
            pending_help_category: None,
            pending_help_command: None,
            pending_voice_model: None,
            pending_clone_tier: None,
            pending_undo_positions: None,
            pending_redo_positions: None,
            router: crate::router::RouterConfig::new(model),
            cost_tracker: crate::cost::CostTracker::new(),
            browser_session: None,
            watcher: None,
            browse_approval: None,
            browse_progress_rx: None,
            browse_approval_rx: None,
        }
    }

    /// Update the model and its cached short form together.
    pub fn set_model(&mut self, model: String) {
        self.model_short = pretty_model_name(&model);
        self.model = model;
    }

    /// Begin a loading turn — picks a random spinner verb and records the start time.
    pub fn start_loading(&mut self) {
        self.is_loading = true;
        self.turn_start = Some(Instant::now());
        self.spinner_verb = match self.spinner_style.as_str() {
            "minimal" => "Working".to_string(),
            "silent" => String::new(),
            _ => {
                let mut rng = rand::rng();
                SPINNER_VERBS
                    .choose(&mut rng)
                    .unwrap_or(&"Thinking")
                    .to_string()
            }
        };
    }

    /// End a loading turn — computes duration and pushes a completion system message.
    /// If `tokens_out` is provided (> 0), includes token count in the message.
    pub fn finish_loading_with_stats(&mut self, tokens_out: u64) {
        self.is_loading = false;
        if let Some(start) = self.turn_start.take() {
            let elapsed = start.elapsed().as_millis();
            if elapsed >= 1000 {
                let mut rng = rand::rng();
                let verb = TURN_COMPLETION_VERBS.choose(&mut rng).unwrap_or(&"Worked");
                let dur = format_duration_ms(elapsed);
                let msg = if tokens_out > 0 {
                    let tok = format_tokens(tokens_out);
                    format!("{verb} for {dur} · ↓ {tok}")
                } else {
                    format!("{verb} for {dur}")
                };
                self.entries.push(ChatEntry::system(msg));
            }
        }
    }

    /// End a loading turn without stats (cancel, plugin install, etc.)
    #[allow(dead_code)]
    pub fn finish_loading(&mut self) {
        self.is_loading = false;
        self.turn_start = None;
    }

    // ── Input helpers ─────────────────────────────────────────────────────────

    #[allow(dead_code)]
    pub fn input_str(&self) -> String {
        self.input.iter().collect()
    }

    pub fn take_input(&mut self) -> String {
        let s: String = self.input.drain(..).collect();
        self.cursor = 0;
        self.history_idx = None;
        if !s.trim().is_empty() {
            self.input_history.push(s.clone());
            // Cap history to prevent unbounded growth in long sessions
            if self.input_history.len() > 500 {
                self.input_history.drain(..self.input_history.len() - 500);
                self.input_history.shrink_to_fit();
            }
        }
        s
    }

    pub fn insert_char(&mut self, c: char) {
        self.history_idx = None;
        self.input.insert(self.cursor, c);
        self.cursor += 1;
        // Snap to bottom when user starts typing so context is always visible
        self.follow_bottom = true;
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.input.remove(self.cursor);
        }
    }

    pub fn cursor_left(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
        }
    }
    pub fn cursor_right(&mut self) {
        if self.cursor < self.input.len() {
            self.cursor += 1;
        }
    }
    /// Ctrl+A — move to start of current line (not buffer). v2.1.91 line-aware fix.
    pub fn cursor_home(&mut self) {
        let mut i = self.cursor;
        while i > 0 && self.input[i - 1] != '\n' {
            i -= 1;
        }
        self.cursor = i;
    }
    /// Ctrl+E — move to end of current line (not buffer). v2.1.91 line-aware fix.
    pub fn cursor_end(&mut self) {
        let len = self.input.len();
        let mut i = self.cursor;
        while i < len && self.input[i] != '\n' {
            i += 1;
        }
        self.cursor = i;
    }

    // ── Vim mode helpers ──────────────────────────────────────────────────────

    /// Enter vim normal mode: clamp cursor so it never sits past the last char.
    pub fn vim_enter_normal(&mut self) {
        self.vim_normal = true;
        self.vim_pending = None;
        let len = self.input.len();
        if len > 0 && self.cursor >= len {
            self.cursor = len - 1;
        }
    }

    /// Enter vim insert mode.
    pub fn vim_enter_insert(&mut self) {
        self.vim_normal = false;
        self.vim_pending = None;
    }

    /// w — move to start of the next word.
    pub fn word_forward(&mut self) {
        let len = self.input.len();
        // Skip current non-whitespace
        while self.cursor < len && !self.input[self.cursor].is_whitespace() {
            self.cursor += 1;
        }
        // Skip whitespace
        while self.cursor < len && self.input[self.cursor].is_whitespace() {
            self.cursor += 1;
        }
        // In normal mode clamp to last char
        if self.vim_normal && self.cursor >= len && len > 0 {
            self.cursor = len - 1;
        }
    }

    /// b — move to start of current/previous word.
    pub fn word_back(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor -= 1;
        // Skip whitespace backward
        while self.cursor > 0 && self.input[self.cursor].is_whitespace() {
            self.cursor -= 1;
        }
        // Skip word chars backward to find the start
        while self.cursor > 0 && !self.input[self.cursor - 1].is_whitespace() {
            self.cursor -= 1;
        }
    }

    /// e — move to end of current/next word.
    pub fn word_end(&mut self) {
        let len = self.input.len();
        if len == 0 {
            return;
        }
        // If on whitespace, skip it first
        if self.cursor < len && self.input[self.cursor].is_whitespace() {
            while self.cursor < len && self.input[self.cursor].is_whitespace() {
                self.cursor += 1;
            }
        } else if self.cursor + 1 < len && !self.input[self.cursor + 1].is_whitespace() {
            self.cursor += 1;
        } else {
            return;
        }
        // Now advance to last char of this word
        while self.cursor + 1 < len && !self.input[self.cursor + 1].is_whitespace() {
            self.cursor += 1;
        }
        if self.cursor >= len && len > 0 {
            self.cursor = len - 1;
        }
    }

    /// x — delete the character under the cursor (normal mode).
    pub fn delete_under(&mut self) {
        if self.cursor < self.input.len() {
            self.input.remove(self.cursor);
            let len = self.input.len();
            if len > 0 && self.cursor >= len {
                self.cursor = len - 1;
            }
        }
    }

    pub fn history_up(&mut self) {
        if self.input_history.is_empty() {
            return;
        }
        match self.history_idx {
            None => {
                self.saved_input = self.input.clone();
                self.history_idx = Some(self.input_history.len() - 1);
            }
            Some(i) if i > 0 => {
                self.history_idx = Some(i - 1);
            }
            _ => {}
        }
        if let Some(i) = self.history_idx {
            self.input = self.input_history[i].chars().collect();
            self.cursor = self.input.len();
        }
    }

    pub fn history_down(&mut self) {
        if let Some(i) = self.history_idx {
            if i + 1 < self.input_history.len() {
                let next = i + 1;
                self.history_idx = Some(next);
                self.input = self.input_history[next].chars().collect();
            } else {
                self.history_idx = None;
                self.input = self.saved_input.clone();
            }
            self.cursor = self.input.len();
        }
    }

    pub fn clear_line(&mut self) {
        self.input.clear();
        self.cursor = 0;
    }

    /// Ctrl+W — delete the word immediately before the cursor.
    pub fn delete_word_back(&mut self) {
        // Skip trailing whitespace
        while self.cursor > 0 && self.input[self.cursor - 1].is_whitespace() {
            self.cursor -= 1;
            self.input.remove(self.cursor);
        }
        // Delete the word itself
        while self.cursor > 0 && !self.input[self.cursor - 1].is_whitespace() {
            self.cursor -= 1;
            self.input.remove(self.cursor);
        }
    }

    /// Alt+D — delete the word immediately after the cursor.
    pub fn delete_word_forward(&mut self) {
        // Skip leading whitespace
        while self.cursor < self.input.len() && self.input[self.cursor].is_whitespace() {
            self.input.remove(self.cursor);
        }
        // Delete the word
        while self.cursor < self.input.len() && !self.input[self.cursor].is_whitespace() {
            self.input.remove(self.cursor);
        }
    }

    /// Return the suffix of the most-recently-used history entry that starts
    /// with the current input (for autosuggestion). None if no match.
    pub fn history_suggestion(&self) -> Option<String> {
        if self.input.is_empty() {
            return None;
        }
        // Only suggest on single-line input (no newlines)
        if self.input.contains(&'\n') {
            return None;
        }
        let input: String = self.input.iter().collect();
        self.input_history
            .iter()
            .rev()
            .find(|h| h.starts_with(&input) && h.len() > input.len())
            .map(|h| h[input.len()..].to_string())
    }

    /// Accept the current history autosuggestion (Tab key).
    pub fn accept_suggestion(&mut self) {
        if let Some(suffix) = self.history_suggestion() {
            for ch in suffix.chars() {
                self.input.push(ch);
            }
            self.cursor = self.input.len();
        }
    }

    /// Ctrl+K — delete from the cursor to the end of the line.
    pub fn delete_to_end(&mut self) {
        // Stop at newline (only delete current line's tail, not the whole input)
        let end = self.input[self.cursor..]
            .iter()
            .position(|&c| c == '\n')
            .map(|p| self.cursor + p)
            .unwrap_or(self.input.len());
        self.input.drain(self.cursor..end);
    }

    /// Alt+B / Ctrl+Left — move cursor to the start of the previous word (readline-style).
    pub fn word_back_readline(&mut self) {
        if self.cursor == 0 {
            return;
        }
        // Skip whitespace backward
        while self.cursor > 0 && self.input[self.cursor - 1].is_whitespace() {
            self.cursor -= 1;
        }
        // Skip word backward
        while self.cursor > 0 && !self.input[self.cursor - 1].is_whitespace() {
            self.cursor -= 1;
        }
    }

    /// Alt+F / Ctrl+Right — move cursor to the end of the next word (readline-style).
    pub fn word_forward_readline(&mut self) {
        let len = self.input.len();
        // Skip whitespace forward
        while self.cursor < len && self.input[self.cursor].is_whitespace() {
            self.cursor += 1;
        }
        // Skip word forward
        while self.cursor < len && !self.input[self.cursor].is_whitespace() {
            self.cursor += 1;
        }
    }

    /// Count the number of visual lines in the current input (for layout).
    pub fn input_line_count(&self) -> usize {
        self.input.iter().filter(|&&c| c == '\n').count() + 1
    }

    /// Insert a newline at the cursor position (Shift+Enter).
    pub fn insert_newline(&mut self) {
        self.input.insert(self.cursor, '\n');
        self.cursor += 1;
    }

    /// Move cursor up one visual line (within multi-line input), preserving column.
    pub fn move_cursor_up_one_line(&mut self) {
        let before: String = self.input[..self.cursor].iter().collect();
        let col = before
            .rfind('\n')
            .map(|p| before.len() - p - 1)
            .unwrap_or(before.len());
        // Find end of the previous line
        let prev_newline = before.rfind('\n');
        if let Some(pnl) = prev_newline {
            // pnl is the index of '\n' ending the previous line
            let prev_line_end = pnl; // exclusive
            let prev_line_start = before[..pnl].rfind('\n').map(|p| p + 1).unwrap_or(0);
            let prev_line_len = prev_line_end - prev_line_start;
            let target_col = col.min(prev_line_len);
            self.cursor = prev_line_start + target_col;
        }
    }

    /// Move cursor down one visual line (within multi-line input), preserving column.
    pub fn move_cursor_down_one_line(&mut self) {
        let before: String = self.input[..self.cursor].iter().collect();
        let col = before
            .rfind('\n')
            .map(|p| before.len() - p - 1)
            .unwrap_or(before.len());
        // Find start of the next line
        let full: String = self.input.iter().collect();
        if let Some(next_nl_rel) = full[self.cursor..].find('\n') {
            let next_line_start = self.cursor + next_nl_rel + 1;
            // Find end of next line
            let next_line_end = full[next_line_start..]
                .find('\n')
                .map(|p| next_line_start + p)
                .unwrap_or(full.len());
            let next_line_len = next_line_end - next_line_start;
            let target_col = col.min(next_line_len);
            self.cursor = next_line_start + target_col;
        }
    }

    // ── Scroll helpers ────────────────────────────────────────────────────────

    /// Scroll up by n lines; disables auto-follow.
    pub fn scroll_up(&mut self) {
        self.follow_bottom = false;
        // Use ~half the terminal height for a page-like feel
        let amount = (crossterm::terminal::size()
            .map(|(_, h)| h as usize / 2)
            .unwrap_or(10))
        .max(5);
        self.scroll = self.scroll.saturating_sub(amount);
    }

    /// Scroll down by n lines; re-enables auto-follow when at bottom.
    #[allow(dead_code)]
    pub fn scroll_down(&mut self, total: usize) {
        self.follow_bottom = false;
        let amount = (crossterm::terminal::size()
            .map(|(_, h)| h as usize / 2)
            .unwrap_or(10))
        .max(5);
        self.scroll = (self.scroll + amount).min(total.saturating_sub(1));
    }

    /// Snap to newest content and stay there (auto-follow on).
    pub fn scroll_to_bottom(&mut self) {
        self.follow_bottom = true;
    }

    // ── Streaming helpers ─────────────────────────────────────────────────────

    pub fn flush_streaming(&mut self) {
        if !self.streaming.is_empty() {
            let text = std::mem::take(&mut self.streaming);
            self.entries.push(ChatEntry::assistant(text));
        }
    }

    // ── Apply events ──────────────────────────────────────────────────────────

    pub fn apply(&mut self, event: AppEvent) {
        match event {
            AppEvent::TextChunk(chunk) => {
                self.streaming.push_str(&chunk);
                // Only auto-scroll if user hasn't manually scrolled up
                if self.follow_bottom {
                    self.scroll_to_bottom();
                }
            }
            AppEvent::ThinkingBlock(text) => {
                self.flush_streaming();
                self.entries.push(ChatEntry::thinking(text));
                self.scroll_to_bottom();
            }
            AppEvent::ToolCall { name, args } => {
                self.flush_streaming();
                let preview = format_tool_preview(&name, &args);
                self.entries
                    .push(ChatEntry::tool_call(format!("{name}  {preview}")));
                self.tool_stream_buf = String::new(); // drop old capacity
                self.scroll_to_bottom();
            }
            AppEvent::ToolOutputStream(line) => {
                // Accumulate live output lines (capped at 4KB — we only display the last 500 chars)
                self.tool_stream_buf.push_str(&line);
                self.tool_stream_buf.push('\n');
                if self.tool_stream_buf.len() > 4096 {
                    let start = self.tool_stream_buf.len() - 2048;
                    // Find a safe char boundary
                    let mut start = start;
                    while start < self.tool_stream_buf.len()
                        && !self.tool_stream_buf.is_char_boundary(start)
                    {
                        start += 1;
                    }
                    self.tool_stream_buf = self.tool_stream_buf[start..].to_string();
                }
                // Update or create the live output entry (last entry if it's a tool_stream kind)
                let display = if self.tool_stream_buf.len() > 500 {
                    format!(
                        "…{}",
                        &self.tool_stream_buf[self.tool_stream_buf.len() - 500..]
                    )
                } else {
                    self.tool_stream_buf.clone()
                };
                if let Some(last) = self.entries.last_mut()
                    && matches!(last.kind, EntryKind::ToolStream)
                {
                    last.text = display;
                    self.scroll_to_bottom();
                    return;
                }
                self.entries.push(ChatEntry {
                    kind: EntryKind::ToolStream,
                    text: display,
                });
                self.scroll_to_bottom();
            }
            AppEvent::ToolResult { is_error, text } => {
                if is_error {
                    self.entries.push(ChatEntry::error(text));
                } else {
                    self.entries.push(ChatEntry::tool_result(text));
                }
                self.scroll_to_bottom();
            }
            AppEvent::Done {
                tokens_in,
                tokens_out,
                cache_read,
                cache_write,
                ..
            } => {
                self.flush_streaming();
                self.finish_loading_with_stats(tokens_out);
                self.api_task = None;
                self.tokens_in = tokens_in;
                self.tokens_out = tokens_out;
                self.cache_read_tokens = cache_read;
                self.cache_write_tokens = cache_write;
                self.scroll_to_bottom();
            }
            AppEvent::Error(msg) => {
                self.flush_streaming();
                self.finish_loading_with_stats(0);
                self.api_task = None;
                self.entries.push(ChatEntry::error(msg));
                self.scroll_to_bottom();
            }
            AppEvent::PermissionRequest {
                tool_name,
                description,
                reply,
            } => {
                self.pending_permission = Some(PendingPermission {
                    tool_name,
                    description,
                    reply,
                });
            }
            AppEvent::AskUser { question, reply } => {
                self.pending_user_question = Some(PendingUserQuestion {
                    question,
                    reply,
                    input: Vec::new(),
                    cursor: 0,
                });
            }
            AppEvent::SetPlanMode(enabled) => {
                self.plan_mode = enabled;
                // System message is sent by run_api_task before this event
            }
            // AppEvent::ToggleBriefMode removed — brief mode toggled directly
            // in run.rs via CommandAction::ToggleBriefMode without going through AppEvent.
            AppEvent::Compacted { summary_len, .. } => {
                self.entries.push(ChatEntry::system(format!(
                    "Context compacted — history replaced with {summary_len}-char summary."
                )));
                self.scroll_to_bottom();
            }
            AppEvent::SystemMessage(msg) => {
                self.entries.push(ChatEntry::system(msg));
                self.scroll_to_bottom();
            }
            AppEvent::VoiceTranscription(text) => {
                for ch in text.chars() {
                    self.insert_char(ch);
                }
                self.voice_recording = false;
                self.voice_task = None;
                self.voice_stop_tx = None;
            }
            AppEvent::VoiceBrowse(_goal) => {
                // Recording state cleared here; run.rs handles the Browse dispatch.
                self.voice_recording = false;
                self.voice_task = None;
                self.voice_stop_tx = None;
            }
            AppEvent::PluginInstallDone { success, message } => {
                if success {
                    self.entries.push(ChatEntry::system(message));
                } else {
                    self.entries.push(ChatEntry::error(message));
                }
                self.is_loading = false;
                self.turn_start = None;
                self.scroll_to_bottom();
            }
            AppEvent::UpgradeCheckDone { message } => {
                self.entries.push(ChatEntry::system(message));
                self.is_loading = false;
                self.turn_start = None;
                self.scroll_to_bottom();
            }
        }
    }

    pub fn clear(&mut self) {
        self.entries = Vec::new();
        self.streaming = String::new();
        self.tool_stream_buf = String::new();
        self.tokens_in = 0;
        self.tokens_out = 0;
        self.cache_read_tokens = 0;
        self.cache_write_tokens = 0;
        self.scroll = 0;
        self.follow_bottom = true;
        self.show_welcome = true;
        self.turn_costs = Vec::new();
    }
}

// ── Module-level helpers ──────────────────────────────────────────────────────

/// Compute current dir with `~` substitution. Called once at App construction.
fn compute_cwd_display() -> String {
    std::env::current_dir()
        .map(|p| {
            let s = p.display().to_string();
            if let Some(home) = dirs::home_dir() {
                let h = home.display().to_string();
                if s.starts_with(&h) {
                    return format!("~{}", &s[h.len()..]);
                }
            }
            s
        })
        .unwrap_or_default()
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Extract a human-readable preview from a tool's JSON args.
pub fn format_tool_preview_pub(name: &str, args: &str) -> String {
    format_tool_preview(name, args)
}

fn format_tool_preview(name: &str, args: &str) -> String {
    let val: serde_json::Value = match serde_json::from_str(args) {
        Ok(v) => v,
        Err(_) => return truncate(args, 100),
    };

    let s = |key: &str| -> Option<String> {
        val.get(key).and_then(|v| v.as_str()).map(|s| s.to_string())
    };

    let preview = match name {
        "Bash" => s("command").unwrap_or_default(),
        "Read" => s("file_path").unwrap_or_default(),
        "Write" => s("file_path").unwrap_or_default(),
        "Edit" => s("file_path").unwrap_or_default(),
        "Glob" => s("pattern").unwrap_or_default(),
        "Grep" => {
            let pat = s("pattern").unwrap_or_default();
            let path = s("path").unwrap_or_default();
            if path.is_empty() {
                pat
            } else {
                format!("{pat}  in {path}")
            }
        }
        "WebFetch" => s("url").unwrap_or_default(),
        "WebSearch" => s("query").unwrap_or_default(),
        "Agent" => s("prompt").map(|p| truncate(&p, 80)).unwrap_or_default(),
        "TodoWrite" => {
            let count = val
                .get("todos")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            format!("{count} items")
        }
        "TaskCreate" => s("subject").unwrap_or_default(),
        "TaskUpdate" => {
            let id = s("task_id").unwrap_or_default();
            let status = s("status").unwrap_or_default();
            format!("{id} → {status}")
        }
        _ => truncate(args, 100),
    };

    truncate(&preview, 120)
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        // Truncate at a char boundary
        let end = s
            .char_indices()
            .map(|(i, _)| i)
            .take_while(|&i| i < max)
            .last()
            .unwrap_or(0);
        format!("{}…", &s[..end])
    }
}
