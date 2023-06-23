use std::{cmp, sync::Arc};

use crate::{
    display_map::Inlay, editor_settings, Anchor, Editor, ExcerptId, InlayId, MultiBuffer,
    MultiBufferSnapshot,
};
use anyhow::Context;
use clock::Global;
use gpui::{ModelHandle, Task, ViewContext};
use language::{Buffer, BufferSnapshot};
use log::error;
use parking_lot::RwLock;
use project::{InlayHint, InlayHintKind};

use collections::{hash_map, HashMap, HashSet};
use util::post_inc;

pub struct InlayHintCache {
    pub hints: HashMap<ExcerptId, Arc<RwLock<CachedExcerptHints>>>,
    pub allowed_hint_kinds: HashSet<Option<InlayHintKind>>,
    pub version: usize,
    update_tasks: HashMap<ExcerptId, InlayHintUpdateTask>,
}

struct InlayHintUpdateTask {
    version: usize,
    _task: Task<()>,
}

#[derive(Debug)]
pub struct CachedExcerptHints {
    version: usize,
    buffer_version: Global,
    pub hints: Vec<(InlayId, InlayHint)>,
}

#[derive(Debug, Clone, Copy)]
struct ExcerptQuery {
    buffer_id: u64,
    excerpt_id: ExcerptId,
    excerpt_range_start: language::Anchor,
    excerpt_range_end: language::Anchor,
    cache_version: usize,
    invalidate: InvalidationStrategy,
}
impl ExcerptQuery {
    fn contains_position(&self, position: text::Anchor, buffer_snapshot: &BufferSnapshot) -> bool {
        self.excerpt_range_start
            .cmp(&position, buffer_snapshot)
            .is_le()
            && self
                .excerpt_range_end
                .cmp(&position, buffer_snapshot)
                .is_ge()
    }
}

#[derive(Debug, Clone, Copy)]
pub enum InvalidationStrategy {
    All,
    OnConflict,
    None,
}

#[derive(Debug, Default)]
pub struct InlaySplice {
    pub to_remove: Vec<InlayId>,
    pub to_insert: Vec<(Anchor, InlayId, InlayHint)>,
}

#[derive(Debug)]
struct ExcerptHintsUpdate {
    excerpt_id: ExcerptId,
    cache_version: usize,
    remove_from_visible: Vec<InlayId>,
    remove_from_cache: HashSet<InlayId>,
    add_to_cache: Vec<InlayHint>,
}

impl InlayHintCache {
    pub fn new(inlay_hint_settings: editor_settings::InlayHints) -> Self {
        Self {
            allowed_hint_kinds: allowed_hint_types(inlay_hint_settings),
            hints: HashMap::default(),
            update_tasks: HashMap::default(),
            version: 0,
        }
    }

    pub fn update_settings(
        &mut self,
        multi_buffer: &ModelHandle<MultiBuffer>,
        inlay_hint_settings: editor_settings::InlayHints,
        visible_hints: Vec<Inlay>,
        cx: &mut ViewContext<Editor>,
    ) -> Option<InlaySplice> {
        let new_allowed_hint_kinds = allowed_hint_types(inlay_hint_settings);
        if !inlay_hint_settings.enabled {
            if self.hints.is_empty() {
                self.allowed_hint_kinds = new_allowed_hint_kinds;
                None
            } else {
                self.clear();
                self.allowed_hint_kinds = new_allowed_hint_kinds;
                Some(InlaySplice {
                    to_remove: visible_hints.iter().map(|inlay| inlay.id).collect(),
                    to_insert: Vec::new(),
                })
            }
        } else if new_allowed_hint_kinds == self.allowed_hint_kinds {
            None
        } else {
            let new_splice = self.new_allowed_hint_kinds_splice(
                multi_buffer,
                &visible_hints,
                &new_allowed_hint_kinds,
                cx,
            );
            if new_splice.is_some() {
                self.version += 1;
                self.update_tasks.clear();
                self.allowed_hint_kinds = new_allowed_hint_kinds;
            }
            new_splice
        }
    }

    pub fn spawn_hints_update(
        &mut self,
        mut excerpts_to_query: HashMap<ExcerptId, ModelHandle<Buffer>>,
        invalidate: InvalidationStrategy,
        cx: &mut ViewContext<Editor>,
    ) {
        let update_tasks = &mut self.update_tasks;
        let invalidate_cache = matches!(
            invalidate,
            InvalidationStrategy::All | InvalidationStrategy::OnConflict
        );
        if invalidate_cache {
            update_tasks
                .retain(|task_excerpt_id, _| excerpts_to_query.contains_key(task_excerpt_id));
        }
        let cache_version = self.version;
        excerpts_to_query.retain(|visible_excerpt_id, _| {
            match update_tasks.entry(*visible_excerpt_id) {
                hash_map::Entry::Occupied(o) => match o.get().version.cmp(&cache_version) {
                    cmp::Ordering::Less => true,
                    cmp::Ordering::Equal => invalidate_cache,
                    cmp::Ordering::Greater => false,
                },
                hash_map::Entry::Vacant(_) => true,
            }
        });

        if invalidate_cache {
            update_tasks
                .retain(|task_excerpt_id, _| excerpts_to_query.contains_key(task_excerpt_id));
        }
        excerpts_to_query.retain(|visible_excerpt_id, _| {
            match update_tasks.entry(*visible_excerpt_id) {
                hash_map::Entry::Occupied(o) => match o.get().version.cmp(&cache_version) {
                    cmp::Ordering::Less => true,
                    cmp::Ordering::Equal => invalidate_cache,
                    cmp::Ordering::Greater => false,
                },
                hash_map::Entry::Vacant(_) => true,
            }
        });

        cx.spawn(|editor, mut cx| async move {
            editor
                .update(&mut cx, |editor, cx| {
                    let visible_hints =
                        Arc::new(visible_inlay_hints(editor, cx).cloned().collect::<Vec<_>>());
                    for (excerpt_id, buffer_handle) in excerpts_to_query {
                        let (multi_buffer_snapshot, excerpt_range) =
                            editor.buffer.update(cx, |multi_buffer, cx| {
                                let multi_buffer_snapshot = multi_buffer.snapshot(cx);
                                (
                                    multi_buffer_snapshot,
                                    multi_buffer
                                        .excerpts_for_buffer(&buffer_handle, cx)
                                        .into_iter()
                                        .find(|(id, _)| id == &excerpt_id)
                                        .map(|(_, range)| range.context),
                                )
                            });

                        if let Some(excerpt_range) = excerpt_range {
                            let buffer = buffer_handle.read(cx);
                            let buffer_snapshot = buffer.snapshot();
                            let query = ExcerptQuery {
                                buffer_id: buffer.remote_id(),
                                excerpt_id,
                                excerpt_range_start: excerpt_range.start,
                                excerpt_range_end: excerpt_range.end,
                                cache_version,
                                invalidate,
                            };
                            let cached_excxerpt_hints =
                                editor.inlay_hint_cache.hints.get(&excerpt_id).cloned();

                            if let Some(cached_excerpt_hints) = &cached_excxerpt_hints {
                                let new_task_buffer_version = buffer_snapshot.version();
                                let cached_excerpt_hints = cached_excerpt_hints.read();
                                let cached_buffer_version = &cached_excerpt_hints.buffer_version;
                                if cached_buffer_version.changed_since(new_task_buffer_version) {
                                    return;
                                }
                                if !new_task_buffer_version.changed_since(&cached_buffer_version)
                                    && !matches!(invalidate, InvalidationStrategy::All)
                                {
                                    return;
                                }
                            }

                            editor.inlay_hint_cache.update_tasks.insert(
                                excerpt_id,
                                new_update_task(
                                    query,
                                    multi_buffer_snapshot,
                                    buffer_snapshot,
                                    Arc::clone(&visible_hints),
                                    cached_excxerpt_hints,
                                    cx,
                                ),
                            );
                        }
                    }
                })
                .ok();
        })
        .detach();
    }

    fn new_allowed_hint_kinds_splice(
        &self,
        multi_buffer: &ModelHandle<MultiBuffer>,
        visible_hints: &[Inlay],
        new_kinds: &HashSet<Option<InlayHintKind>>,
        cx: &mut ViewContext<Editor>,
    ) -> Option<InlaySplice> {
        let old_kinds = &self.allowed_hint_kinds;
        if new_kinds == old_kinds {
            return None;
        }

        let mut to_remove = Vec::new();
        let mut to_insert = Vec::new();
        let mut shown_hints_to_remove = visible_hints.iter().fold(
            HashMap::<ExcerptId, Vec<(Anchor, InlayId)>>::default(),
            |mut current_hints, inlay| {
                current_hints
                    .entry(inlay.position.excerpt_id)
                    .or_default()
                    .push((inlay.position, inlay.id));
                current_hints
            },
        );

        let multi_buffer = multi_buffer.read(cx);
        let multi_buffer_snapshot = multi_buffer.snapshot(cx);

        for (excerpt_id, excerpt_cached_hints) in &self.hints {
            let shown_excerpt_hints_to_remove =
                shown_hints_to_remove.entry(*excerpt_id).or_default();
            let excerpt_cached_hints = excerpt_cached_hints.read();
            let mut excerpt_cache = excerpt_cached_hints.hints.iter().fuse().peekable();
            shown_excerpt_hints_to_remove.retain(|(shown_anchor, shown_hint_id)| {
                let Some(buffer) = shown_anchor
                    .buffer_id
                    .and_then(|buffer_id| multi_buffer.buffer(buffer_id)) else { return false };
                let buffer_snapshot = buffer.read(cx).snapshot();
                loop {
                    match excerpt_cache.peek() {
                        Some((cached_hint_id, cached_hint)) => {
                            if cached_hint_id == shown_hint_id {
                                excerpt_cache.next();
                                return !new_kinds.contains(&cached_hint.kind);
                            }

                            match cached_hint
                                .position
                                .cmp(&shown_anchor.text_anchor, &buffer_snapshot)
                            {
                                cmp::Ordering::Less | cmp::Ordering::Equal => {
                                    if !old_kinds.contains(&cached_hint.kind)
                                        && new_kinds.contains(&cached_hint.kind)
                                    {
                                        to_insert.push((
                                            multi_buffer_snapshot.anchor_in_excerpt(
                                                *excerpt_id,
                                                cached_hint.position,
                                            ),
                                            *cached_hint_id,
                                            cached_hint.clone(),
                                        ));
                                    }
                                    excerpt_cache.next();
                                }
                                cmp::Ordering::Greater => return true,
                            }
                        }
                        None => return true,
                    }
                }
            });

            for (cached_hint_id, maybe_missed_cached_hint) in excerpt_cache {
                let cached_hint_kind = maybe_missed_cached_hint.kind;
                if !old_kinds.contains(&cached_hint_kind) && new_kinds.contains(&cached_hint_kind) {
                    to_insert.push((
                        multi_buffer_snapshot
                            .anchor_in_excerpt(*excerpt_id, maybe_missed_cached_hint.position),
                        *cached_hint_id,
                        maybe_missed_cached_hint.clone(),
                    ));
                }
            }
        }

        to_remove.extend(
            shown_hints_to_remove
                .into_values()
                .flatten()
                .map(|(_, hint_id)| hint_id),
        );
        if to_remove.is_empty() && to_insert.is_empty() {
            None
        } else {
            Some(InlaySplice {
                to_remove,
                to_insert,
            })
        }
    }

    fn clear(&mut self) {
        self.version += 1;
        self.update_tasks.clear();
        self.hints.clear();
        self.allowed_hint_kinds.clear();
    }
}

fn new_update_task(
    query: ExcerptQuery,
    multi_buffer_snapshot: MultiBufferSnapshot,
    buffer_snapshot: BufferSnapshot,
    visible_hints: Arc<Vec<Inlay>>,
    cached_excerpt_hints: Option<Arc<RwLock<CachedExcerptHints>>>,
    cx: &mut ViewContext<'_, '_, Editor>,
) -> InlayHintUpdateTask {
    let hints_fetch_task = hints_fetch_task(query, cx);
    InlayHintUpdateTask {
        version: query.cache_version,
        _task: cx.spawn(|editor, mut cx| async move {
            match hints_fetch_task.await {
                Ok(Some(new_hints)) => {
                    let task_buffer_snapshot = buffer_snapshot.clone();
                    if let Some(new_update) = cx
                        .background()
                        .spawn(async move {
                            new_excerpt_hints_update_result(
                                query,
                                new_hints,
                                &task_buffer_snapshot,
                                cached_excerpt_hints,
                                &visible_hints,
                            )
                        })
                        .await
                    {
                        editor
                            .update(&mut cx, |editor, cx| {
                                let cached_excerpt_hints = editor
                                    .inlay_hint_cache
                                    .hints
                                    .entry(new_update.excerpt_id)
                                    .or_insert_with(|| {
                                        Arc::new(RwLock::new(CachedExcerptHints {
                                            version: new_update.cache_version,
                                            buffer_version: buffer_snapshot.version().clone(),
                                            hints: Vec::new(),
                                        }))
                                    });
                                let mut cached_excerpt_hints = cached_excerpt_hints.write();
                                match new_update.cache_version.cmp(&cached_excerpt_hints.version) {
                                    cmp::Ordering::Less => return,
                                    cmp::Ordering::Greater | cmp::Ordering::Equal => {
                                        cached_excerpt_hints.version = new_update.cache_version;
                                    }
                                }
                                cached_excerpt_hints.hints.retain(|(hint_id, _)| {
                                    !new_update.remove_from_cache.contains(hint_id)
                                });
                                cached_excerpt_hints.buffer_version =
                                    buffer_snapshot.version().clone();
                                editor.inlay_hint_cache.version += 1;

                                let mut splice = InlaySplice {
                                    to_remove: new_update.remove_from_visible,
                                    to_insert: Vec::new(),
                                };

                                for new_hint in new_update.add_to_cache {
                                    let new_hint_position = multi_buffer_snapshot
                                        .anchor_in_excerpt(query.excerpt_id, new_hint.position);
                                    let new_inlay_id = InlayId(post_inc(&mut editor.next_inlay_id));
                                    if editor
                                        .inlay_hint_cache
                                        .allowed_hint_kinds
                                        .contains(&new_hint.kind)
                                    {
                                        splice.to_insert.push((
                                            new_hint_position,
                                            new_inlay_id,
                                            new_hint.clone(),
                                        ));
                                    }

                                    cached_excerpt_hints.hints.push((new_inlay_id, new_hint));
                                }

                                cached_excerpt_hints
                                    .hints
                                    .sort_by(|(_, hint_a), (_, hint_b)| {
                                        hint_a.position.cmp(&hint_b.position, &buffer_snapshot)
                                    });
                                drop(cached_excerpt_hints);

                                let InlaySplice {
                                    to_remove,
                                    to_insert,
                                } = splice;
                                if !to_remove.is_empty() || !to_insert.is_empty() {
                                    editor.splice_inlay_hints(to_remove, to_insert, cx)
                                }
                            })
                            .ok();
                    }
                }
                Ok(None) => {}
                Err(e) => error!(
                    "Failed to fecth hints for excerpt {:?} in buffer {} : {}",
                    query.excerpt_id, query.buffer_id, e
                ),
            }
        }),
    }
}

fn new_excerpt_hints_update_result(
    query: ExcerptQuery,
    new_excerpt_hints: Vec<InlayHint>,
    buffer_snapshot: &BufferSnapshot,
    cached_excerpt_hints: Option<Arc<RwLock<CachedExcerptHints>>>,
    visible_hints: &[Inlay],
) -> Option<ExcerptHintsUpdate> {
    let mut add_to_cache: Vec<InlayHint> = Vec::new();

    let mut excerpt_hints_to_persist = HashMap::default();
    for new_hint in new_excerpt_hints {
        if !query.contains_position(new_hint.position, buffer_snapshot) {
            continue;
        }
        let missing_from_cache = match &cached_excerpt_hints {
            Some(cached_excerpt_hints) => {
                let cached_excerpt_hints = cached_excerpt_hints.read();
                match cached_excerpt_hints.hints.binary_search_by(|probe| {
                    probe.1.position.cmp(&new_hint.position, buffer_snapshot)
                }) {
                    Ok(ix) => {
                        let (cached_inlay_id, cached_hint) = &cached_excerpt_hints.hints[ix];
                        if cached_hint == &new_hint {
                            excerpt_hints_to_persist.insert(*cached_inlay_id, cached_hint.kind);
                            false
                        } else {
                            true
                        }
                    }
                    Err(_) => true,
                }
            }
            None => true,
        };
        if missing_from_cache {
            add_to_cache.push(new_hint);
        }
    }

    let mut remove_from_visible = Vec::new();
    let mut remove_from_cache = HashSet::default();
    if matches!(
        query.invalidate,
        InvalidationStrategy::All | InvalidationStrategy::OnConflict
    ) {
        remove_from_visible.extend(
            visible_hints
                .iter()
                .filter(|hint| hint.position.excerpt_id == query.excerpt_id)
                .filter(|hint| query.contains_position(hint.position.text_anchor, buffer_snapshot))
                .map(|inlay_hint| inlay_hint.id)
                .filter(|hint_id| !excerpt_hints_to_persist.contains_key(hint_id)),
        );

        if let Some(cached_excerpt_hints) = &cached_excerpt_hints {
            let cached_excerpt_hints = cached_excerpt_hints.read();
            remove_from_cache.extend(
                cached_excerpt_hints
                    .hints
                    .iter()
                    .filter(|(cached_inlay_id, _)| {
                        !excerpt_hints_to_persist.contains_key(cached_inlay_id)
                    })
                    .map(|(cached_inlay_id, _)| *cached_inlay_id),
            );
        }
    }

    if remove_from_visible.is_empty() && remove_from_cache.is_empty() && add_to_cache.is_empty() {
        None
    } else {
        Some(ExcerptHintsUpdate {
            cache_version: query.cache_version,
            excerpt_id: query.excerpt_id,
            remove_from_visible,
            remove_from_cache,
            add_to_cache,
        })
    }
}

fn allowed_hint_types(
    inlay_hint_settings: editor_settings::InlayHints,
) -> HashSet<Option<InlayHintKind>> {
    let mut new_allowed_hint_types = HashSet::default();
    if inlay_hint_settings.show_type_hints {
        new_allowed_hint_types.insert(Some(InlayHintKind::Type));
    }
    if inlay_hint_settings.show_parameter_hints {
        new_allowed_hint_types.insert(Some(InlayHintKind::Parameter));
    }
    if inlay_hint_settings.show_other_hints {
        new_allowed_hint_types.insert(None);
    }
    new_allowed_hint_types
}

fn hints_fetch_task(
    query: ExcerptQuery,
    cx: &mut ViewContext<'_, '_, Editor>,
) -> Task<anyhow::Result<Option<Vec<InlayHint>>>> {
    cx.spawn(|editor, mut cx| async move {
        let task = editor
            .update(&mut cx, |editor, cx| {
                editor
                    .buffer()
                    .read(cx)
                    .buffer(query.buffer_id)
                    .and_then(|buffer| {
                        let project = editor.project.as_ref()?;
                        Some(project.update(cx, |project, cx| {
                            project.inlay_hints(
                                buffer,
                                query.excerpt_range_start..query.excerpt_range_end,
                                cx,
                            )
                        }))
                    })
            })
            .ok()
            .flatten();
        Ok(match task {
            Some(task) => Some(task.await.context("inlays for buffer task")?),
            None => None,
        })
    })
}

pub fn visible_inlay_hints<'a, 'b: 'a, 'c, 'd: 'a>(
    editor: &'a Editor,
    cx: &'b ViewContext<'c, 'd, Editor>,
) -> impl Iterator<Item = &'b Inlay> + 'a {
    editor
        .display_map
        .read(cx)
        .current_inlays()
        .filter(|inlay| Some(inlay.id) != editor.copilot_state.suggestion.as_ref().map(|h| h.id))
}
