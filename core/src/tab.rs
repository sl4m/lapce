use std::{path::PathBuf, sync::Arc, thread};

use druid::{
    theme, BoxConstraints, Command, Env, Event, EventCtx, Insets, LayoutCtx,
    LifeCycle, LifeCycleCtx, PaintCtx, Point, Size, Target, Widget, WidgetExt,
    WidgetId, WidgetPod,
};

use crate::{
    buffer::{BufferId, BufferNew, BufferState, BufferUpdate, UpdateEvent},
    command::{LapceUICommand, LAPCE_UI_COMMAND},
    completion::{CompletionContainer, CompletionNew, CompletionStatus},
    data::{LapceEditorLens, LapceMainSplitData, LapceTabData},
    editor::LapceEditorView,
    scroll::LapceScrollNew,
    split::LapceSplitNew,
    state::{LapceWorkspace, LapceWorkspaceType},
};

pub struct LapceTabNew {
    id: WidgetId,
    main_split: WidgetPod<LapceTabData, Box<dyn Widget<LapceTabData>>>,
    completion: WidgetPod<LapceTabData, Box<dyn Widget<LapceTabData>>>,
}

impl LapceTabNew {
    pub fn new(data: &LapceTabData) -> Self {
        let editor = data.main_split.editors.iter().next().unwrap().1;
        let main_split = LapceSplitNew::new(*data.main_split.split_id)
            .with_flex_child(
                LapceEditorView::new(
                    editor.view_id,
                    editor.container_id,
                    editor.editor_id,
                )
                .lens(LapceEditorLens(editor.view_id))
                .boxed(),
                1.0,
            );
        let completion = CompletionContainer::new(&data.completion);

        Self {
            id: data.id,
            main_split: WidgetPod::new(main_split.boxed()),
            completion: WidgetPod::new(completion.boxed()),
        }
    }
}

impl Widget<LapceTabData> for LapceTabNew {
    fn id(&self) -> Option<WidgetId> {
        Some(self.id)
    }

    fn event(
        &mut self,
        ctx: &mut EventCtx,
        event: &Event,
        data: &mut LapceTabData,
        env: &Env,
    ) {
        match event {
            Event::WindowConnected => {
                for (_, buffer) in data.main_split.buffers.iter() {
                    if !buffer.loaded {
                        buffer.retrieve_file(
                            data.proxy.clone(),
                            data.id,
                            ctx.get_external_handle(),
                        );
                    }
                }
                let receiver = data.update_receiver.take().unwrap();
                let event_sink = ctx.get_external_handle();
                let tab_id = self.id;
                thread::spawn(move || {
                    LapceTabData::buffer_update_process(
                        tab_id, receiver, event_sink,
                    );
                });
                let workspace = LapceWorkspace {
                    kind: LapceWorkspaceType::Local,
                    path: PathBuf::from("/Users/Lulu/lapce"),
                };
                data.proxy.start(workspace, ctx.get_external_handle());
            }
            Event::Command(cmd) if cmd.is(LAPCE_UI_COMMAND) => {
                let command = cmd.get_unchecked(LAPCE_UI_COMMAND);
                match command {
                    LapceUICommand::UpdateWindowOrigin => {
                        data.window_origin = ctx.window_origin();
                    }
                    LapceUICommand::LoadBuffer { id, content } => {
                        let buffer = data.main_split.buffers.get_mut(id).unwrap();
                        Arc::make_mut(buffer).load_content(content);
                        data.main_split.notify_update_text_layouts(ctx, id);
                        ctx.set_handled();
                    }
                    LapceUICommand::UpdateSemanticTokens(id, rev, tokens) => {
                        if let Some(buffer) = data.main_split.buffers.get(id) {
                            if buffer.rev == *rev {
                                if let Some(language) = buffer.language.as_ref() {
                                    data.update_sender.send(
                                        UpdateEvent::SemanticTokens(
                                            BufferUpdate {
                                                id: buffer.id,
                                                rope: buffer.rope.clone(),
                                                rev: *rev,
                                                language: *language,
                                                highlights: buffer.styles.clone(),
                                            },
                                            tokens.to_owned(),
                                        ),
                                    );
                                }
                            }
                        }
                        ctx.set_handled();
                    }
                    LapceUICommand::UpdateStyle {
                        id,
                        rev,
                        highlights,
                        semantic_tokens,
                    } => {
                        let buffer = data.main_split.buffers.get_mut(id).unwrap();
                        Arc::make_mut(buffer).update_styles(
                            *rev,
                            highlights.to_owned(),
                            *semantic_tokens,
                        );
                        data.main_split.notify_update_text_layouts(ctx, id);
                        ctx.set_handled();
                    }
                    _ => (),
                }
            }
            _ => (),
        }
        self.completion.event(ctx, event, data, env);
        self.main_split.event(ctx, event, data, env);
    }

    fn lifecycle(
        &mut self,
        ctx: &mut LifeCycleCtx,
        event: &LifeCycle,
        data: &LapceTabData,
        env: &Env,
    ) {
        self.main_split.lifecycle(ctx, event, data, env);
        self.completion.lifecycle(ctx, event, data, env);
    }

    fn update(
        &mut self,
        ctx: &mut druid::UpdateCtx,
        old_data: &LapceTabData,
        data: &LapceTabData,
        env: &Env,
    ) {
        if data.completion.status == CompletionStatus::Done {
            let completion = &data.completion;
            let old_editor = old_data.main_split.active_editor();
            let editor = data.main_split.active_editor();
            if old_editor.window_origin != editor.window_origin {
                let completion_origin = data.completion_origin(ctx.size(), env);
                let rect = completion.size.to_rect().with_origin(completion_origin)
                    + Insets::new(1.0, 1.0, 1.0, 1.0);
                ctx.request_paint_rect(rect);
            }
        }

        self.main_split.update(ctx, data, env);
        self.completion.update(ctx, data, env);
    }

    fn layout(
        &mut self,
        ctx: &mut LayoutCtx,
        bc: &BoxConstraints,
        data: &LapceTabData,
        env: &Env,
    ) -> Size {
        let self_size = bc.max();
        self.main_split.layout(ctx, bc, data, env);
        self.main_split.set_origin(ctx, data, env, Point::ZERO);

        let completion_origin = data.completion_origin(self_size.clone(), env);
        self.completion.layout(ctx, bc, data, env);
        self.completion
            .set_origin(ctx, data, env, completion_origin);
        println!("completion origin {}", completion_origin);

        self_size
    }

    fn paint(&mut self, ctx: &mut PaintCtx, data: &LapceTabData, env: &Env) {
        self.main_split.paint(ctx, data, env);
        self.completion.paint(ctx, data, env);
    }
}