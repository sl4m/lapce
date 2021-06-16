use std::{
    collections::HashMap,
    fs::File,
    io::{BufReader, Read},
    path::PathBuf,
    process::{self, Stdio},
    sync::Arc,
    thread,
};

use anyhow::{anyhow, Result};
use crossbeam_channel::{unbounded, Receiver, Sender, TryRecvError};
use crossbeam_utils::sync::WaitGroup;
use druid::{
    theme, Application, Color, Command, Data, Env, EventCtx, ExtEventSink,
    FontDescriptor, FontFamily, KeyEvent, Lens, Point, Rect, Size, Target, Vec2,
    WidgetId, WindowId,
};
use im;
use lsp_types::CompletionResponse;
use parking_lot::Mutex;
use serde_json::Value;
use tree_sitter_highlight::{Highlight, HighlightEvent, Highlighter};
use xi_core_lib::selection::InsertDrift;
use xi_rope::{spans::SpansBuilder, DeltaBuilder, Interval, Rope, RopeDelta};
use xi_rpc::{RpcLoop, RpcPeer};

use crate::{
    buffer::{
        previous_has_unmatched_pair, Buffer, BufferId, BufferNew, BufferState,
        BufferUpdate, EditType, Style, UpdateEvent,
    },
    command::{LapceCommand, LapceUICommand, LAPCE_UI_COMMAND},
    completion::{CompletionData, CompletionStatus},
    keypress::{KeyPressData, KeyPressFocus},
    language::new_highlight_config,
    movement::{Cursor, CursorMode, LinePosition, Movement, SelRegion, Selection},
    proxy::{LapceProxy, ProxyHandlerNew},
    split::SplitMoveDirection,
    state::{LapceWorkspace, LapceWorkspaceType, Mode, VisualMode},
    theme::LapceTheme,
};

#[derive(Clone, Data)]
pub struct LapceData {
    pub windows: im::HashMap<WindowId, LapceWindowData>,
    pub theme: Arc<std::collections::HashMap<String, Color>>,
    pub keypress: Arc<KeyPressData>,
}

impl LapceData {
    pub fn load() -> Self {
        let mut windows = im::HashMap::new();
        let keypress = Arc::new(KeyPressData::new());
        let theme =
            Arc::new(Self::get_theme().unwrap_or(std::collections::HashMap::new()));
        let window = LapceWindowData::new(keypress.clone(), theme.clone());
        windows.insert(WindowId::next(), window);
        Self {
            windows,
            theme,
            keypress,
        }
    }

    fn get_theme() -> Result<std::collections::HashMap<String, Color>> {
        let mut f = File::open("/Users/Lulu/lapce/.lapce/theme.toml")?;
        let mut content = vec![];
        f.read_to_end(&mut content)?;
        let toml_theme: im::HashMap<String, String> = toml::from_slice(&content)?;

        let mut theme = std::collections::HashMap::new();
        for (name, hex) in toml_theme.iter() {
            if let Ok(color) = Color::from_hex_str(hex) {
                theme.insert(name.to_string(), color);
            }
        }
        Ok(theme)
    }

    pub fn reload_env(&self, env: &mut Env) {
        let changed = match env.try_get(&LapceTheme::CHANGED) {
            Ok(changed) => changed,
            Err(e) => true,
        };
        if !changed {
            return;
        }

        env.set(LapceTheme::CHANGED, false);
        let theme = &self.theme;
        if let Some(line_highlight) = theme.get("line_highlight") {
            env.set(
                LapceTheme::EDITOR_CURRENT_LINE_BACKGROUND,
                line_highlight.clone(),
            );
        };
        if let Some(caret) = theme.get("caret") {
            env.set(LapceTheme::EDITOR_CURSOR_COLOR, caret.clone());
        };
        if let Some(foreground) = theme.get("foreground") {
            env.set(LapceTheme::EDITOR_FOREGROUND, foreground.clone());
        };
        if let Some(background) = theme.get("background") {
            env.set(LapceTheme::EDITOR_BACKGROUND, background.clone());
        };
        if let Some(selection) = theme.get("selection") {
            env.set(LapceTheme::EDITOR_SELECTION_COLOR, selection.clone());
        };
        if let Some(color) = theme.get("comment") {
            env.set(LapceTheme::EDITOR_COMMENT, color.clone());
        };
        if let Some(color) = theme.get("error") {
            env.set(LapceTheme::EDITOR_ERROR, color.clone());
        };
        if let Some(color) = theme.get("warn") {
            env.set(LapceTheme::EDITOR_WARN, color.clone());
        };
        env.set(LapceTheme::EDITOR_LINE_HEIGHT, 25.0);
        env.set(LapceTheme::PALETTE_BACKGROUND, Color::rgb8(125, 125, 125));
        env.set(LapceTheme::PALETTE_INPUT_FOREROUND, Color::rgb8(0, 0, 0));
        env.set(
            LapceTheme::PALETTE_INPUT_BACKGROUND,
            Color::rgb8(255, 255, 255),
        );
        env.set(LapceTheme::PALETTE_INPUT_BORDER, Color::rgb8(0, 0, 0));
        env.set(
            LapceTheme::EDITOR_FONT,
            FontDescriptor::new(FontFamily::new_unchecked("Cascadia Code"))
                .with_size(13.0),
        );
        env.set(
            theme::SCROLLBAR_COLOR,
            Color::from_hex_str("#c4c4c4").unwrap(),
        );
    }
}

#[derive(Clone)]
pub struct LapceWindowData {
    pub tabs: im::HashMap<WidgetId, LapceTabData>,
    pub active: WidgetId,
    pub keypress: Arc<KeyPressData>,
    pub theme: Arc<std::collections::HashMap<String, Color>>,
}

impl Data for LapceWindowData {
    fn same(&self, other: &Self) -> bool {
        self.active == other.active && self.tabs.same(&other.tabs)
    }
}

impl LapceWindowData {
    pub fn new(
        keypress: Arc<KeyPressData>,
        theme: Arc<std::collections::HashMap<String, Color>>,
    ) -> Self {
        let mut tabs = im::HashMap::new();
        let tab_id = WidgetId::next();
        let tab = LapceTabData::new(tab_id, keypress.clone(), theme.clone());
        tabs.insert(tab_id, tab);
        Self {
            tabs,
            active: tab_id,
            keypress,
            theme,
        }
    }
}

#[derive(Clone, Lens)]
pub struct LapceTabData {
    pub id: WidgetId,
    pub main_split: LapceMainSplitData,
    pub completion: Arc<CompletionData>,
    pub proxy: Arc<LapceProxy>,
    pub keypress: Arc<KeyPressData>,
    pub update_receiver: Option<Receiver<UpdateEvent>>,
    pub update_sender: Arc<Sender<UpdateEvent>>,
    pub theme: Arc<std::collections::HashMap<String, Color>>,
    pub window_origin: Point,
}

impl Data for LapceTabData {
    fn same(&self, other: &Self) -> bool {
        self.main_split.same(&other.main_split)
            && self.completion.same(&other.completion)
    }
}

impl LapceTabData {
    pub fn new(
        tab_id: WidgetId,
        keypress: Arc<KeyPressData>,
        theme: Arc<std::collections::HashMap<String, Color>>,
    ) -> Self {
        let (update_sender, update_receiver) = unbounded();
        let update_sender = Arc::new(update_sender);
        let proxy = Arc::new(LapceProxy::new(tab_id));
        let main_split = LapceMainSplitData::new(update_sender.clone());
        let completion = Arc::new(CompletionData::new());
        Self {
            id: tab_id,
            main_split,
            completion,
            proxy,
            keypress,
            theme,
            update_sender,
            update_receiver: Some(update_receiver),
            window_origin: Point::ZERO,
        }
    }

    pub fn completion_origin(&self, tab_size: Size, env: &Env) -> Point {
        let line_height = env.get(LapceTheme::EDITOR_LINE_HEIGHT);

        let editor = self.main_split.active_editor();
        let buffer_id = self.main_split.open_files.get(&editor.buffer).unwrap();
        let buffer = self.main_split.buffers.get(&buffer_id).unwrap();
        let offset = self.completion.offset;
        let (line, col) = buffer.offset_to_line_col(offset);
        let width = 7.6171875;
        let x = buffer.col_x(line, col, width);
        let y = (line + 1) as f64 * line_height;
        let mut origin =
            editor.window_origin - self.window_origin.to_vec2() + Vec2::new(x, y);
        if origin.y + self.completion.size.height + 1.0 > tab_size.height {
            let height = self
                .completion
                .size
                .height
                .min(self.completion.len() as f64 * line_height);
            origin.y = editor.window_origin.y - self.window_origin.y
                + line as f64 * line_height
                - height;
        }
        if origin.x + self.completion.size.width + 1.0 > tab_size.width {
            origin.x = tab_size.width - self.completion.size.width - 1.0;
        }

        origin
    }

    pub fn completion_done(&mut self, resp: CompletionResponse) {
        let items = match resp {
            CompletionResponse::Array(items) => items,
            CompletionResponse::List(list) => list.items,
        };
        let editor = self.main_split.active_editor();
        let buffer_id = self.main_split.open_files.get(&editor.buffer).unwrap();
        let buffer = self.main_split.buffers.get(&buffer_id).unwrap();
        let offset = editor.cursor.offset();

        let start_offset = buffer.prev_code_boundary(offset);
        let end_offset = buffer.next_code_boundary(offset);
        let input = buffer.slice_to_cow(start_offset..end_offset).to_string();

        let completion = Arc::make_mut(&mut self.completion);
        completion.done(input, items);
    }

    pub fn buffer_update_process(
        tab_id: WidgetId,
        receiver: Receiver<UpdateEvent>,
        event_sink: ExtEventSink,
    ) {
        use std::collections::{HashMap, HashSet};
        fn insert_update(
            updates: &mut HashMap<BufferId, UpdateEvent>,
            event: UpdateEvent,
        ) {
            let update = match &event {
                UpdateEvent::Buffer(update) => update,
                UpdateEvent::SemanticTokens(update, tokens) => update,
            };
            if let Some(current) = updates.get(&update.id) {
                let current = match &event {
                    UpdateEvent::Buffer(update) => update,
                    UpdateEvent::SemanticTokens(update, tokens) => update,
                };
                if update.rev > current.rev {
                    updates.insert(update.id, event);
                }
            } else {
                updates.insert(update.id, event);
            }
        }

        fn receive_batch(
            receiver: &Receiver<UpdateEvent>,
        ) -> HashMap<BufferId, UpdateEvent> {
            let mut updates = HashMap::new();
            loop {
                let update = receiver.recv().unwrap();
                insert_update(&mut updates, update);
                match receiver.try_recv() {
                    Ok(update) => {
                        insert_update(&mut updates, update);
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => (),
                }
            }
            updates
        }

        let mut highlighter = Highlighter::new();
        let mut highlight_configs = HashMap::new();
        loop {
            let events = receive_batch(&receiver);
            for (_, event) in events {
                let (update, tokens) = match event {
                    UpdateEvent::Buffer(update) => (update, None),
                    UpdateEvent::SemanticTokens(update, tokens) => {
                        (update, Some(tokens))
                    }
                };

                let semantic_tokens = tokens.is_some();

                let highlights = if let Some(tokens) = tokens {
                    let start = std::time::SystemTime::now();
                    let mut highlights = SpansBuilder::new(update.rope.len());
                    for (start, end, hl) in tokens {
                        highlights.add_span(
                            Interval::new(start, end),
                            Style {
                                fg_color: Some(hl.to_string()),
                            },
                        );
                    }
                    let highlights = highlights.build();
                    let end = std::time::SystemTime::now();
                    let duration = end.duration_since(start).unwrap().as_micros();
                    // println!("semantic tokens took {}", duration);
                    highlights
                } else {
                    if !highlight_configs.contains_key(&update.language) {
                        let (highlight_config, highlight_names) =
                            new_highlight_config(update.language);
                        highlight_configs.insert(
                            update.language,
                            (highlight_config, highlight_names),
                        );
                    }
                    let (highlight_config, highlight_names) =
                        highlight_configs.get(&update.language).unwrap();
                    let mut current_hl: Option<Highlight> = None;
                    let mut highlights = SpansBuilder::new(update.rope.len());
                    for hightlight in highlighter
                        .highlight(
                            highlight_config,
                            update
                                .rope
                                .slice_to_cow(0..update.rope.len())
                                .as_bytes(),
                            None,
                            |_| None,
                        )
                        .unwrap()
                    {
                        if let Ok(highlight) = hightlight {
                            match highlight {
                                HighlightEvent::Source { start, end } => {
                                    if let Some(hl) = current_hl {
                                        if let Some(hl) = highlight_names.get(hl.0) {
                                            highlights.add_span(
                                                Interval::new(start, end),
                                                Style {
                                                    fg_color: Some(hl.to_string()),
                                                },
                                            );
                                        }
                                    }
                                }
                                HighlightEvent::HighlightStart(hl) => {
                                    current_hl = Some(hl);
                                }
                                HighlightEvent::HighlightEnd => current_hl = None,
                            }
                        }
                    }
                    let highlights = highlights.build();
                    highlights
                };

                event_sink.submit_command(
                    LAPCE_UI_COMMAND,
                    LapceUICommand::UpdateStyle {
                        id: update.id,
                        rev: update.rev,
                        highlights,
                        semantic_tokens,
                    },
                    Target::Widget(tab_id),
                );
            }
        }
    }
}

pub struct LapceTabLens(pub WidgetId);

impl Lens<LapceWindowData, LapceTabData> for LapceTabLens {
    fn with<V, F: FnOnce(&LapceTabData) -> V>(
        &self,
        data: &LapceWindowData,
        f: F,
    ) -> V {
        let tab = data.tabs.get(&self.0).unwrap();
        f(&tab)
    }

    fn with_mut<V, F: FnOnce(&mut LapceTabData) -> V>(
        &self,
        data: &mut LapceWindowData,
        f: F,
    ) -> V {
        let mut tab = data.tabs.get(&self.0).unwrap().clone();
        tab.keypress = data.keypress.clone();
        tab.theme = data.theme.clone();
        let result = f(&mut tab);
        data.keypress = tab.keypress.clone();
        data.theme = tab.theme.clone();
        if !tab.same(data.tabs.get(&self.0).unwrap()) {
            data.tabs.insert(self.0, tab);
        }
        result
    }
}

pub struct LapceWindowLens(pub WindowId);

impl Lens<LapceData, LapceWindowData> for LapceWindowLens {
    fn with<V, F: FnOnce(&LapceWindowData) -> V>(
        &self,
        data: &LapceData,
        f: F,
    ) -> V {
        let tab = data.windows.get(&self.0).unwrap();
        f(&tab)
    }

    fn with_mut<V, F: FnOnce(&mut LapceWindowData) -> V>(
        &self,
        data: &mut LapceData,
        f: F,
    ) -> V {
        let mut win = data.windows.get(&self.0).unwrap().clone();
        win.keypress = data.keypress.clone();
        win.theme = data.theme.clone();
        let result = f(&mut win);
        data.keypress = win.keypress.clone();
        data.theme = win.theme.clone();
        if !win.same(data.windows.get(&self.0).unwrap()) {
            data.windows.insert(self.0, win);
        }
        result
    }
}

#[derive(Clone, Default)]
pub struct RegisterData {
    pub content: String,
    pub mode: VisualMode,
}

#[derive(Clone, Default)]
pub struct Register {
    unamed: RegisterData,
    last_yank: RegisterData,
    last_deletes: [RegisterData; 10],
    newest_delete: usize,
}

impl Register {
    pub fn add_delete(&mut self, data: RegisterData) {
        self.unamed = data.clone();
    }

    pub fn add_yank(&mut self, data: RegisterData) {
        self.unamed = data.clone();
        self.last_yank = data;
    }
}

#[derive(Clone, Data, Lens)]
pub struct LapceMainSplitData {
    pub split_id: Arc<WidgetId>,
    pub active: Arc<WidgetId>,
    pub editors: im::HashMap<WidgetId, Arc<LapceEditorData>>,
    pub buffers: im::HashMap<BufferId, Arc<BufferNew>>,
    pub open_files: im::HashMap<PathBuf, BufferId>,
    pub update_sender: Arc<Sender<UpdateEvent>>,
    pub register: Arc<Register>,
}

impl LapceMainSplitData {
    pub fn notify_update_text_layouts(
        &self,
        ctx: &mut EventCtx,
        buffer_id: &BufferId,
    ) {
        for (editor_id, editor) in &self.editors {
            let editor_buffer_id = self.open_files.get(&editor.buffer).unwrap();
            if editor_buffer_id == buffer_id {
                ctx.submit_command(Command::new(
                    LAPCE_UI_COMMAND,
                    LapceUICommand::FillTextLayouts,
                    Target::Widget(*editor_id),
                ));
            }
        }
    }

    pub fn active_editor(&self) -> &LapceEditorData {
        self.editors.get(&self.active).unwrap()
    }
}

impl LapceMainSplitData {
    pub fn new(update_sender: Arc<Sender<UpdateEvent>>) -> Self {
        let split_id = Arc::new(WidgetId::next());
        let mut editors = im::HashMap::new();
        let path = PathBuf::from("/Users/Lulu/lapce/core/src/editor.rs");
        let editor = LapceEditorData::new(*split_id, path.clone());
        let view_id = editor.view_id;
        editors.insert(editor.view_id, Arc::new(editor));
        let buffer = BufferNew::new(path.clone(), update_sender.clone());
        let mut open_files = im::HashMap::new();
        open_files.insert(path.clone(), buffer.id);
        let mut buffers = im::HashMap::new();
        buffers.insert(buffer.id, Arc::new(buffer));
        Self {
            split_id,
            editors,
            buffers,
            open_files,
            active: Arc::new(view_id),
            update_sender,
            register: Arc::new(Register::default()),
        }
    }
}

#[derive(Clone, Debug)]
pub struct LapceEditorData {
    pub split_id: WidgetId,
    pub view_id: WidgetId,
    pub container_id: WidgetId,
    pub editor_id: WidgetId,
    pub buffer: PathBuf,
    pub scroll_offset: Vec2,
    pub cursor: Cursor,
    pub size: Size,
    pub window_origin: Point,
}

impl LapceEditorData {
    pub fn new(split_id: WidgetId, buffer: PathBuf) -> Self {
        Self {
            split_id,
            view_id: WidgetId::next(),
            container_id: WidgetId::next(),
            editor_id: WidgetId::next(),
            buffer,
            scroll_offset: Vec2::ZERO,
            cursor: Cursor::default(),
            size: Size::ZERO,
            window_origin: Point::ZERO,
        }
    }
}

#[derive(Clone, Data, Lens)]
pub struct LapceEditorViewData {
    pub main_split: LapceMainSplitData,
    pub proxy: Arc<LapceProxy>,
    pub editor: Arc<LapceEditorData>,
    pub buffer: Arc<BufferNew>,
    pub keypress: Arc<KeyPressData>,
    pub completion: Arc<CompletionData>,
    pub theme: Arc<std::collections::HashMap<String, Color>>,
}

impl LapceEditorViewData {
    pub fn key_down(
        &mut self,
        ctx: &mut EventCtx,
        key_event: &KeyEvent,
        env: &Env,
    ) -> bool {
        let mut keypress = self.keypress.clone();
        let k = Arc::make_mut(&mut keypress);
        let executed = k.key_down(ctx, key_event, self, env);
        self.keypress = keypress;
        executed
    }

    pub fn buffer_mut(&mut self) -> &mut BufferNew {
        Arc::make_mut(&mut self.buffer)
    }

    pub fn fill_text_layouts(
        &mut self,
        ctx: &mut EventCtx,
        theme: &Arc<HashMap<String, Color>>,
        env: &Env,
    ) {
        let start = std::time::SystemTime::now();
        let line_height = env.get(LapceTheme::EDITOR_LINE_HEIGHT);
        let start_line = (self.editor.scroll_offset.y / line_height) as usize;
        let size = self.editor.size;
        let num_lines = ((size.height / line_height).ceil()) as usize;
        let text = ctx.text();
        let buffer = self.buffer_mut();
        for line in start_line..start_line + num_lines + 1 {
            buffer.update_line_layouts(text, line, theme, env);
        }
        let end = std::time::SystemTime::now();
        let duration = end.duration_since(start).unwrap().as_micros();
        // println!("fill text layout took {}", duration);
    }

    fn move_command(
        &self,
        count: Option<usize>,
        cmd: &LapceCommand,
    ) -> Option<Movement> {
        match cmd {
            LapceCommand::Left => Some(Movement::Left),
            LapceCommand::Right => Some(Movement::Right),
            LapceCommand::Up => Some(Movement::Up),
            LapceCommand::Down => Some(Movement::Down),
            LapceCommand::LineStart => Some(Movement::StartOfLine),
            LapceCommand::LineEnd => Some(Movement::EndOfLine),
            LapceCommand::GotoLineDefaultFirst => Some(match count {
                Some(n) => Movement::Line(LinePosition::Line(n)),
                None => Movement::Line(LinePosition::First),
            }),
            LapceCommand::GotoLineDefaultLast => Some(match count {
                Some(n) => Movement::Line(LinePosition::Line(n)),
                None => Movement::Line(LinePosition::Last),
            }),
            LapceCommand::WordBackward => Some(Movement::WordBackward),
            LapceCommand::WordFoward => Some(Movement::WordForward),
            LapceCommand::WordEndForward => Some(Movement::WordEndForward),
            LapceCommand::MatchPairs => Some(Movement::MatchPairs),
            LapceCommand::NextUnmatchedRightBracket => {
                Some(Movement::NextUnmatched(')'))
            }
            LapceCommand::PreviousUnmatchedLeftBracket => {
                Some(Movement::PreviousUnmatched('('))
            }
            LapceCommand::NextUnmatchedRightCurlyBracket => {
                Some(Movement::NextUnmatched('}'))
            }
            LapceCommand::PreviousUnmatchedLeftCurlyBracket => {
                Some(Movement::PreviousUnmatched('{'))
            }
            _ => None,
        }
    }

    fn toggle_visual(&mut self, visual_mode: VisualMode) {
        let cursor = &mut Arc::make_mut(&mut self.editor).cursor;

        match &cursor.mode {
            CursorMode::Visual { start, end, mode } => {
                if mode != &visual_mode {
                    cursor.mode = CursorMode::Visual {
                        start: *start,
                        end: *end,
                        mode: visual_mode,
                    };
                } else {
                    cursor.mode = CursorMode::Normal(*end);
                };
            }
            _ => {
                let offset = cursor.offset();
                cursor.mode = CursorMode::Visual {
                    start: offset,
                    end: offset,
                    mode: visual_mode,
                };
            }
        }
    }

    fn scroll(&mut self, ctx: &mut EventCtx, down: bool, count: usize, env: &Env) {
        let line_height = env.get(LapceTheme::EDITOR_LINE_HEIGHT);
        let diff = line_height * count as f64;
        let diff = if down { diff } else { -diff };

        let offset = self.editor.cursor.offset();
        let (line, col) = self.buffer.offset_to_line_col(offset);
        let top = self.editor.scroll_offset.y + diff;
        let bottom = top + self.editor.size.height;

        let line = if (line + 1) as f64 * line_height + line_height > bottom {
            let line = (bottom / line_height).floor() as usize;
            if line > 2 {
                line - 2
            } else {
                0
            }
        } else if line as f64 * line_height - line_height < top {
            let line = (top / line_height).ceil() as usize;
            line + 1
        } else {
            line
        };

        let offset = self.buffer.offset_of_line(line)
            + col.min(self.buffer.line_max_col(line, false));
        self.set_cursor(Cursor::new(
            CursorMode::Normal(offset),
            self.editor.cursor.horiz.clone(),
        ));
        ctx.submit_command(Command::new(
            LAPCE_UI_COMMAND,
            LapceUICommand::ScrollTo((self.editor.scroll_offset.x, top)),
            Target::Widget(self.editor.container_id),
        ));
    }

    fn page_move(&mut self, ctx: &mut EventCtx, down: bool, env: &Env) {
        let line_height = env.get(LapceTheme::EDITOR_LINE_HEIGHT);
        let lines = (self.editor.size.height / line_height / 2.0).round() as usize;
        let distance = (lines as f64) * line_height;
        let offset = self.editor.cursor.offset();
        let (offset, horiz) = self.buffer.move_offset(
            offset,
            self.editor.cursor.horiz.as_ref(),
            lines,
            if down { &Movement::Down } else { &Movement::Up },
            false,
            false,
        );
        self.set_cursor(Cursor::new(CursorMode::Normal(offset), Some(horiz)));
        let rect = Rect::ZERO
            .with_origin(
                self.editor.scroll_offset.to_point()
                    + Vec2::new(0.0, if down { distance } else { -distance }),
            )
            .with_size(self.editor.size.clone());
        ctx.submit_command(Command::new(
            LAPCE_UI_COMMAND,
            LapceUICommand::EnsureRectVisible(rect),
            Target::Widget(self.editor.container_id),
        ));
    }

    pub fn do_move(&mut self, movement: &Movement, count: usize) {
        match &self.editor.cursor.mode {
            &CursorMode::Normal(offset) => {
                let (new_offset, horiz) = self.buffer.move_offset(
                    offset,
                    self.editor.cursor.horiz.as_ref(),
                    count,
                    movement,
                    false,
                    false,
                );
                let editor = Arc::make_mut(&mut self.editor);
                editor.cursor.mode = CursorMode::Normal(new_offset);
                editor.cursor.horiz = Some(horiz);
            }
            CursorMode::Visual { start, end, mode } => {
                let (new_offset, horiz) = self.buffer.move_offset(
                    *end,
                    self.editor.cursor.horiz.as_ref(),
                    count,
                    movement,
                    true,
                    false,
                );
                let start = *start;
                let mode = mode.clone();
                let editor = Arc::make_mut(&mut self.editor);
                editor.cursor.mode = CursorMode::Visual {
                    start,
                    end: new_offset,
                    mode,
                };
                editor.cursor.horiz = Some(horiz);
            }
            CursorMode::Insert(selection) => {
                let selection = self.buffer.update_selection(
                    selection, count, movement, true, false, false,
                );
                self.set_cursor(Cursor::new(CursorMode::Insert(selection), None));
            }
        }
    }

    pub fn cusor_region(&self, env: &Env) -> Rect {
        self.editor.cursor.region(&self.buffer, env)
    }

    pub fn insert_new_line(&mut self, ctx: &mut EventCtx, offset: usize) {
        let (line, col) = self.buffer.offset_to_line_col(offset);
        let line_content = self.buffer.line_content(line);
        let line_indent = self.buffer.indent_on_line(line);

        let indent = if previous_has_unmatched_pair(&line_content, col) {
            format!("{}    ", line_indent)
        } else if line_indent.len() >= col {
            line_indent[..col].to_string()
        } else {
            let next_line_indent = self.buffer.indent_on_line(line + 1);
            if next_line_indent.len() > line_indent.len() {
                next_line_indent
            } else {
                line_indent.clone()
            }
        };

        let selection = Selection::caret(offset);
        let content = format!("{}{}", "\n", indent);

        let selection =
            self.edit(ctx, &selection, &content, true, EditType::InsertNewline);
        let editor = Arc::make_mut(&mut self.editor);
        editor.cursor.mode = CursorMode::Insert(selection);
        editor.cursor.horiz = None;
    }

    fn set_cursor_after_change(&mut self, selection: Selection) {
        match self.editor.cursor.mode {
            CursorMode::Normal(_) | CursorMode::Visual { .. } => {
                let offset = selection.min_offset();
                let offset = self.buffer.offset_line_end(offset, false).min(offset);
                self.set_cursor(Cursor::new(CursorMode::Normal(offset), None));
            }
            CursorMode::Insert(_) => {
                self.set_cursor(Cursor::new(CursorMode::Insert(selection), None));
            }
        }
    }

    fn set_cursor(&mut self, cursor: Cursor) {
        let editor = Arc::make_mut(&mut self.editor);
        editor.cursor = cursor;
    }

    fn paste(&mut self, ctx: &mut EventCtx, data: &RegisterData) {
        match data.mode {
            VisualMode::Normal => {
                let selection = match self.editor.cursor.mode {
                    CursorMode::Normal(offset) => {
                        let line_end = self.buffer.offset_line_end(offset, true);
                        let offset = (offset + 1).min(line_end);
                        Selection::caret(offset)
                    }
                    CursorMode::Insert { .. } | CursorMode::Visual { .. } => {
                        self.editor.cursor.edit_selection(&self.buffer)
                    }
                };
                let after = !data.content.contains("\n");
                let selection = self.edit(
                    ctx,
                    &selection,
                    &data.content,
                    after,
                    EditType::InsertChars,
                );
                if !after {
                    self.set_cursor_after_change(selection);
                } else {
                    match self.editor.cursor.mode {
                        CursorMode::Normal(_) | CursorMode::Visual { .. } => {
                            let offset = selection.min_offset() - 1;
                            self.set_cursor(Cursor::new(
                                CursorMode::Normal(offset),
                                None,
                            ));
                        }
                        CursorMode::Insert { .. } => {
                            self.set_cursor(Cursor::new(
                                CursorMode::Insert(selection),
                                None,
                            ));
                        }
                    }
                }
            }
            VisualMode::Linewise | VisualMode::Blockwise => {
                let (selection, content) = match &self.editor.cursor.mode {
                    CursorMode::Normal(offset) => {
                        let line = self.buffer.line_of_offset(*offset);
                        let offset = self.buffer.offset_of_line(line + 1);
                        (Selection::caret(offset), data.content.clone())
                    }
                    CursorMode::Insert { .. } => (
                        self.editor.cursor.edit_selection(&self.buffer),
                        "\n".to_string() + &data.content,
                    ),
                    CursorMode::Visual { mode, .. } => {
                        let selection =
                            self.editor.cursor.edit_selection(&self.buffer);
                        let data = match mode {
                            VisualMode::Linewise => data.content.clone(),
                            _ => "\n".to_string() + &data.content,
                        };
                        (selection, data)
                    }
                };
                let selection = self.edit(
                    ctx,
                    &selection,
                    &content,
                    false,
                    EditType::InsertChars,
                );
                match self.editor.cursor.mode {
                    CursorMode::Normal(_) | CursorMode::Visual { .. } => {
                        let offset = selection.min_offset();
                        let offset = if self.editor.cursor.is_visual() {
                            offset + 1
                        } else {
                            offset
                        };
                        let line = self.buffer.line_of_offset(offset);
                        let offset =
                            self.buffer.first_non_blank_character_on_line(line);
                        self.set_cursor(Cursor::new(
                            CursorMode::Normal(offset),
                            None,
                        ));
                    }
                    CursorMode::Insert(_) => {
                        self.set_cursor(Cursor::new(
                            CursorMode::Insert(selection),
                            None,
                        ));
                    }
                }
            }
        }
    }

    fn edit(
        &mut self,
        ctx: &mut EventCtx,
        selection: &Selection,
        c: &str,
        after: bool,
        edit_type: EditType,
    ) -> Selection {
        match &self.editor.cursor.mode {
            CursorMode::Normal(_) => {
                if !selection.is_caret() {
                    let data = self.editor.cursor.yank(&self.buffer);
                    let register = Arc::make_mut(&mut self.main_split.register);
                    register.add_delete(data);
                }
            }
            CursorMode::Visual { start, end, mode } => {
                let data = self.editor.cursor.yank(&self.buffer);
                let register = Arc::make_mut(&mut self.main_split.register);
                register.add_delete(data);
            }
            CursorMode::Insert(_) => {}
        }

        let proxy = self.proxy.clone();
        let buffer = self.buffer_mut();
        let delta = buffer.edit(ctx, &selection, c, proxy, edit_type);
        let buffer_id = buffer.id;
        self.main_split.notify_update_text_layouts(ctx, &buffer_id);
        self.inactive_apply_delta(&delta);
        let selection = selection.apply_delta(&delta, after, InsertDrift::Default);
        selection
    }

    fn inactive_apply_delta(&mut self, delta: &RopeDelta) {
        let open_files = self.main_split.open_files.clone();
        for (view_id, editor) in self.main_split.editors.iter_mut() {
            if view_id != &self.editor.view_id {
                let editor_buffer_id = open_files.get(&editor.buffer).unwrap();
                if editor_buffer_id == &self.buffer.id {
                    Arc::make_mut(editor).cursor.apply_delta(delta);
                }
            }
        }
    }

    pub fn cancel_completion(&mut self) {
        let completion = Arc::make_mut(&mut self.completion);
        completion.cancel();
    }

    fn update_completion(&mut self, ctx: &mut EventCtx) {
        if self.get_mode() != Mode::Insert {
            return;
        }
        let offset = self.editor.cursor.offset();
        let start_offset = self.buffer.prev_code_boundary(offset);
        let end_offset = self.buffer.next_code_boundary(offset);
        let input = self
            .buffer
            .slice_to_cow(start_offset..end_offset)
            .to_string();
        let char = self
            .buffer
            .slice_to_cow(start_offset - 1..start_offset)
            .to_string();
        let completion = Arc::make_mut(&mut self.completion);
        if input == "" && char != "." && char != ":" {
            completion.cancel();
            return;
        }

        if completion.status != CompletionStatus::Inactive
            && completion.offset == start_offset
            && completion.buffer_id == self.buffer.id
        {
            println!("update input {}", input);
            completion.update_input(input);
            return;
        }

        completion.buffer_id = self.buffer.id;
        completion.offset = start_offset;
        completion.status = CompletionStatus::Started;
        completion.request_id += 1;
        let request_id = completion.request_id;
        let event_sink = ctx.get_external_handle();
        let completion_widget_id = self.completion.id;
        let buffer_id = self.buffer.id;
        println!("proxy get completion");
        self.proxy.get_completion(
            start_offset,
            self.buffer.id,
            self.buffer.offset_to_position(start_offset),
            Box::new(move |result| {
                if let Ok(res) = result {
                    println!("proxy completion result");
                    if let Ok(resp) =
                        serde_json::from_value::<CompletionResponse>(res)
                    {
                        event_sink.submit_command(
                            LAPCE_UI_COMMAND,
                            LapceUICommand::UpdateCompletion(request_id, resp),
                            Target::Widget(completion_widget_id),
                        );
                        return;
                    }
                }

                event_sink.submit_command(
                    LAPCE_UI_COMMAND,
                    LapceUICommand::CancelCompletion(request_id),
                    Target::Widget(completion_widget_id),
                );
            }),
        );
    }
}

pub struct LapceEditorLens(pub WidgetId);

impl Lens<LapceTabData, LapceEditorViewData> for LapceEditorLens {
    fn with<V, F: FnOnce(&LapceEditorViewData) -> V>(
        &self,
        data: &LapceTabData,
        f: F,
    ) -> V {
        let main_split = &data.main_split;
        let editor = main_split.editors.get(&self.0).unwrap();
        let editor_view = LapceEditorViewData {
            buffer: main_split
                .buffers
                .get(main_split.open_files.get(&editor.buffer).unwrap())
                .unwrap()
                .clone(),
            editor: editor.clone(),
            main_split: main_split.clone(),
            keypress: data.keypress.clone(),
            completion: data.completion.clone(),
            theme: data.theme.clone(),
            proxy: data.proxy.clone(),
        };
        f(&editor_view)
    }

    fn with_mut<V, F: FnOnce(&mut LapceEditorViewData) -> V>(
        &self,
        data: &mut LapceTabData,
        f: F,
    ) -> V {
        let editor = data.main_split.editors.get(&self.0).unwrap().clone();
        let buffer_id = *data.main_split.open_files.get(&editor.buffer).unwrap();
        let mut editor_view = LapceEditorViewData {
            buffer: data.main_split.buffers.get(&buffer_id).unwrap().clone(),
            editor: editor.clone(),
            main_split: data.main_split.clone(),
            keypress: data.keypress.clone(),
            completion: data.completion.clone(),
            theme: data.theme.clone(),
            proxy: data.proxy.clone(),
        };
        let result = f(&mut editor_view);

        data.keypress = editor_view.keypress.clone();
        data.completion = editor_view.completion.clone();
        data.main_split = editor_view.main_split.clone();
        data.theme = editor_view.theme.clone();
        if !editor.same(&editor_view.editor) {
            data.main_split
                .editors
                .insert(self.0, editor_view.editor.clone());
        }
        if !data
            .main_split
            .buffers
            .get(&buffer_id)
            .unwrap()
            .same(&editor_view.buffer)
        {
            data.main_split
                .buffers
                .insert(buffer_id, editor_view.buffer.clone());
        }

        result
    }
}

impl KeyPressFocus for LapceEditorViewData {
    fn get_mode(&self) -> Mode {
        match self.editor.cursor.mode {
            CursorMode::Normal(_) => Mode::Normal,
            CursorMode::Visual { .. } => Mode::Visual,
            CursorMode::Insert(_) => Mode::Insert,
        }
    }

    fn check_condition(&self, condition: &str) -> bool {
        let condition = condition.trim();
        let (reverse, condition) = if condition.starts_with("!") {
            (true, &condition[1..])
        } else {
            (false, condition)
        };
        let matched = match condition {
            "list_focus" => {
                self.completion.status == CompletionStatus::Done
                    && if self.completion.input == "" {
                        self.completion.items.len() > 0
                    } else {
                        self.completion.filtered_items.len() > 0
                    }
            }
            _ => false,
        };
        if reverse {
            !matched
        } else {
            matched
        }
    }

    fn run_command(
        &mut self,
        ctx: &mut EventCtx,
        cmd: &LapceCommand,
        count: Option<usize>,
        env: &Env,
    ) {
        if let Some(movement) = self.move_command(count, cmd) {
            self.do_move(&movement, count.unwrap_or(1));
            self.cancel_completion();
            return;
        }
        match cmd {
            LapceCommand::SplitLeft => {
                ctx.submit_command(Command::new(
                    LAPCE_UI_COMMAND,
                    LapceUICommand::SplitEditorMove(
                        SplitMoveDirection::Left,
                        self.editor.view_id,
                    ),
                    Target::Widget(self.editor.split_id),
                ));
            }
            LapceCommand::SplitRight => {
                ctx.submit_command(Command::new(
                    LAPCE_UI_COMMAND,
                    LapceUICommand::SplitEditorMove(
                        SplitMoveDirection::Right,
                        self.editor.view_id,
                    ),
                    Target::Widget(self.editor.split_id),
                ));
            }
            LapceCommand::SplitExchange => {
                ctx.submit_command(Command::new(
                    LAPCE_UI_COMMAND,
                    LapceUICommand::SplitEditorExchange(self.editor.view_id),
                    Target::Widget(self.editor.split_id),
                ));
            }
            LapceCommand::SplitVertical => {
                ctx.submit_command(Command::new(
                    LAPCE_UI_COMMAND,
                    LapceUICommand::SplitEditor(true, self.editor.view_id),
                    Target::Widget(self.editor.split_id),
                ));
            }
            LapceCommand::SplitClose => {
                ctx.submit_command(Command::new(
                    LAPCE_UI_COMMAND,
                    LapceUICommand::SplitEditorClose(self.editor.view_id),
                    Target::Widget(self.editor.split_id),
                ));
            }
            LapceCommand::Undo => {
                let proxy = self.proxy.clone();
                let buffer = self.buffer_mut();
                if let Some(delta) = buffer.do_undo(proxy) {
                    let buffer_id = buffer.id;
                    self.main_split.notify_update_text_layouts(ctx, &buffer_id);
                    let selection = Selection::caret(self.editor.cursor.offset())
                        .apply_delta(&delta, true, InsertDrift::Default);
                    self.set_cursor_after_change(selection);
                }
            }
            LapceCommand::Redo => {
                let proxy = self.proxy.clone();
                let buffer = self.buffer_mut();
                if let Some(delta) = buffer.do_redo(proxy) {
                    let buffer_id = buffer.id;
                    self.main_split.notify_update_text_layouts(ctx, &buffer_id);
                    let selection = Selection::caret(self.editor.cursor.offset())
                        .apply_delta(&delta, true, InsertDrift::Default);
                    self.set_cursor_after_change(selection);
                }
            }
            LapceCommand::Append => {
                let offset = self
                    .buffer
                    .move_offset(
                        self.editor.cursor.offset(),
                        None,
                        1,
                        &Movement::Right,
                        true,
                        false,
                    )
                    .0;
                self.buffer_mut().update_edit_type();
                self.set_cursor(Cursor::new(
                    CursorMode::Insert(Selection::caret(offset)),
                    None,
                ));
            }
            LapceCommand::AppendEndOfLine => {
                let (offset, horiz) = self.buffer.move_offset(
                    self.editor.cursor.offset(),
                    None,
                    1,
                    &Movement::EndOfLine,
                    true,
                    false,
                );
                self.buffer_mut().update_edit_type();
                self.set_cursor(Cursor::new(
                    CursorMode::Insert(Selection::caret(offset)),
                    Some(horiz),
                ));
            }
            LapceCommand::InsertMode => {
                Arc::make_mut(&mut self.editor).cursor.mode = CursorMode::Insert(
                    Selection::caret(self.editor.cursor.offset()),
                );
                self.buffer_mut().update_edit_type();
            }
            LapceCommand::InsertFirstNonBlank => {
                match &self.editor.cursor.mode {
                    CursorMode::Normal(offset) => {
                        let (offset, horiz) = self.buffer.move_offset(
                            *offset,
                            None,
                            1,
                            &Movement::FirstNonBlank,
                            true,
                            false,
                        );
                        self.buffer_mut().update_edit_type();
                        self.set_cursor(Cursor::new(
                            CursorMode::Insert(Selection::caret(offset)),
                            Some(horiz),
                        ));
                    }
                    CursorMode::Visual { start, end, mode } => {
                        let mut selection = Selection::new();
                        for region in
                            self.editor.cursor.edit_selection(&self.buffer).regions()
                        {
                            selection.add_region(SelRegion::caret(region.min()));
                        }
                        self.buffer_mut().update_edit_type();
                        self.set_cursor(Cursor::new(
                            CursorMode::Insert(selection),
                            None,
                        ));
                    }
                    CursorMode::Insert(_) => {}
                };
            }
            LapceCommand::NewLineAbove => {
                let line = self.editor.cursor.current_line(&self.buffer);
                let offset = if line > 0 {
                    self.buffer.line_end_offset(line - 1, true)
                } else {
                    self.buffer.first_non_blank_character_on_line(line)
                };
                self.insert_new_line(ctx, offset);
            }
            LapceCommand::NewLineBelow => {
                let offset = self.editor.cursor.offset();
                let offset = self.buffer.offset_line_end(offset, true);
                self.insert_new_line(ctx, offset);
            }
            LapceCommand::DeleteToBeginningOfLine => {
                let selection = match self.editor.cursor.mode {
                    CursorMode::Normal(_) | CursorMode::Visual { .. } => {
                        self.editor.cursor.edit_selection(&self.buffer)
                    }
                    CursorMode::Insert(_) => {
                        let selection =
                            self.editor.cursor.edit_selection(&self.buffer);
                        let selection = self.buffer.update_selection(
                            &selection,
                            1,
                            &Movement::StartOfLine,
                            true,
                            true,
                            true,
                        );
                        selection
                    }
                };
                let selection =
                    self.edit(ctx, &selection, "", true, EditType::Delete);
                match self.editor.cursor.mode {
                    CursorMode::Normal(_) | CursorMode::Visual { .. } => {
                        let offset = selection.min_offset();
                        let offset =
                            self.buffer.offset_line_end(offset, false).min(offset);
                        self.set_cursor(Cursor::new(
                            CursorMode::Normal(offset),
                            None,
                        ));
                    }
                    CursorMode::Insert(_) => {
                        self.set_cursor(Cursor::new(
                            CursorMode::Insert(selection),
                            None,
                        ));
                    }
                }
            }
            LapceCommand::Yank => {
                let data = self.editor.cursor.yank(&self.buffer);
                let register = Arc::make_mut(&mut self.main_split.register);
                register.add_yank(data);
                match &self.editor.cursor.mode {
                    CursorMode::Visual { start, end, mode } => {
                        let offset = *start.min(end);
                        let offset =
                            self.buffer.offset_line_end(offset, false).min(offset);
                        self.set_cursor(Cursor::new(
                            CursorMode::Normal(offset),
                            None,
                        ));
                    }
                    CursorMode::Normal(_) => {}
                    CursorMode::Insert(_) => {}
                }
            }
            LapceCommand::ClipboardCopy => {
                let data = self.editor.cursor.yank(&self.buffer);
                Application::global().clipboard().put_string(data.content);
            }
            LapceCommand::ClipboardPaste => {
                if let Some(s) = Application::global().clipboard().get_string() {
                    let data = RegisterData {
                        content: s.to_string(),
                        mode: VisualMode::Normal,
                    };
                    self.paste(ctx, &data);
                }
            }
            LapceCommand::Paste => {
                let data = self.main_split.register.unamed.clone();
                self.paste(ctx, &data);
            }
            LapceCommand::DeleteWordBackward => {
                let selection = match self.editor.cursor.mode {
                    CursorMode::Normal(_) | CursorMode::Visual { .. } => {
                        self.editor.cursor.edit_selection(&self.buffer)
                    }
                    CursorMode::Insert(_) => {
                        let selection =
                            self.editor.cursor.edit_selection(&self.buffer);
                        let selection = self.buffer.update_selection(
                            &selection,
                            1,
                            &Movement::WordBackward,
                            true,
                            true,
                            true,
                        );
                        selection
                    }
                };
                let selection =
                    self.edit(ctx, &selection, "", true, EditType::Delete);
                self.set_cursor_after_change(selection);
                self.update_completion(ctx);
            }
            LapceCommand::DeleteBackward => {
                let selection = match self.editor.cursor.mode {
                    CursorMode::Normal(_) | CursorMode::Visual { .. } => {
                        self.editor.cursor.edit_selection(&self.buffer)
                    }
                    CursorMode::Insert(_) => {
                        let selection =
                            self.editor.cursor.edit_selection(&self.buffer);
                        let selection = self.buffer.update_selection(
                            &selection,
                            1,
                            &Movement::Left,
                            true,
                            true,
                            true,
                        );
                        selection
                    }
                };
                let selection =
                    self.edit(ctx, &selection, "", true, EditType::Delete);
                self.set_cursor_after_change(selection);
                self.update_completion(ctx);
            }
            LapceCommand::DeleteForeward => {
                let selection = self.editor.cursor.edit_selection(&self.buffer);
                let selection =
                    self.edit(ctx, &selection, "", true, EditType::Delete);
                self.set_cursor_after_change(selection);
                self.update_completion(ctx);
            }
            LapceCommand::DeleteForewardAndInsert => {
                let selection = self.editor.cursor.edit_selection(&self.buffer);
                let selection =
                    self.edit(ctx, &selection, "", true, EditType::Delete);
                self.set_cursor(Cursor::new(CursorMode::Insert(selection), None));
                self.update_completion(ctx);
            }
            LapceCommand::InsertNewLine => {
                let selection = self.editor.cursor.edit_selection(&self.buffer);
                if selection.regions().len() > 1 {
                    let selection = self.edit(
                        ctx,
                        &selection,
                        "\n",
                        true,
                        EditType::InsertNewline,
                    );
                    self.set_cursor(Cursor::new(
                        CursorMode::Insert(selection),
                        None,
                    ));
                    return;
                };
                self.insert_new_line(ctx, self.editor.cursor.offset());
                self.update_completion(ctx);
            }
            LapceCommand::ToggleVisualMode => {
                self.toggle_visual(VisualMode::Normal);
            }
            LapceCommand::ToggleLinewiseVisualMode => {
                self.toggle_visual(VisualMode::Linewise);
            }
            LapceCommand::ToggleBlockwiseVisualMode => {
                self.toggle_visual(VisualMode::Blockwise);
            }
            LapceCommand::ScrollDown => {
                self.scroll(ctx, true, count.unwrap_or(1), env);
            }
            LapceCommand::ScrollUp => {
                self.scroll(ctx, false, count.unwrap_or(1), env);
            }
            LapceCommand::PageDown => {
                self.page_move(ctx, true, env);
            }
            LapceCommand::PageUp => {
                self.page_move(ctx, false, env);
            }
            LapceCommand::ListNext => {
                let completion = Arc::make_mut(&mut self.completion);
                completion.next();
            }
            LapceCommand::ListPrevious => {
                let completion = Arc::make_mut(&mut self.completion);
                completion.previous();
            }
            LapceCommand::ListSelect => {
                let selection = self.editor.cursor.edit_selection(&self.buffer);

                let count = self.completion.input.len();
                let selection = if count > 0 {
                    self.buffer.update_selection(
                        &selection,
                        count,
                        &Movement::Left,
                        true,
                        false,
                        true,
                    )
                } else {
                    selection
                };

                let content = self.completion.current().to_string();
                let selection = self.edit(
                    ctx,
                    &selection,
                    &content,
                    true,
                    EditType::InsertChars,
                );
                self.set_cursor_after_change(selection);
                self.cancel_completion();
            }
            LapceCommand::NormalMode => {
                let offset = match &self.editor.cursor.mode {
                    CursorMode::Insert(selection) => {
                        self.buffer
                            .move_offset(
                                selection.get_cursor_offset(),
                                None,
                                1,
                                &Movement::Left,
                                false,
                                false,
                            )
                            .0
                    }
                    CursorMode::Visual { start, end, mode } => {
                        self.buffer.offset_line_end(*end, false).min(*end)
                    }
                    CursorMode::Normal(offset) => *offset,
                };
                self.buffer_mut().update_edit_type();
                let mut cursor = &mut Arc::make_mut(&mut self.editor).cursor;
                cursor.mode = CursorMode::Normal(offset);
                cursor.horiz = None;
                self.cancel_completion();
            }
            _ => (),
        }
    }

    fn insert(&mut self, ctx: &mut EventCtx, c: &str) {
        if self.get_mode() == Mode::Insert {
            let selection = self.editor.cursor.edit_selection(&self.buffer);
            let selection =
                self.edit(ctx, &selection, c, true, EditType::InsertChars);
            let editor = Arc::make_mut(&mut self.editor);
            editor.cursor.mode = CursorMode::Insert(selection);
            editor.cursor.horiz = None;
            self.update_completion(ctx);
        }
    }
}

pub fn hex_to_color(hex: &str) -> Result<Color> {
    let hex = hex.trim_start_matches("#");
    let (r, g, b, a) = match hex.len() {
        3 => (
            format!("{}{}", &hex[0..0], &hex[0..0]),
            format!("{}{}", &hex[1..1], &hex[1..1]),
            format!("{}{}", &hex[2..2], &hex[2..2]),
            "ff".to_string(),
        ),
        6 => (
            hex[0..2].to_string(),
            hex[2..4].to_string(),
            hex[4..6].to_string(),
            "ff".to_string(),
        ),
        8 => (
            hex[0..2].to_string(),
            hex[2..4].to_string(),
            hex[4..6].to_string(),
            hex[6..8].to_string(),
        ),
        _ => return Err(anyhow!("invalid hex color")),
    };
    Ok(Color::rgba8(
        u8::from_str_radix(&r, 16)?,
        u8::from_str_radix(&g, 16)?,
        u8::from_str_radix(&b, 16)?,
        u8::from_str_radix(&a, 16)?,
    ))
}