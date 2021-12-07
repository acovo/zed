pub use crate::{
    highlight_map::{HighlightId, HighlightMap},
    proto, BracketPair, Grammar, Language, LanguageConfig, LanguageRegistry, LanguageServerConfig,
    PLAIN_TEXT,
};
use anyhow::{anyhow, Result};
use clock::ReplicaId;
use futures::FutureExt as _;
use gpui::{fonts::HighlightStyle, AppContext, Entity, ModelContext, MutableAppContext, Task};
use lazy_static::lazy_static;
use lsp::LanguageServer;
use parking_lot::Mutex;
use postage::{prelude::Stream, sink::Sink, watch};
use similar::{ChangeTag, TextDiff};
use smol::future::yield_now;
use std::{
    any::Any,
    cell::RefCell,
    cmp,
    collections::{BTreeMap, HashMap, HashSet},
    ffi::OsString,
    future::Future,
    iter::{Iterator, Peekable},
    ops::{Deref, DerefMut, Range},
    path::{Path, PathBuf},
    str,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    vec,
};
pub use text::{Buffer as TextBuffer, Operation as _, *};
use theme::SyntaxTheme;
use tree_sitter::{InputEdit, Parser, QueryCursor, Tree};
use util::{post_inc, TryFutureExt as _};

#[cfg(any(test, feature = "test-support"))]
pub use tree_sitter_rust;

pub use lsp::DiagnosticSeverity;

thread_local! {
    static PARSER: RefCell<Parser> = RefCell::new(Parser::new());
}

lazy_static! {
    static ref QUERY_CURSORS: Mutex<Vec<QueryCursor>> = Default::default();
}

// TODO - Make this configurable
const INDENT_SIZE: u32 = 4;

pub struct Buffer {
    text: TextBuffer,
    file: Option<Box<dyn File>>,
    saved_version: clock::Global,
    saved_mtime: SystemTime,
    language: Option<Arc<Language>>,
    autoindent_requests: Vec<Arc<AutoindentRequest>>,
    pending_autoindent: Option<Task<()>>,
    sync_parse_timeout: Duration,
    syntax_tree: Mutex<Option<SyntaxTree>>,
    parsing_in_background: bool,
    parse_count: usize,
    diagnostics: AnchorRangeMultimap<Diagnostic>,
    diagnostics_update_count: usize,
    language_server: Option<LanguageServerState>,
    #[cfg(test)]
    pub(crate) operations: Vec<Operation>,
}

pub struct Snapshot {
    text: text::Snapshot,
    tree: Option<Tree>,
    diagnostics: AnchorRangeMultimap<Diagnostic>,
    diagnostics_update_count: usize,
    is_parsing: bool,
    language: Option<Arc<Language>>,
    parse_count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub severity: DiagnosticSeverity,
    pub message: String,
    pub group_id: usize,
    pub is_primary: bool,
}

struct LanguageServerState {
    server: Arc<LanguageServer>,
    latest_snapshot: watch::Sender<Option<LanguageServerSnapshot>>,
    pending_snapshots: BTreeMap<usize, LanguageServerSnapshot>,
    next_version: usize,
    _maintain_server: Task<Option<()>>,
}

#[derive(Clone)]
struct LanguageServerSnapshot {
    buffer_snapshot: text::Snapshot,
    version: usize,
    path: Arc<Path>,
}

#[derive(Clone)]
pub enum Operation {
    Buffer(text::Operation),
    UpdateDiagnostics(AnchorRangeMultimap<Diagnostic>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Event {
    Edited,
    Dirtied,
    Saved,
    FileHandleChanged,
    Reloaded,
    Reparsed,
    DiagnosticsUpdated,
    Closed,
}

pub trait File {
    fn worktree_id(&self) -> usize;

    fn entry_id(&self) -> Option<usize>;

    fn mtime(&self) -> SystemTime;

    /// Returns the path of this file relative to the worktree's root directory.
    fn path(&self) -> &Arc<Path>;

    /// Returns the absolute path of this file.
    fn abs_path(&self) -> Option<PathBuf>;

    /// Returns the path of this file relative to the worktree's parent directory (this means it
    /// includes the name of the worktree's root folder).
    fn full_path(&self) -> PathBuf;

    /// Returns the last component of this handle's absolute path. If this handle refers to the root
    /// of its worktree, then this method will return the name of the worktree itself.
    fn file_name(&self) -> Option<OsString>;

    fn is_deleted(&self) -> bool;

    fn save(
        &self,
        buffer_id: u64,
        text: Rope,
        version: clock::Global,
        cx: &mut MutableAppContext,
    ) -> Task<Result<(clock::Global, SystemTime)>>;

    fn load_local(&self, cx: &AppContext) -> Option<Task<Result<String>>>;

    fn buffer_updated(&self, buffer_id: u64, operation: Operation, cx: &mut MutableAppContext);

    fn buffer_removed(&self, buffer_id: u64, cx: &mut MutableAppContext);

    fn boxed_clone(&self) -> Box<dyn File>;

    fn as_any(&self) -> &dyn Any;
}

struct QueryCursorHandle(Option<QueryCursor>);

#[derive(Clone)]
struct SyntaxTree {
    tree: Tree,
    version: clock::Global,
}

#[derive(Clone)]
struct AutoindentRequest {
    selection_set_ids: HashSet<SelectionSetId>,
    before_edit: Snapshot,
    edited: AnchorSet,
    inserted: Option<AnchorRangeSet>,
}

#[derive(Debug)]
struct IndentSuggestion {
    basis_row: u32,
    indent: bool,
}

struct TextProvider<'a>(&'a Rope);

struct Highlights<'a> {
    captures: tree_sitter::QueryCaptures<'a, 'a, TextProvider<'a>>,
    next_capture: Option<(tree_sitter::QueryMatch<'a, 'a>, usize)>,
    stack: Vec<(usize, HighlightId)>,
    highlight_map: HighlightMap,
    theme: &'a SyntaxTheme,
    _query_cursor: QueryCursorHandle,
}

pub struct Chunks<'a> {
    range: Range<usize>,
    chunks: rope::Chunks<'a>,
    diagnostic_endpoints: Peekable<vec::IntoIter<DiagnosticEndpoint>>,
    error_depth: usize,
    warning_depth: usize,
    information_depth: usize,
    hint_depth: usize,
    highlights: Option<Highlights<'a>>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Chunk<'a> {
    pub text: &'a str,
    pub highlight_style: Option<HighlightStyle>,
    pub diagnostic: Option<DiagnosticSeverity>,
}

pub(crate) struct Diff {
    base_version: clock::Global,
    new_text: Arc<str>,
    changes: Vec<(ChangeTag, usize)>,
}

#[derive(Clone, Copy)]
struct DiagnosticEndpoint {
    offset: usize,
    is_start: bool,
    severity: DiagnosticSeverity,
}

impl Buffer {
    pub fn new<T: Into<Arc<str>>>(
        replica_id: ReplicaId,
        base_text: T,
        cx: &mut ModelContext<Self>,
    ) -> Self {
        Self::build(
            TextBuffer::new(
                replica_id,
                cx.model_id() as u64,
                History::new(base_text.into()),
            ),
            None,
        )
    }

    pub fn from_file<T: Into<Arc<str>>>(
        replica_id: ReplicaId,
        base_text: T,
        file: Box<dyn File>,
        cx: &mut ModelContext<Self>,
    ) -> Self {
        Self::build(
            TextBuffer::new(
                replica_id,
                cx.model_id() as u64,
                History::new(base_text.into()),
            ),
            Some(file),
        )
    }

    pub fn from_proto(
        replica_id: ReplicaId,
        message: proto::Buffer,
        file: Option<Box<dyn File>>,
        cx: &mut ModelContext<Self>,
    ) -> Result<Self> {
        let mut buffer =
            text::Buffer::new(replica_id, message.id, History::new(message.content.into()));
        let ops = message
            .history
            .into_iter()
            .map(|op| text::Operation::Edit(proto::deserialize_edit_operation(op)));
        buffer.apply_ops(ops)?;
        for set in message.selections {
            let set = proto::deserialize_selection_set(set);
            buffer.add_raw_selection_set(set.id, set);
        }
        let mut this = Self::build(buffer, file);
        if let Some(diagnostics) = message.diagnostics {
            this.apply_diagnostic_update(proto::deserialize_diagnostics(diagnostics), cx);
        }
        Ok(this)
    }

    pub fn to_proto(&self) -> proto::Buffer {
        proto::Buffer {
            id: self.remote_id(),
            content: self.text.base_text().to_string(),
            history: self
                .text
                .history()
                .map(proto::serialize_edit_operation)
                .collect(),
            selections: self
                .selection_sets()
                .map(|(_, set)| proto::serialize_selection_set(set))
                .collect(),
            diagnostics: Some(proto::serialize_diagnostics(&self.diagnostics)),
        }
    }

    pub fn with_language(
        mut self,
        language: Option<Arc<Language>>,
        language_server: Option<Arc<LanguageServer>>,
        cx: &mut ModelContext<Self>,
    ) -> Self {
        self.set_language(language, language_server, cx);
        self
    }

    fn build(buffer: TextBuffer, file: Option<Box<dyn File>>) -> Self {
        let saved_mtime;
        if let Some(file) = file.as_ref() {
            saved_mtime = file.mtime();
        } else {
            saved_mtime = UNIX_EPOCH;
        }

        Self {
            saved_mtime,
            saved_version: buffer.version(),
            text: buffer,
            file,
            syntax_tree: Mutex::new(None),
            parsing_in_background: false,
            parse_count: 0,
            sync_parse_timeout: Duration::from_millis(1),
            autoindent_requests: Default::default(),
            pending_autoindent: Default::default(),
            language: None,
            diagnostics: Default::default(),
            diagnostics_update_count: 0,
            language_server: None,
            #[cfg(test)]
            operations: Default::default(),
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            text: self.text.snapshot(),
            tree: self.syntax_tree(),
            diagnostics: self.diagnostics.clone(),
            diagnostics_update_count: self.diagnostics_update_count,
            is_parsing: self.parsing_in_background,
            language: self.language.clone(),
            parse_count: self.parse_count,
        }
    }

    pub fn file(&self) -> Option<&dyn File> {
        self.file.as_deref()
    }

    pub fn save(
        &mut self,
        cx: &mut ModelContext<Self>,
    ) -> Result<Task<Result<(clock::Global, SystemTime)>>> {
        let file = self
            .file
            .as_ref()
            .ok_or_else(|| anyhow!("buffer has no file"))?;
        let text = self.as_rope().clone();
        let version = self.version();
        let save = file.save(self.remote_id(), text, version, cx.as_mut());
        Ok(cx.spawn(|this, mut cx| async move {
            let (version, mtime) = save.await?;
            this.update(&mut cx, |this, cx| {
                this.did_save(version.clone(), mtime, None, cx);
            });
            Ok((version, mtime))
        }))
    }

    pub fn set_language(
        &mut self,
        language: Option<Arc<Language>>,
        language_server: Option<Arc<lsp::LanguageServer>>,
        cx: &mut ModelContext<Self>,
    ) {
        self.language = language;
        self.language_server = if let Some(server) = language_server {
            let (latest_snapshot_tx, mut latest_snapshot_rx) = watch::channel();
            Some(LanguageServerState {
                latest_snapshot: latest_snapshot_tx,
                pending_snapshots: Default::default(),
                next_version: 0,
                server: server.clone(),
                _maintain_server: cx.background().spawn(
                    async move {
                        let mut prev_snapshot: Option<LanguageServerSnapshot> = None;
                        while let Some(snapshot) = latest_snapshot_rx.recv().await {
                            if let Some(snapshot) = snapshot {
                                let uri = lsp::Url::from_file_path(&snapshot.path).unwrap();
                                if let Some(prev_snapshot) = prev_snapshot {
                                    let changes = lsp::DidChangeTextDocumentParams {
                                        text_document: lsp::VersionedTextDocumentIdentifier::new(
                                            uri,
                                            snapshot.version as i32,
                                        ),
                                        content_changes: snapshot
                                            .buffer_snapshot
                                            .edits_since::<(PointUtf16, usize)>(
                                                prev_snapshot.buffer_snapshot.version(),
                                            )
                                            .map(|edit| {
                                                let edit_start = edit.new.start.0;
                                                let edit_end = edit_start
                                                    + (edit.old.end.0 - edit.old.start.0);
                                                let new_text = snapshot
                                                    .buffer_snapshot
                                                    .text_for_range(
                                                        edit.new.start.1..edit.new.end.1,
                                                    )
                                                    .collect();
                                                lsp::TextDocumentContentChangeEvent {
                                                    range: Some(lsp::Range::new(
                                                        lsp::Position::new(
                                                            edit_start.row,
                                                            edit_start.column,
                                                        ),
                                                        lsp::Position::new(
                                                            edit_end.row,
                                                            edit_end.column,
                                                        ),
                                                    )),
                                                    range_length: None,
                                                    text: new_text,
                                                }
                                            })
                                            .collect(),
                                    };
                                    server
                                        .notify::<lsp::notification::DidChangeTextDocument>(changes)
                                        .await?;
                                } else {
                                    server
                                        .notify::<lsp::notification::DidOpenTextDocument>(
                                            lsp::DidOpenTextDocumentParams {
                                                text_document: lsp::TextDocumentItem::new(
                                                    uri,
                                                    Default::default(),
                                                    snapshot.version as i32,
                                                    snapshot.buffer_snapshot.text().to_string(),
                                                ),
                                            },
                                        )
                                        .await?;
                                }

                                prev_snapshot = Some(snapshot);
                            }
                        }
                        Ok(())
                    }
                    .log_err(),
                ),
            })
        } else {
            None
        };

        self.reparse(cx);
        self.update_language_server();
    }

    pub fn did_save(
        &mut self,
        version: clock::Global,
        mtime: SystemTime,
        new_file: Option<Box<dyn File>>,
        cx: &mut ModelContext<Self>,
    ) {
        self.saved_mtime = mtime;
        self.saved_version = version;
        if let Some(new_file) = new_file {
            self.file = Some(new_file);
        }
        if let Some(state) = &self.language_server {
            cx.background()
                .spawn(
                    state
                        .server
                        .notify::<lsp::notification::DidSaveTextDocument>(
                            lsp::DidSaveTextDocumentParams {
                                text_document: lsp::TextDocumentIdentifier {
                                    uri: lsp::Url::from_file_path(
                                        self.file.as_ref().unwrap().abs_path().unwrap(),
                                    )
                                    .unwrap(),
                                },
                                text: None,
                            },
                        ),
                )
                .detach()
        }
        cx.emit(Event::Saved);
    }

    pub fn file_updated(
        &mut self,
        new_file: Box<dyn File>,
        cx: &mut ModelContext<Self>,
    ) -> Option<Task<()>> {
        let old_file = self.file.as_ref()?;
        let mut file_changed = false;
        let mut task = None;

        if new_file.path() != old_file.path() {
            file_changed = true;
        }

        if new_file.is_deleted() {
            if !old_file.is_deleted() {
                file_changed = true;
                if !self.is_dirty() {
                    cx.emit(Event::Dirtied);
                }
            }
        } else {
            let new_mtime = new_file.mtime();
            if new_mtime != old_file.mtime() {
                file_changed = true;

                if !self.is_dirty() {
                    task = Some(cx.spawn(|this, mut cx| {
                        async move {
                            let new_text = this.read_with(&cx, |this, cx| {
                                this.file.as_ref().and_then(|file| file.load_local(cx))
                            });
                            if let Some(new_text) = new_text {
                                let new_text = new_text.await?;
                                let diff = this
                                    .read_with(&cx, |this, cx| this.diff(new_text.into(), cx))
                                    .await;
                                this.update(&mut cx, |this, cx| {
                                    if this.apply_diff(diff, cx) {
                                        this.saved_version = this.version();
                                        this.saved_mtime = new_mtime;
                                        cx.emit(Event::Reloaded);
                                    }
                                });
                            }
                            Ok(())
                        }
                        .log_err()
                        .map(drop)
                    }));
                }
            }
        }

        if file_changed {
            cx.emit(Event::FileHandleChanged);
        }
        self.file = Some(new_file);
        task
    }

    pub fn close(&mut self, cx: &mut ModelContext<Self>) {
        cx.emit(Event::Closed);
    }

    pub fn language(&self) -> Option<&Arc<Language>> {
        self.language.as_ref()
    }

    pub fn parse_count(&self) -> usize {
        self.parse_count
    }

    pub(crate) fn syntax_tree(&self) -> Option<Tree> {
        if let Some(syntax_tree) = self.syntax_tree.lock().as_mut() {
            self.interpolate_tree(syntax_tree);
            Some(syntax_tree.tree.clone())
        } else {
            None
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn is_parsing(&self) -> bool {
        self.parsing_in_background
    }

    #[cfg(test)]
    pub fn set_sync_parse_timeout(&mut self, timeout: Duration) {
        self.sync_parse_timeout = timeout;
    }

    fn reparse(&mut self, cx: &mut ModelContext<Self>) -> bool {
        if self.parsing_in_background {
            return false;
        }

        if let Some(grammar) = self.grammar().cloned() {
            let old_tree = self.syntax_tree();
            let text = self.as_rope().clone();
            let parsed_version = self.version();
            let parse_task = cx.background().spawn({
                let grammar = grammar.clone();
                async move { Self::parse_text(&text, old_tree, &grammar) }
            });

            match cx
                .background()
                .block_with_timeout(self.sync_parse_timeout, parse_task)
            {
                Ok(new_tree) => {
                    self.did_finish_parsing(new_tree, parsed_version, cx);
                    return true;
                }
                Err(parse_task) => {
                    self.parsing_in_background = true;
                    cx.spawn(move |this, mut cx| async move {
                        let new_tree = parse_task.await;
                        this.update(&mut cx, move |this, cx| {
                            let grammar_changed = this
                                .grammar()
                                .map_or(true, |curr_grammar| !Arc::ptr_eq(&grammar, curr_grammar));
                            let parse_again = this.version.gt(&parsed_version) || grammar_changed;
                            this.parsing_in_background = false;
                            this.did_finish_parsing(new_tree, parsed_version, cx);

                            if parse_again && this.reparse(cx) {
                                return;
                            }
                        });
                    })
                    .detach();
                }
            }
        }
        false
    }

    fn parse_text(text: &Rope, old_tree: Option<Tree>, grammar: &Grammar) -> Tree {
        PARSER.with(|parser| {
            let mut parser = parser.borrow_mut();
            parser
                .set_language(grammar.ts_language)
                .expect("incompatible grammar");
            let mut chunks = text.chunks_in_range(0..text.len());
            let tree = parser
                .parse_with(
                    &mut move |offset, _| {
                        chunks.seek(offset);
                        chunks.next().unwrap_or("").as_bytes()
                    },
                    old_tree.as_ref(),
                )
                .unwrap();
            tree
        })
    }

    fn interpolate_tree(&self, tree: &mut SyntaxTree) {
        for edit in self.edits_since::<(usize, Point)>(&tree.version) {
            let (bytes, lines) = edit.flatten();
            tree.tree.edit(&InputEdit {
                start_byte: bytes.new.start,
                old_end_byte: bytes.new.start + bytes.old.len(),
                new_end_byte: bytes.new.end,
                start_position: lines.new.start.to_ts_point(),
                old_end_position: (lines.new.start + (lines.old.end - lines.old.start))
                    .to_ts_point(),
                new_end_position: lines.new.end.to_ts_point(),
            });
        }
        tree.version = self.version();
    }

    fn did_finish_parsing(
        &mut self,
        tree: Tree,
        version: clock::Global,
        cx: &mut ModelContext<Self>,
    ) {
        self.parse_count += 1;
        *self.syntax_tree.lock() = Some(SyntaxTree { tree, version });
        self.request_autoindent(cx);
        cx.emit(Event::Reparsed);
        cx.notify();
    }

    pub fn update_diagnostics(
        &mut self,
        version: Option<i32>,
        mut diagnostics: Vec<lsp::Diagnostic>,
        cx: &mut ModelContext<Self>,
    ) -> Result<Operation> {
        let version = version.map(|version| version as usize);
        let content = if let Some(version) = version {
            let language_server = self.language_server.as_mut().unwrap();
            let snapshot = language_server
                .pending_snapshots
                .get(&version)
                .ok_or_else(|| anyhow!("missing snapshot"))?;
            &snapshot.buffer_snapshot
        } else {
            self.deref()
        };
        let abs_path = self.file.as_ref().and_then(|f| f.abs_path());

        let empty_set = HashSet::new();
        let disk_based_sources = self
            .language
            .as_ref()
            .and_then(|language| language.disk_based_diagnostic_sources())
            .unwrap_or(&empty_set);

        diagnostics.sort_unstable_by_key(|d| (d.range.start, d.range.end));
        self.diagnostics = {
            let mut edits_since_save = content
                .edits_since::<PointUtf16>(&self.saved_version)
                .peekable();
            let mut last_edit_old_end = PointUtf16::zero();
            let mut last_edit_new_end = PointUtf16::zero();
            let mut group_ids_by_diagnostic_range = HashMap::new();
            let mut diagnostics_by_group_id = HashMap::new();
            let mut next_group_id = 0;
            'outer: for diagnostic in &diagnostics {
                let mut start = diagnostic.range.start.to_point_utf16();
                let mut end = diagnostic.range.end.to_point_utf16();
                let source = diagnostic.source.as_ref();
                let code = diagnostic.code.as_ref();
                let group_id = diagnostic_ranges(&diagnostic, abs_path.as_deref())
                    .find_map(|range| group_ids_by_diagnostic_range.get(&(source, code, range)))
                    .copied()
                    .unwrap_or_else(|| {
                        let group_id = post_inc(&mut next_group_id);
                        for range in diagnostic_ranges(&diagnostic, abs_path.as_deref()) {
                            group_ids_by_diagnostic_range.insert((source, code, range), group_id);
                        }
                        group_id
                    });

                if diagnostic
                    .source
                    .as_ref()
                    .map_or(false, |source| disk_based_sources.contains(source))
                {
                    while let Some(edit) = edits_since_save.peek() {
                        if edit.old.end <= start {
                            last_edit_old_end = edit.old.end;
                            last_edit_new_end = edit.new.end;
                            edits_since_save.next();
                        } else if edit.old.start <= end && edit.old.end >= start {
                            continue 'outer;
                        } else {
                            break;
                        }
                    }

                    start = last_edit_new_end + (start - last_edit_old_end);
                    end = last_edit_new_end + (end - last_edit_old_end);
                }

                let mut range = content.clip_point_utf16(start, Bias::Left)
                    ..content.clip_point_utf16(end, Bias::Right);
                if range.start == range.end {
                    range.end.column += 1;
                    range.end = content.clip_point_utf16(range.end, Bias::Right);
                    if range.start == range.end && range.end.column > 0 {
                        range.start.column -= 1;
                        range.start = content.clip_point_utf16(range.start, Bias::Left);
                    }
                }

                diagnostics_by_group_id
                    .entry(group_id)
                    .or_insert(Vec::new())
                    .push((
                        range,
                        Diagnostic {
                            severity: diagnostic.severity.unwrap_or(DiagnosticSeverity::ERROR),
                            message: diagnostic.message.clone(),
                            group_id,
                            is_primary: false,
                        },
                    ));
            }

            content.anchor_range_multimap(
                Bias::Left,
                Bias::Right,
                diagnostics_by_group_id
                    .into_values()
                    .flat_map(|mut diagnostics| {
                        let primary_diagnostic =
                            diagnostics.iter_mut().min_by_key(|d| d.1.severity).unwrap();
                        primary_diagnostic.1.is_primary = true;
                        diagnostics
                    }),
            )
        };

        if let Some(version) = version {
            let language_server = self.language_server.as_mut().unwrap();
            let versions_to_delete = language_server
                .pending_snapshots
                .range(..version)
                .map(|(v, _)| *v)
                .collect::<Vec<_>>();
            for version in versions_to_delete {
                language_server.pending_snapshots.remove(&version);
            }
        }

        self.diagnostics_update_count += 1;
        cx.notify();
        cx.emit(Event::DiagnosticsUpdated);
        Ok(Operation::UpdateDiagnostics(self.diagnostics.clone()))
    }

    pub fn diagnostics_in_range<'a, T, O>(
        &'a self,
        search_range: Range<T>,
    ) -> impl Iterator<Item = (Range<O>, &Diagnostic)> + 'a
    where
        T: 'a + ToOffset,
        O: 'a + FromAnchor,
    {
        self.diagnostics
            .intersecting_ranges(search_range, self, true)
            .map(move |(_, range, diagnostic)| (range, diagnostic))
    }

    pub fn diagnostic_group<'a, O>(
        &'a self,
        group_id: usize,
    ) -> impl Iterator<Item = (Range<O>, &Diagnostic)> + 'a
    where
        O: 'a + FromAnchor,
    {
        self.diagnostics
            .filter(self, move |diagnostic| diagnostic.group_id == group_id)
            .map(move |(_, range, diagnostic)| (range, diagnostic))
    }

    pub fn diagnostics_update_count(&self) -> usize {
        self.diagnostics_update_count
    }

    fn request_autoindent(&mut self, cx: &mut ModelContext<Self>) {
        if let Some(indent_columns) = self.compute_autoindents() {
            let indent_columns = cx.background().spawn(indent_columns);
            match cx
                .background()
                .block_with_timeout(Duration::from_micros(500), indent_columns)
            {
                Ok(indent_columns) => self.apply_autoindents(indent_columns, cx),
                Err(indent_columns) => {
                    self.pending_autoindent = Some(cx.spawn(|this, mut cx| async move {
                        let indent_columns = indent_columns.await;
                        this.update(&mut cx, |this, cx| {
                            this.apply_autoindents(indent_columns, cx);
                        });
                    }));
                }
            }
        }
    }

    fn compute_autoindents(&self) -> Option<impl Future<Output = BTreeMap<u32, u32>>> {
        let max_rows_between_yields = 100;
        let snapshot = self.snapshot();
        if snapshot.language.is_none()
            || snapshot.tree.is_none()
            || self.autoindent_requests.is_empty()
        {
            return None;
        }

        let autoindent_requests = self.autoindent_requests.clone();
        Some(async move {
            let mut indent_columns = BTreeMap::new();
            for request in autoindent_requests {
                let old_to_new_rows = request
                    .edited
                    .iter::<Point>(&request.before_edit)
                    .map(|point| point.row)
                    .zip(
                        request
                            .edited
                            .iter::<Point>(&snapshot)
                            .map(|point| point.row),
                    )
                    .collect::<BTreeMap<u32, u32>>();

                let mut old_suggestions = HashMap::<u32, u32>::default();
                let old_edited_ranges =
                    contiguous_ranges(old_to_new_rows.keys().copied(), max_rows_between_yields);
                for old_edited_range in old_edited_ranges {
                    let suggestions = request
                        .before_edit
                        .suggest_autoindents(old_edited_range.clone())
                        .into_iter()
                        .flatten();
                    for (old_row, suggestion) in old_edited_range.zip(suggestions) {
                        let indentation_basis = old_to_new_rows
                            .get(&suggestion.basis_row)
                            .and_then(|from_row| old_suggestions.get(from_row).copied())
                            .unwrap_or_else(|| {
                                request
                                    .before_edit
                                    .indent_column_for_line(suggestion.basis_row)
                            });
                        let delta = if suggestion.indent { INDENT_SIZE } else { 0 };
                        old_suggestions.insert(
                            *old_to_new_rows.get(&old_row).unwrap(),
                            indentation_basis + delta,
                        );
                    }
                    yield_now().await;
                }

                // At this point, old_suggestions contains the suggested indentation for all edited lines with respect to the state of the
                // buffer before the edit, but keyed by the row for these lines after the edits were applied.
                let new_edited_row_ranges =
                    contiguous_ranges(old_to_new_rows.values().copied(), max_rows_between_yields);
                for new_edited_row_range in new_edited_row_ranges {
                    let suggestions = snapshot
                        .suggest_autoindents(new_edited_row_range.clone())
                        .into_iter()
                        .flatten();
                    for (new_row, suggestion) in new_edited_row_range.zip(suggestions) {
                        let delta = if suggestion.indent { INDENT_SIZE } else { 0 };
                        let new_indentation = indent_columns
                            .get(&suggestion.basis_row)
                            .copied()
                            .unwrap_or_else(|| {
                                snapshot.indent_column_for_line(suggestion.basis_row)
                            })
                            + delta;
                        if old_suggestions
                            .get(&new_row)
                            .map_or(true, |old_indentation| new_indentation != *old_indentation)
                        {
                            indent_columns.insert(new_row, new_indentation);
                        }
                    }
                    yield_now().await;
                }

                if let Some(inserted) = request.inserted.as_ref() {
                    let inserted_row_ranges = contiguous_ranges(
                        inserted
                            .ranges::<Point>(&snapshot)
                            .flat_map(|range| range.start.row..range.end.row + 1),
                        max_rows_between_yields,
                    );
                    for inserted_row_range in inserted_row_ranges {
                        let suggestions = snapshot
                            .suggest_autoindents(inserted_row_range.clone())
                            .into_iter()
                            .flatten();
                        for (row, suggestion) in inserted_row_range.zip(suggestions) {
                            let delta = if suggestion.indent { INDENT_SIZE } else { 0 };
                            let new_indentation = indent_columns
                                .get(&suggestion.basis_row)
                                .copied()
                                .unwrap_or_else(|| {
                                    snapshot.indent_column_for_line(suggestion.basis_row)
                                })
                                + delta;
                            indent_columns.insert(row, new_indentation);
                        }
                        yield_now().await;
                    }
                }
            }
            indent_columns
        })
    }

    fn apply_autoindents(
        &mut self,
        indent_columns: BTreeMap<u32, u32>,
        cx: &mut ModelContext<Self>,
    ) {
        let selection_set_ids = self
            .autoindent_requests
            .drain(..)
            .flat_map(|req| req.selection_set_ids.clone())
            .collect::<HashSet<_>>();

        self.start_transaction(selection_set_ids.iter().copied())
            .unwrap();
        for (row, indent_column) in &indent_columns {
            self.set_indent_column_for_line(*row, *indent_column, cx);
        }

        for selection_set_id in &selection_set_ids {
            if let Ok(set) = self.selection_set(*selection_set_id) {
                let new_selections = set
                    .selections::<Point>(&*self)
                    .map(|selection| {
                        if selection.start.column == 0 {
                            let delta = Point::new(
                                0,
                                indent_columns
                                    .get(&selection.start.row)
                                    .copied()
                                    .unwrap_or(0),
                            );
                            if delta.column > 0 {
                                return Selection {
                                    id: selection.id,
                                    goal: selection.goal,
                                    reversed: selection.reversed,
                                    start: selection.start + delta,
                                    end: selection.end + delta,
                                };
                            }
                        }
                        selection
                    })
                    .collect::<Vec<_>>();
                self.update_selection_set(*selection_set_id, &new_selections, cx)
                    .unwrap();
            }
        }

        self.end_transaction(selection_set_ids.iter().copied(), cx)
            .unwrap();
    }

    fn set_indent_column_for_line(&mut self, row: u32, column: u32, cx: &mut ModelContext<Self>) {
        let current_column = self.indent_column_for_line(row);
        if column > current_column {
            let offset = Point::new(row, 0).to_offset(&*self);
            self.edit(
                [offset..offset],
                " ".repeat((column - current_column) as usize),
                cx,
            );
        } else if column < current_column {
            self.edit(
                [Point::new(row, 0)..Point::new(row, current_column - column)],
                "",
                cx,
            );
        }
    }

    pub fn range_for_syntax_ancestor<T: ToOffset>(&self, range: Range<T>) -> Option<Range<usize>> {
        if let Some(tree) = self.syntax_tree() {
            let root = tree.root_node();
            let range = range.start.to_offset(self)..range.end.to_offset(self);
            let mut node = root.descendant_for_byte_range(range.start, range.end);
            while node.map_or(false, |n| n.byte_range() == range) {
                node = node.unwrap().parent();
            }
            node.map(|n| n.byte_range())
        } else {
            None
        }
    }

    pub fn enclosing_bracket_ranges<T: ToOffset>(
        &self,
        range: Range<T>,
    ) -> Option<(Range<usize>, Range<usize>)> {
        let (grammar, tree) = self.grammar().zip(self.syntax_tree())?;
        let open_capture_ix = grammar.brackets_query.capture_index_for_name("open")?;
        let close_capture_ix = grammar.brackets_query.capture_index_for_name("close")?;

        // Find bracket pairs that *inclusively* contain the given range.
        let range = range.start.to_offset(self).saturating_sub(1)..range.end.to_offset(self) + 1;
        let mut cursor = QueryCursorHandle::new();
        let matches = cursor.set_byte_range(range).matches(
            &grammar.brackets_query,
            tree.root_node(),
            TextProvider(self.as_rope()),
        );

        // Get the ranges of the innermost pair of brackets.
        matches
            .filter_map(|mat| {
                let open = mat.nodes_for_capture_index(open_capture_ix).next()?;
                let close = mat.nodes_for_capture_index(close_capture_ix).next()?;
                Some((open.byte_range(), close.byte_range()))
            })
            .min_by_key(|(open_range, close_range)| close_range.end - open_range.start)
    }

    pub(crate) fn diff(&self, new_text: Arc<str>, cx: &AppContext) -> Task<Diff> {
        // TODO: it would be nice to not allocate here.
        let old_text = self.text();
        let base_version = self.version();
        cx.background().spawn(async move {
            let changes = TextDiff::from_lines(old_text.as_str(), new_text.as_ref())
                .iter_all_changes()
                .map(|c| (c.tag(), c.value().len()))
                .collect::<Vec<_>>();
            Diff {
                base_version,
                new_text,
                changes,
            }
        })
    }

    pub(crate) fn apply_diff(&mut self, diff: Diff, cx: &mut ModelContext<Self>) -> bool {
        if self.version == diff.base_version {
            self.start_transaction(None).unwrap();
            let mut offset = 0;
            for (tag, len) in diff.changes {
                let range = offset..(offset + len);
                match tag {
                    ChangeTag::Equal => offset += len,
                    ChangeTag::Delete => self.edit(Some(range), "", cx),
                    ChangeTag::Insert => {
                        self.edit(Some(offset..offset), &diff.new_text[range], cx);
                        offset += len;
                    }
                }
            }
            self.end_transaction(None, cx).unwrap();
            true
        } else {
            false
        }
    }

    pub fn is_dirty(&self) -> bool {
        !self.saved_version.ge(&self.version)
            || self.file.as_ref().map_or(false, |file| file.is_deleted())
    }

    pub fn has_conflict(&self) -> bool {
        !self.saved_version.ge(&self.version)
            && self
                .file
                .as_ref()
                .map_or(false, |file| file.mtime() > self.saved_mtime)
    }

    pub fn subscribe(&mut self) -> Subscription {
        self.text.subscribe()
    }

    pub fn start_transaction(
        &mut self,
        selection_set_ids: impl IntoIterator<Item = SelectionSetId>,
    ) -> Result<()> {
        self.start_transaction_at(selection_set_ids, Instant::now())
    }

    pub(crate) fn start_transaction_at(
        &mut self,
        selection_set_ids: impl IntoIterator<Item = SelectionSetId>,
        now: Instant,
    ) -> Result<()> {
        self.text.start_transaction_at(selection_set_ids, now)
    }

    pub fn end_transaction(
        &mut self,
        selection_set_ids: impl IntoIterator<Item = SelectionSetId>,
        cx: &mut ModelContext<Self>,
    ) -> Result<()> {
        self.end_transaction_at(selection_set_ids, Instant::now(), cx)
    }

    pub(crate) fn end_transaction_at(
        &mut self,
        selection_set_ids: impl IntoIterator<Item = SelectionSetId>,
        now: Instant,
        cx: &mut ModelContext<Self>,
    ) -> Result<()> {
        if let Some(start_version) = self.text.end_transaction_at(selection_set_ids, now) {
            let was_dirty = start_version != self.saved_version;
            self.did_edit(&start_version, was_dirty, cx);
        }
        Ok(())
    }

    fn update_language_server(&mut self) {
        let language_server = if let Some(language_server) = self.language_server.as_mut() {
            language_server
        } else {
            return;
        };
        let abs_path = self
            .file
            .as_ref()
            .map_or(Path::new("/").to_path_buf(), |file| {
                file.abs_path().unwrap()
            });

        let version = post_inc(&mut language_server.next_version);
        let snapshot = LanguageServerSnapshot {
            buffer_snapshot: self.text.snapshot(),
            version,
            path: Arc::from(abs_path),
        };
        language_server
            .pending_snapshots
            .insert(version, snapshot.clone());
        let _ = language_server
            .latest_snapshot
            .blocking_send(Some(snapshot));
    }

    pub fn edit<I, S, T>(&mut self, ranges_iter: I, new_text: T, cx: &mut ModelContext<Self>)
    where
        I: IntoIterator<Item = Range<S>>,
        S: ToOffset,
        T: Into<String>,
    {
        self.edit_internal(ranges_iter, new_text, false, cx)
    }

    pub fn edit_with_autoindent<I, S, T>(
        &mut self,
        ranges_iter: I,
        new_text: T,
        cx: &mut ModelContext<Self>,
    ) where
        I: IntoIterator<Item = Range<S>>,
        S: ToOffset,
        T: Into<String>,
    {
        self.edit_internal(ranges_iter, new_text, true, cx)
    }

    pub fn edit_internal<I, S, T>(
        &mut self,
        ranges_iter: I,
        new_text: T,
        autoindent: bool,
        cx: &mut ModelContext<Self>,
    ) where
        I: IntoIterator<Item = Range<S>>,
        S: ToOffset,
        T: Into<String>,
    {
        let new_text = new_text.into();

        // Skip invalid ranges and coalesce contiguous ones.
        let mut ranges: Vec<Range<usize>> = Vec::new();
        for range in ranges_iter {
            let range = range.start.to_offset(self)..range.end.to_offset(self);
            if !new_text.is_empty() || !range.is_empty() {
                if let Some(prev_range) = ranges.last_mut() {
                    if prev_range.end >= range.start {
                        prev_range.end = cmp::max(prev_range.end, range.end);
                    } else {
                        ranges.push(range);
                    }
                } else {
                    ranges.push(range);
                }
            }
        }
        if ranges.is_empty() {
            return;
        }

        self.start_transaction(None).unwrap();
        self.pending_autoindent.take();
        let autoindent_request = if autoindent && self.language.is_some() {
            let before_edit = self.snapshot();
            let edited = self.anchor_set(
                Bias::Left,
                ranges.iter().filter_map(|range| {
                    let start = range.start.to_point(self);
                    if new_text.starts_with('\n') && start.column == self.line_len(start.row) {
                        None
                    } else {
                        Some(range.start)
                    }
                }),
            );
            Some((before_edit, edited))
        } else {
            None
        };

        let first_newline_ix = new_text.find('\n');
        let new_text_len = new_text.len();

        let edit = self.text.edit(ranges.iter().cloned(), new_text);

        if let Some((before_edit, edited)) = autoindent_request {
            let mut inserted = None;
            if let Some(first_newline_ix) = first_newline_ix {
                let mut delta = 0isize;
                inserted = Some(self.anchor_range_set(
                    Bias::Left,
                    Bias::Right,
                    ranges.iter().map(|range| {
                        let start = (delta + range.start as isize) as usize + first_newline_ix + 1;
                        let end = (delta + range.start as isize) as usize + new_text_len;
                        delta +=
                            (range.end as isize - range.start as isize) + new_text_len as isize;
                        start..end
                    }),
                ));
            }

            let selection_set_ids = self
                .text
                .peek_undo_stack()
                .unwrap()
                .starting_selection_set_ids()
                .collect();
            self.autoindent_requests.push(Arc::new(AutoindentRequest {
                selection_set_ids,
                before_edit,
                edited,
                inserted,
            }));
        }

        self.end_transaction(None, cx).unwrap();
        self.send_operation(Operation::Buffer(text::Operation::Edit(edit)), cx);
    }

    fn did_edit(
        &mut self,
        old_version: &clock::Global,
        was_dirty: bool,
        cx: &mut ModelContext<Self>,
    ) {
        if self.edits_since::<usize>(old_version).next().is_none() {
            return;
        }

        self.reparse(cx);
        self.update_language_server();

        cx.emit(Event::Edited);
        if !was_dirty {
            cx.emit(Event::Dirtied);
        }
        cx.notify();
    }

    fn grammar(&self) -> Option<&Arc<Grammar>> {
        self.language.as_ref().and_then(|l| l.grammar.as_ref())
    }

    pub fn add_selection_set<T: ToOffset>(
        &mut self,
        selections: &[Selection<T>],
        cx: &mut ModelContext<Self>,
    ) -> SelectionSetId {
        let operation = self.text.add_selection_set(selections);
        if let text::Operation::UpdateSelections { set_id, .. } = &operation {
            let set_id = *set_id;
            cx.notify();
            self.send_operation(Operation::Buffer(operation), cx);
            set_id
        } else {
            unreachable!()
        }
    }

    pub fn update_selection_set<T: ToOffset>(
        &mut self,
        set_id: SelectionSetId,
        selections: &[Selection<T>],
        cx: &mut ModelContext<Self>,
    ) -> Result<()> {
        let operation = self.text.update_selection_set(set_id, selections)?;
        cx.notify();
        self.send_operation(Operation::Buffer(operation), cx);
        Ok(())
    }

    pub fn set_active_selection_set(
        &mut self,
        set_id: Option<SelectionSetId>,
        cx: &mut ModelContext<Self>,
    ) -> Result<()> {
        let operation = self.text.set_active_selection_set(set_id)?;
        self.send_operation(Operation::Buffer(operation), cx);
        Ok(())
    }

    pub fn remove_selection_set(
        &mut self,
        set_id: SelectionSetId,
        cx: &mut ModelContext<Self>,
    ) -> Result<()> {
        let operation = self.text.remove_selection_set(set_id)?;
        cx.notify();
        self.send_operation(Operation::Buffer(operation), cx);
        Ok(())
    }

    pub fn apply_ops<I: IntoIterator<Item = Operation>>(
        &mut self,
        ops: I,
        cx: &mut ModelContext<Self>,
    ) -> Result<()> {
        self.pending_autoindent.take();
        let was_dirty = self.is_dirty();
        let old_version = self.version.clone();
        let buffer_ops = ops
            .into_iter()
            .filter_map(|op| match op {
                Operation::Buffer(op) => Some(op),
                Operation::UpdateDiagnostics(diagnostics) => {
                    self.apply_diagnostic_update(diagnostics, cx);
                    None
                }
            })
            .collect::<Vec<_>>();
        self.text.apply_ops(buffer_ops)?;
        self.did_edit(&old_version, was_dirty, cx);
        // Notify independently of whether the buffer was edited as the operations could include a
        // selection update.
        cx.notify();
        Ok(())
    }

    fn apply_diagnostic_update(
        &mut self,
        diagnostics: AnchorRangeMultimap<Diagnostic>,
        cx: &mut ModelContext<Self>,
    ) {
        self.diagnostics = diagnostics;
        self.diagnostics_update_count += 1;
        cx.notify();
    }

    #[cfg(not(test))]
    pub fn send_operation(&mut self, operation: Operation, cx: &mut ModelContext<Self>) {
        if let Some(file) = &self.file {
            file.buffer_updated(self.remote_id(), operation, cx.as_mut());
        }
    }

    #[cfg(test)]
    pub fn send_operation(&mut self, operation: Operation, _: &mut ModelContext<Self>) {
        self.operations.push(operation);
    }

    pub fn remove_peer(&mut self, replica_id: ReplicaId, cx: &mut ModelContext<Self>) {
        self.text.remove_peer(replica_id);
        cx.notify();
    }

    pub fn undo(&mut self, cx: &mut ModelContext<Self>) {
        let was_dirty = self.is_dirty();
        let old_version = self.version.clone();

        for operation in self.text.undo() {
            self.send_operation(Operation::Buffer(operation), cx);
        }

        self.did_edit(&old_version, was_dirty, cx);
    }

    pub fn redo(&mut self, cx: &mut ModelContext<Self>) {
        let was_dirty = self.is_dirty();
        let old_version = self.version.clone();

        for operation in self.text.redo() {
            self.send_operation(Operation::Buffer(operation), cx);
        }

        self.did_edit(&old_version, was_dirty, cx);
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Buffer {
    pub fn randomly_edit<T>(
        &mut self,
        rng: &mut T,
        old_range_count: usize,
        cx: &mut ModelContext<Self>,
    ) where
        T: rand::Rng,
    {
        self.start_transaction(None).unwrap();
        self.text.randomly_edit(rng, old_range_count);
        self.end_transaction(None, cx).unwrap();
    }

    pub fn randomly_mutate<T>(&mut self, rng: &mut T, cx: &mut ModelContext<Self>)
    where
        T: rand::Rng,
    {
        self.start_transaction(None).unwrap();
        self.text.randomly_mutate(rng);
        self.end_transaction(None, cx).unwrap();
    }
}

impl Entity for Buffer {
    type Event = Event;

    fn release(&mut self, cx: &mut gpui::MutableAppContext) {
        if let Some(file) = self.file.as_ref() {
            file.buffer_removed(self.remote_id(), cx);
        }
    }
}

impl Deref for Buffer {
    type Target = TextBuffer;

    fn deref(&self) -> &Self::Target {
        &self.text
    }
}

impl Snapshot {
    fn suggest_autoindents<'a>(
        &'a self,
        row_range: Range<u32>,
    ) -> Option<impl Iterator<Item = IndentSuggestion> + 'a> {
        let mut query_cursor = QueryCursorHandle::new();
        if let Some((grammar, tree)) = self.grammar().zip(self.tree.as_ref()) {
            let prev_non_blank_row = self.prev_non_blank_row(row_range.start);

            // Get the "indentation ranges" that intersect this row range.
            let indent_capture_ix = grammar.indents_query.capture_index_for_name("indent");
            let end_capture_ix = grammar.indents_query.capture_index_for_name("end");
            query_cursor.set_point_range(
                Point::new(prev_non_blank_row.unwrap_or(row_range.start), 0).to_ts_point()
                    ..Point::new(row_range.end, 0).to_ts_point(),
            );
            let mut indentation_ranges = Vec::<(Range<Point>, &'static str)>::new();
            for mat in query_cursor.matches(
                &grammar.indents_query,
                tree.root_node(),
                TextProvider(self.as_rope()),
            ) {
                let mut node_kind = "";
                let mut start: Option<Point> = None;
                let mut end: Option<Point> = None;
                for capture in mat.captures {
                    if Some(capture.index) == indent_capture_ix {
                        node_kind = capture.node.kind();
                        start.get_or_insert(Point::from_ts_point(capture.node.start_position()));
                        end.get_or_insert(Point::from_ts_point(capture.node.end_position()));
                    } else if Some(capture.index) == end_capture_ix {
                        end = Some(Point::from_ts_point(capture.node.start_position().into()));
                    }
                }

                if let Some((start, end)) = start.zip(end) {
                    if start.row == end.row {
                        continue;
                    }

                    let range = start..end;
                    match indentation_ranges.binary_search_by_key(&range.start, |r| r.0.start) {
                        Err(ix) => indentation_ranges.insert(ix, (range, node_kind)),
                        Ok(ix) => {
                            let prev_range = &mut indentation_ranges[ix];
                            prev_range.0.end = prev_range.0.end.max(range.end);
                        }
                    }
                }
            }

            let mut prev_row = prev_non_blank_row.unwrap_or(0);
            Some(row_range.map(move |row| {
                let row_start = Point::new(row, self.indent_column_for_line(row));

                let mut indent_from_prev_row = false;
                let mut outdent_to_row = u32::MAX;
                for (range, _node_kind) in &indentation_ranges {
                    if range.start.row >= row {
                        break;
                    }

                    if range.start.row == prev_row && range.end > row_start {
                        indent_from_prev_row = true;
                    }
                    if range.end.row >= prev_row && range.end <= row_start {
                        outdent_to_row = outdent_to_row.min(range.start.row);
                    }
                }

                let suggestion = if outdent_to_row == prev_row {
                    IndentSuggestion {
                        basis_row: prev_row,
                        indent: false,
                    }
                } else if indent_from_prev_row {
                    IndentSuggestion {
                        basis_row: prev_row,
                        indent: true,
                    }
                } else if outdent_to_row < prev_row {
                    IndentSuggestion {
                        basis_row: outdent_to_row,
                        indent: false,
                    }
                } else {
                    IndentSuggestion {
                        basis_row: prev_row,
                        indent: false,
                    }
                };

                prev_row = row;
                suggestion
            }))
        } else {
            None
        }
    }

    fn prev_non_blank_row(&self, mut row: u32) -> Option<u32> {
        while row > 0 {
            row -= 1;
            if !self.is_line_blank(row) {
                return Some(row);
            }
        }
        None
    }

    pub fn chunks<'a, T: ToOffset>(
        &'a self,
        range: Range<T>,
        theme: Option<&'a SyntaxTheme>,
    ) -> Chunks<'a> {
        let range = range.start.to_offset(self)..range.end.to_offset(self);

        let mut highlights = None;
        let mut diagnostic_endpoints = Vec::<DiagnosticEndpoint>::new();
        if let Some(theme) = theme {
            for (_, range, diagnostic) in
                self.diagnostics
                    .intersecting_ranges(range.clone(), self, true)
            {
                diagnostic_endpoints.push(DiagnosticEndpoint {
                    offset: range.start,
                    is_start: true,
                    severity: diagnostic.severity,
                });
                diagnostic_endpoints.push(DiagnosticEndpoint {
                    offset: range.end,
                    is_start: false,
                    severity: diagnostic.severity,
                });
            }
            diagnostic_endpoints
                .sort_unstable_by_key(|endpoint| (endpoint.offset, !endpoint.is_start));

            if let Some((grammar, tree)) = self.grammar().zip(self.tree.as_ref()) {
                let mut query_cursor = QueryCursorHandle::new();

                // TODO - add a Tree-sitter API to remove the need for this.
                let cursor = unsafe {
                    std::mem::transmute::<_, &'static mut QueryCursor>(query_cursor.deref_mut())
                };
                let captures = cursor.set_byte_range(range.clone()).captures(
                    &grammar.highlights_query,
                    tree.root_node(),
                    TextProvider(self.text.as_rope()),
                );
                highlights = Some(Highlights {
                    captures,
                    next_capture: None,
                    stack: Default::default(),
                    highlight_map: grammar.highlight_map(),
                    _query_cursor: query_cursor,
                    theme,
                })
            }
        }

        let diagnostic_endpoints = diagnostic_endpoints.into_iter().peekable();
        let chunks = self.text.as_rope().chunks_in_range(range.clone());

        Chunks {
            range,
            chunks,
            diagnostic_endpoints,
            error_depth: 0,
            warning_depth: 0,
            information_depth: 0,
            hint_depth: 0,
            highlights,
        }
    }

    fn grammar(&self) -> Option<&Arc<Grammar>> {
        self.language
            .as_ref()
            .and_then(|language| language.grammar.as_ref())
    }

    pub fn diagnostics_update_count(&self) -> usize {
        self.diagnostics_update_count
    }

    pub fn parse_count(&self) -> usize {
        self.parse_count
    }
}

impl Clone for Snapshot {
    fn clone(&self) -> Self {
        Self {
            text: self.text.clone(),
            tree: self.tree.clone(),
            diagnostics: self.diagnostics.clone(),
            diagnostics_update_count: self.diagnostics_update_count,
            is_parsing: self.is_parsing,
            language: self.language.clone(),
            parse_count: self.parse_count,
        }
    }
}

impl Deref for Snapshot {
    type Target = text::Snapshot;

    fn deref(&self) -> &Self::Target {
        &self.text
    }
}

impl<'a> tree_sitter::TextProvider<'a> for TextProvider<'a> {
    type I = ByteChunks<'a>;

    fn text(&mut self, node: tree_sitter::Node) -> Self::I {
        ByteChunks(self.0.chunks_in_range(node.byte_range()))
    }
}

struct ByteChunks<'a>(rope::Chunks<'a>);

impl<'a> Iterator for ByteChunks<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(str::as_bytes)
    }
}

unsafe impl<'a> Send for Chunks<'a> {}

impl<'a> Chunks<'a> {
    pub fn seek(&mut self, offset: usize) {
        self.range.start = offset;
        self.chunks.seek(self.range.start);
        if let Some(highlights) = self.highlights.as_mut() {
            highlights
                .stack
                .retain(|(end_offset, _)| *end_offset > offset);
            if let Some((mat, capture_ix)) = &highlights.next_capture {
                let capture = mat.captures[*capture_ix as usize];
                if offset >= capture.node.start_byte() {
                    let next_capture_end = capture.node.end_byte();
                    if offset < next_capture_end {
                        highlights.stack.push((
                            next_capture_end,
                            highlights.highlight_map.get(capture.index),
                        ));
                    }
                    highlights.next_capture.take();
                }
            }
            highlights.captures.set_byte_range(self.range.clone());
        }
    }

    pub fn offset(&self) -> usize {
        self.range.start
    }

    fn update_diagnostic_depths(&mut self, endpoint: DiagnosticEndpoint) {
        let depth = match endpoint.severity {
            DiagnosticSeverity::ERROR => &mut self.error_depth,
            DiagnosticSeverity::WARNING => &mut self.warning_depth,
            DiagnosticSeverity::INFORMATION => &mut self.information_depth,
            DiagnosticSeverity::HINT => &mut self.hint_depth,
            _ => return,
        };
        if endpoint.is_start {
            *depth += 1;
        } else {
            *depth -= 1;
        }
    }

    fn current_diagnostic_severity(&mut self) -> Option<DiagnosticSeverity> {
        if self.error_depth > 0 {
            Some(DiagnosticSeverity::ERROR)
        } else if self.warning_depth > 0 {
            Some(DiagnosticSeverity::WARNING)
        } else if self.information_depth > 0 {
            Some(DiagnosticSeverity::INFORMATION)
        } else if self.hint_depth > 0 {
            Some(DiagnosticSeverity::HINT)
        } else {
            None
        }
    }
}

impl<'a> Iterator for Chunks<'a> {
    type Item = Chunk<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut next_capture_start = usize::MAX;
        let mut next_diagnostic_endpoint = usize::MAX;

        if let Some(highlights) = self.highlights.as_mut() {
            while let Some((parent_capture_end, _)) = highlights.stack.last() {
                if *parent_capture_end <= self.range.start {
                    highlights.stack.pop();
                } else {
                    break;
                }
            }

            if highlights.next_capture.is_none() {
                highlights.next_capture = highlights.captures.next();
            }

            while let Some((mat, capture_ix)) = highlights.next_capture.as_ref() {
                let capture = mat.captures[*capture_ix as usize];
                if self.range.start < capture.node.start_byte() {
                    next_capture_start = capture.node.start_byte();
                    break;
                } else {
                    let highlight_id = highlights.highlight_map.get(capture.index);
                    highlights
                        .stack
                        .push((capture.node.end_byte(), highlight_id));
                    highlights.next_capture = highlights.captures.next();
                }
            }
        }

        while let Some(endpoint) = self.diagnostic_endpoints.peek().copied() {
            if endpoint.offset <= self.range.start {
                self.update_diagnostic_depths(endpoint);
                self.diagnostic_endpoints.next();
            } else {
                next_diagnostic_endpoint = endpoint.offset;
                break;
            }
        }

        if let Some(chunk) = self.chunks.peek() {
            let chunk_start = self.range.start;
            let mut chunk_end = (self.chunks.offset() + chunk.len())
                .min(next_capture_start)
                .min(next_diagnostic_endpoint);
            let mut highlight_style = None;
            if let Some(highlights) = self.highlights.as_ref() {
                if let Some((parent_capture_end, parent_highlight_id)) = highlights.stack.last() {
                    chunk_end = chunk_end.min(*parent_capture_end);
                    highlight_style = parent_highlight_id.style(highlights.theme);
                }
            }

            let slice =
                &chunk[chunk_start - self.chunks.offset()..chunk_end - self.chunks.offset()];
            self.range.start = chunk_end;
            if self.range.start == self.chunks.offset() + chunk.len() {
                self.chunks.next().unwrap();
            }

            Some(Chunk {
                text: slice,
                highlight_style,
                diagnostic: self.current_diagnostic_severity(),
            })
        } else {
            None
        }
    }
}

impl QueryCursorHandle {
    fn new() -> Self {
        QueryCursorHandle(Some(
            QUERY_CURSORS
                .lock()
                .pop()
                .unwrap_or_else(|| QueryCursor::new()),
        ))
    }
}

impl Deref for QueryCursorHandle {
    type Target = QueryCursor;

    fn deref(&self) -> &Self::Target {
        self.0.as_ref().unwrap()
    }
}

impl DerefMut for QueryCursorHandle {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0.as_mut().unwrap()
    }
}

impl Drop for QueryCursorHandle {
    fn drop(&mut self) {
        let mut cursor = self.0.take().unwrap();
        cursor.set_byte_range(0..usize::MAX);
        cursor.set_point_range(Point::zero().to_ts_point()..Point::MAX.to_ts_point());
        QUERY_CURSORS.lock().push(cursor)
    }
}

trait ToTreeSitterPoint {
    fn to_ts_point(self) -> tree_sitter::Point;
    fn from_ts_point(point: tree_sitter::Point) -> Self;
}

impl ToTreeSitterPoint for Point {
    fn to_ts_point(self) -> tree_sitter::Point {
        tree_sitter::Point::new(self.row as usize, self.column as usize)
    }

    fn from_ts_point(point: tree_sitter::Point) -> Self {
        Point::new(point.row as u32, point.column as u32)
    }
}

trait ToPointUtf16 {
    fn to_point_utf16(self) -> PointUtf16;
}

impl ToPointUtf16 for lsp::Position {
    fn to_point_utf16(self) -> PointUtf16 {
        PointUtf16::new(self.line, self.character)
    }
}

fn diagnostic_ranges<'a>(
    diagnostic: &'a lsp::Diagnostic,
    abs_path: Option<&'a Path>,
) -> impl 'a + Iterator<Item = Range<PointUtf16>> {
    diagnostic
        .related_information
        .iter()
        .flatten()
        .filter_map(move |info| {
            if info.location.uri.to_file_path().ok()? == abs_path? {
                let info_start = PointUtf16::new(
                    info.location.range.start.line,
                    info.location.range.start.character,
                );
                let info_end = PointUtf16::new(
                    info.location.range.end.line,
                    info.location.range.end.character,
                );
                Some(info_start..info_end)
            } else {
                None
            }
        })
        .chain(Some(
            diagnostic.range.start.to_point_utf16()..diagnostic.range.end.to_point_utf16(),
        ))
}

pub fn contiguous_ranges(
    values: impl IntoIterator<Item = u32>,
    max_len: usize,
) -> impl Iterator<Item = Range<u32>> {
    let mut values = values.into_iter();
    let mut current_range: Option<Range<u32>> = None;
    std::iter::from_fn(move || loop {
        if let Some(value) = values.next() {
            if let Some(range) = &mut current_range {
                if value == range.end && range.len() < max_len {
                    range.end += 1;
                    continue;
                }
            }

            let prev_range = current_range.clone();
            current_range = Some(value..(value + 1));
            if prev_range.is_some() {
                return prev_range;
            }
        } else {
            return current_range.take();
        }
    })
}

impl crate::document::Document for Buffer {
    type Snapshot = Snapshot;
    type SelectionSet = SelectionSet;

    fn replica_id(&self) -> ReplicaId {
        (**self).replica_id()
    }

    fn language(&self) -> Option<&Arc<Language>> {
        todo!()
    }

    fn snapshot(&self) -> Self::Snapshot {
        self.snapshot()
    }

    fn subscribe(&mut self) -> Subscription {
        self.subscribe()
    }

    fn start_transaction(&mut self, set_id: Option<SelectionSetId>) -> Result<()> {
        todo!()
    }

    fn end_transaction(
        &mut self,
        set_id: Option<SelectionSetId>,
        cx: &mut ModelContext<Self>,
    ) -> Result<()> {
        todo!()
    }

    fn edit<I, S, T>(&mut self, ranges_iter: I, new_text: T, cx: &mut ModelContext<Self>)
    where
        I: IntoIterator<Item = Range<S>>,
        S: crate::document::ToDocumentOffset<Self::Snapshot>,
        T: Into<String>,
    {
        todo!()
    }

    fn edit_with_autoindent<I, S, T>(
        &mut self,
        ranges_iter: I,
        new_text: T,
        cx: &mut ModelContext<Self>,
    ) where
        I: IntoIterator<Item = Range<S>>,
        S: crate::document::ToDocumentOffset<Self::Snapshot>,
        T: Into<String>,
    {
        todo!()
    }

    fn undo(&mut self, cx: &mut ModelContext<Self>) {
        todo!()
    }

    fn redo(&mut self, cx: &mut ModelContext<Self>) {
        todo!()
    }

    fn add_selection_set<T: crate::document::ToDocumentOffset<Self::Snapshot>>(
        &mut self,
        selections: &[Selection<T>],
        cx: &mut ModelContext<Self>,
    ) -> SelectionSetId {
        todo!()
    }

    fn update_selection_set<T: crate::document::ToDocumentOffset<Self::Snapshot>>(
        &mut self,
        set_id: SelectionSetId,
        selections: &[Selection<T>],
        cx: &mut ModelContext<Self>,
    ) -> Result<()> {
        todo!()
    }

    fn remove_selection_set(
        &mut self,
        set_id: SelectionSetId,
        cx: &mut ModelContext<Self>,
    ) -> Result<()> {
        todo!()
    }

    fn set_active_selection_set(
        &mut self,
        set_id: Option<SelectionSetId>,
        cx: &mut ModelContext<Self>,
    ) -> Result<()> {
        todo!()
    }

    fn selection_set(&self, set_id: SelectionSetId) -> Option<&SelectionSet> {
        todo!()
    }

    fn selection_sets<'a>(
        &'a self,
    ) -> Box<dyn 'a + Iterator<Item = (&'a SelectionSetId, &'a SelectionSet)>> {
        todo!()
    }
}

impl crate::document::DocumentSnapshot for Snapshot {
    type Anchor = Anchor;
    type AnchorRangeSet = AnchorRangeSet;

    fn text(&self) -> String {
        todo!()
    }

    fn text_for_range<'a, T: crate::document::ToDocumentOffset<Self>>(
        &'a self,
        range: Range<T>,
    ) -> Box<dyn 'a + Iterator<Item = &'a str>> {
        todo!()
    }

    fn chunks<'a, T: crate::document::ToDocumentOffset<Self>>(
        &'a self,
        range: Range<T>,
        theme: Option<&'a SyntaxTheme>,
    ) -> Box<dyn 'a + crate::document::DocumentChunks<'a>> {
        Box::new(self.chunks(
            range.start.to_offset(self)..range.end.to_offset(self),
            theme,
        ))
    }

    fn chars_at<'a, T: crate::document::ToDocumentOffset<Self>>(
        &'a self,
        position: T,
    ) -> Box<dyn 'a + Iterator<Item = char>> {
        todo!()
    }

    fn chars_for_range<'a, T: crate::document::ToDocumentOffset<Self>>(
        &'a self,
        range: Range<T>,
    ) -> Box<dyn 'a + Iterator<Item = char>> {
        todo!()
    }

    fn reversed_chars_at<'a, T: crate::document::ToDocumentOffset<Self>>(
        &'a self,
        position: T,
    ) -> Box<dyn 'a + Iterator<Item = char>> {
        todo!()
    }

    fn bytes_in_range<'a, T: crate::document::ToDocumentOffset<Self>>(
        &'a self,
        range: Range<T>,
    ) -> Box<dyn 'a + crate::document::DocumentBytes<'a>> {
        todo!()
    }

    fn contains_str_at<T: crate::document::ToDocumentOffset<Self>>(
        &self,
        position: T,
        needle: &str,
    ) -> bool {
        todo!()
    }

    fn is_line_blank(&self, row: u32) -> bool {
        todo!()
    }

    fn indent_column_for_line(&self, row: u32) -> u32 {
        todo!()
    }

    fn range_for_syntax_ancestor<T: crate::document::ToDocumentOffset<Self>>(
        &self,
        range: Range<T>,
    ) -> Option<Range<usize>> {
        todo!()
    }

    fn enclosing_bracket_ranges<T: crate::document::ToDocumentOffset<Self>>(
        &self,
        range: Range<T>,
    ) -> Option<(Range<usize>, Range<usize>)> {
        todo!()
    }

    fn text_summary(&self) -> TextSummary {
        (**self).text_summary()
    }

    fn text_summary_for_range<'a, D, O>(&'a self, range: Range<O>) -> D
    where
        D: rope::TextDimension,
        O: crate::document::ToDocumentOffset<Self>,
    {
        (**self).text_summary_for_range(range.start.to_offset(self)..range.end.to_offset(self))
    }

    fn max_point(&self) -> Point {
        (**self).max_point()
    }

    fn len(&self) -> usize {
        (**self).len()
    }

    fn line_len(&self, row: u32) -> u32 {
        (**self).line_len(row)
    }

    fn anchor_before<T: crate::document::ToDocumentOffset<Self>>(
        &self,
        position: T,
    ) -> Self::Anchor {
        (**self).anchor_before(position.to_offset(self))
    }

    fn anchor_at<T: crate::document::ToDocumentOffset<Self>>(
        &self,
        position: T,
        bias: Bias,
    ) -> Self::Anchor {
        todo!()
    }

    fn anchor_after<T: crate::document::ToDocumentOffset<Self>>(
        &self,
        position: T,
    ) -> Self::Anchor {
        (**self).anchor_after(position.to_offset(self))
    }

    fn anchor_range_set<E>(
        &self,
        start_bias: Bias,
        end_bias: Bias,
        entries: E,
    ) -> Self::AnchorRangeSet
    where
        E: IntoIterator<Item = Range<usize>>,
    {
        todo!()
    }

    fn clip_offset(&self, offset: usize, bias: Bias) -> usize {
        (**self).clip_offset(offset, bias)
    }

    fn clip_point(&self, point: Point, bias: Bias) -> Point {
        (**self).clip_point(point, bias)
    }

    fn to_offset(&self, point: Point) -> usize {
        (**self).to_offset(point)
    }

    fn to_point(&self, offset: usize) -> Point {
        (**self).to_point(offset)
    }

    fn parse_count(&self) -> usize {
        self.parse_count()
    }

    fn diagnostics_update_count(&self) -> usize {
        self.diagnostics_update_count()
    }

    fn diagnostics_in_range<'a, T, O>(
        &'a self,
        search_range: Range<T>,
    ) -> Box<dyn 'a + Iterator<Item = (Range<O>, &Diagnostic)>>
    where
        T: 'a + crate::document::ToDocumentOffset<Self>,
        O: 'a + crate::document::FromDocumentAnchor<Self>,
    {
        todo!()
    }

    fn diagnostic_group<'a, O>(
        &'a self,
        group_id: usize,
    ) -> Box<dyn 'a + Iterator<Item = (Range<O>, &Diagnostic)>>
    where
        O: 'a + crate::document::FromDocumentAnchor<Self>,
    {
        todo!()
    }
}

impl crate::document::DocumentAnchor for Anchor {
    type Snapshot = Snapshot;

    fn min() -> Self {
        Self::min()
    }

    fn max() -> Self {
        Self::max()
    }

    fn cmp(&self, other: &Self, snapshot: &Self::Snapshot) -> cmp::Ordering {
        self.cmp(other, snapshot).unwrap()
    }

    fn summary<'a, D: rope::TextDimension>(&self, snapshot: &'a Self::Snapshot) -> D {
        self.summary(snapshot)
    }
}

impl crate::document::DocumentAnchorRangeSet for AnchorRangeSet {
    type Snapshot = Snapshot;

    fn len(&self) -> usize {
        self.len()
    }

    fn version(&self) -> &clock::Global {
        self.version()
    }

    fn ranges<'a, D>(
        &'a self,
        snapshot: &'a Self::Snapshot,
    ) -> Box<dyn 'a + Iterator<Item = Range<Point>>>
    where
        D: rope::TextDimension,
    {
        Box::new(self.ranges::<D>(snapshot))
    }
}

impl crate::document::DocumentSelectionSet for SelectionSet {
    type Document = Buffer;

    fn len(&self) -> usize {
        todo!()
    }

    fn is_active(&self) -> bool {
        self.active
    }

    fn intersecting_selections<'a, D, I>(
        &'a self,
        range: Range<(I, Bias)>,
        snapshot: &'a Buffer,
    ) -> Box<dyn 'a + Iterator<Item = Selection<D>>>
    where
        D: 'a + rope::TextDimension,
        I: 'a + crate::document::ToDocumentOffset<Snapshot>,
    {
        todo!()
    }

    fn selections<'a, D>(
        &'a self,
        document: &'a Self::Document,
    ) -> Box<dyn 'a + Iterator<Item = Selection<D>>>
    where
        D: rope::TextDimension,
    {
        todo!()
    }

    fn oldest_selection<'a, D>(&'a self, document: &'a Self::Document) -> Option<Selection<D>>
    where
        D: rope::TextDimension,
    {
        todo!()
    }

    fn newest_selection<'a, D>(&'a self, document: &'a Self::Document) -> Option<Selection<D>>
    where
        D: rope::TextDimension,
    {
        todo!()
    }
}

impl<'a> crate::document::DocumentChunks<'a> for Chunks<'a> {
    fn seek(&mut self, offset: usize) {
        self.seek(offset);
    }

    fn offset(&self) -> usize {
        self.offset()
    }
}

impl crate::document::ToDocumentOffset<Snapshot> for PointUtf16 {
    fn to_offset<'a>(&self, content: &Snapshot) -> usize {
        text::ToOffset::to_offset(self, content)
    }
}

impl crate::document::ToDocumentOffset<Snapshot> for Anchor {
    fn to_offset<'a>(&self, content: &Snapshot) -> usize {
        text::ToOffset::to_offset(self, content)
    }
}

impl<'a> crate::document::ToDocumentOffset<Snapshot> for &'a Anchor {
    fn to_offset(&self, content: &Snapshot) -> usize {
        text::ToOffset::to_offset(self, content)
    }
}

impl crate::document::ToDocumentPoint<Snapshot> for Anchor {
    fn to_point<'a>(&self, content: &Snapshot) -> Point {
        text::ToPoint::to_point(self, content)
    }
}
