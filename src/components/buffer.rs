use euclid::default::SideOffsets2D;
use ropey::{Rope, RopeSlice};
use size_format::SizeFormatterBinary;
use std::{
    borrow::Cow,
    cmp,
    fs::File,
    io::{self, BufReader, BufWriter},
    iter,
    path::PathBuf,
    time::Instant,
};
use tree_sitter::{InputEdit as TreeSitterInputEdit, Point as TreeSitterPoint};
use zee_highlight::SelectorNodeId;

use super::{
    cursor::Cursor,
    syntax::{
        text_style_at_char, NodeTrace, ParserStatus, SyntaxCursor, SyntaxTree, Theme as SyntaxTheme,
    },
    theme::Theme as EditorTheme,
    Component, Context, Scheduler, TaskKind, TaskResult,
};
use crate::{
    error::Result,
    mode::{self, Mode},
    terminal::{Position, Rect, Screen, Size, Style},
    utils::{
        self, next_grapheme_boundary, prev_grapheme_boundary, strip_trailing_whitespace,
        RopeGraphemes, TAB_WIDTH,
    },
};
use termion::event::Key;

#[derive(Clone, Debug)]
pub struct Theme {
    pub syntax: SyntaxTheme,
    pub border: Style,
    pub status_base: Style,
    pub status_frame_id_focused: Style,
    pub status_frame_id_unfocused: Style,
    pub status_is_modified: Style,
    pub status_is_not_modified: Style,
    pub status_file_name: Style,
    pub status_file_size: Style,
    pub status_position_in_file: Style,
    pub status_mode: Style,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OpaqueDiff {
    byte_index: usize,
    old_length: usize,
    new_length: usize,
}

impl OpaqueDiff {
    fn new(byte_index: usize, old_length: usize, new_length: usize) -> Self {
        Self {
            byte_index,
            old_length,
            new_length,
        }
    }

    fn no_diff() -> Self {
        Self {
            byte_index: 0,
            old_length: 0,
            new_length: 0,
        }
    }

    fn is_empty(&self) -> bool {
        self.byte_index == 0 && self.old_length == 0 && self.new_length == 0
    }
}

#[derive(Clone, Debug)]
enum ModifiedStatus {
    Changed,
    Unchanged,
    Saving(Instant),
}

pub enum BufferTask {
    SaveFile { text: Rope },
    ParseSyntax(ParserStatus),
}

pub struct Buffer {
    mode: &'static Mode,
    text: Rope,
    yanked_line: Option<Rope>,
    has_unsaved_changes: ModifiedStatus,
    file_path: Option<PathBuf>,
    cursor: Cursor,
    first_line: usize,
    syntax: Option<SyntaxTree>,
}

impl Buffer {
    pub fn from_file(file_path: PathBuf) -> Result<Self> {
        let mode = mode::find_by_filename(&file_path);
        Ok(Buffer {
            text: if file_path.exists() {
                Rope::from_reader(BufReader::new(File::open(&file_path)?))?
            } else {
                // Optimistically check we can create it
                File::open(&file_path).map(|_| ()).or_else(|error| {
                    if error.kind() == io::ErrorKind::NotFound {
                        Ok(())
                    } else {
                        Err(error)
                    }
                })?;
                Rope::new()
            },
            yanked_line: None,
            has_unsaved_changes: ModifiedStatus::Unchanged,
            file_path: Some(file_path),
            cursor: Cursor::new(),
            first_line: 0,
            syntax: mode.language().map(|language| SyntaxTree::new(*language)),
            mode,
        })
    }

    pub fn spawn_save_file(&mut self, scheduler: &mut Scheduler, context: &Context) -> Result<()> {
        self.has_unsaved_changes = ModifiedStatus::Saving(context.time);
        if let Some(ref file_path) = self.file_path {
            let text = self.text.clone();
            let file_path = file_path.clone();
            scheduler.spawn(move || {
                let text = strip_trailing_whitespace(text);
                text.write_to(BufWriter::new(File::create(&file_path)?))?;
                Ok(TaskKind::Buffer(BufferTask::SaveFile { text }))
            })?;
        }

        Ok(())
    }

    #[inline]
    fn ensure_cursor_in_view(&mut self, frame: &Rect) {
        let new_line = self.text.char_to_line(self.cursor.range.start);
        if new_line < self.first_line {
            self.first_line = new_line;
        } else if new_line - self.first_line > frame.size.height - 1 {
            self.first_line = new_line - frame.size.height + 1;
        }
    }

    #[inline]
    fn draw_line(
        &self,
        screen: &mut Screen,
        context: &Context,
        line_index: usize,
        line: RopeSlice,
        mut syntax_cursor: Option<&mut SyntaxCursor>,
        mut trace: &mut NodeTrace<SelectorNodeId>,
    ) -> usize {
        // Get references to the relevant bits of context
        let Context {
            ref frame,
            focused,
            theme: EditorTheme {
                buffer: ref theme, ..
            },
            ..
        } = *context;

        // Highlight the currently selected line
        let line_under_cursor = self.text.char_to_line(self.cursor.range.start) == line_index;
        if line_under_cursor && focused {
            screen.clear_region(
                Rect::new(
                    Position::new(frame.origin.x, frame.origin.y),
                    Size::new(frame.size.width, 1),
                ),
                theme.syntax.text_current_line,
            );
        }

        let mut visual_cursor_x = 0;
        let mut visual_x = frame.origin.x;
        let mut char_index = self.text.line_to_char(line_index);

        let mut content: Cow<str> = self
            .text
            .slice(
                self.text.byte_to_char(trace.byte_range.start)
                    ..self.text.byte_to_char(trace.byte_range.end),
            )
            .into();
        let mut scope = self
            .mode
            .highlights()
            .and_then(|highlights| highlights.matches(&trace.trace, &trace.nth_children, &content))
            .map(|scope| scope.0.as_str());

        for grapheme in RopeGraphemes::new(&line.slice(..)) {
            let byte_index = self.text.char_to_byte(char_index);
            match (syntax_cursor.as_mut(), self.mode.highlights()) {
                (Some(syntax_cursor), Some(highlights))
                    if !trace.byte_range.contains(&byte_index) =>
                {
                    syntax_cursor.trace_at(&mut trace, byte_index, |node| {
                        highlights.get_selector_node_id(node.kind_id())
                    });
                    content = self
                        .text
                        .slice(
                            self.text.byte_to_char(trace.byte_range.start)
                                ..self.text.byte_to_char(trace.byte_range.end),
                        )
                        .into();

                    scope = highlights
                        .matches(&trace.trace, &trace.nth_children, &content)
                        .map(|scope| scope.0.as_str());
                }
                _ => {}
            };

            if self.cursor.range.contains(&char_index) && focused {
                eprintln!(
                    "Symbol under cursor [{}] -- {:?} {:?} {:?} {}",
                    scope.unwrap_or(""),
                    trace.path,
                    trace.trace,
                    trace.nth_children,
                    content,
                );
                visual_cursor_x = visual_x.saturating_sub(frame.origin.x);
            }

            let style = text_style_at_char(
                &theme.syntax,
                &self.cursor,
                char_index,
                focused,
                line_under_cursor,
                scope.unwrap_or(""),
                trace.is_error,
            );
            let grapheme_width = utils::grapheme_width(&grapheme);
            let horizontal_bounds_inclusive = frame.min_x()..=frame.max_x();
            if !horizontal_bounds_inclusive.contains(&(visual_x + grapheme_width)) {
                break;
            }

            if grapheme == "\t" {
                for offset in 0..grapheme_width {
                    screen.draw_str(visual_x + offset, frame.origin.y, style, " ");
                }
            } else if grapheme_width == 0 {
                screen.draw_str(visual_x, frame.origin.y, style, " ");
            } else {
                screen.draw_rope_slice(visual_x, frame.origin.y, style, &grapheme);
            }

            char_index += grapheme.len_chars();
            visual_x += grapheme_width;
        }

        if line_index == self.text.len_lines() - 1
            && self.cursor.range.start == self.text.len_chars()
        {
            screen.draw_str(
                frame.origin.x,
                frame.origin.y,
                if focused {
                    theme.syntax.cursor_focused
                } else {
                    theme.syntax.cursor_unfocused
                },
                " ",
            );
        }

        visual_cursor_x
    }

    #[inline]
    fn draw_text(&mut self, screen: &mut Screen, context: &Context) -> usize {
        self.ensure_cursor_in_view(&context.frame);
        let mut syntax_cursor = self.syntax.as_ref().and_then(|syntax| syntax.cursor());
        let mut trace: NodeTrace<SelectorNodeId> = NodeTrace::new();

        let mut visual_cursor_x = 0;
        for (line_index, line) in self
            .text
            .lines_at(self.first_line)
            .take(context.frame.size.height)
            .enumerate()
        {
            visual_cursor_x = cmp::max(
                visual_cursor_x,
                self.draw_line(
                    screen,
                    &context.set_frame(
                        context
                            .frame
                            .inner_rect(SideOffsets2D::new(line_index, 0, 0, 0)),
                    ),
                    self.first_line + line_index,
                    line,
                    syntax_cursor.as_mut(),
                    &mut trace,
                ),
            );
        }

        visual_cursor_x
    }

    #[inline]
    fn draw_line_info(&self, screen: &mut Screen, context: &Context) {
        for screen_index in 0..context.frame.size.height - 1 {
            screen.draw_str(
                context.frame.origin.x,
                context.frame.origin.y + screen_index as usize,
                context.theme.buffer.border,
                if self.first_line + screen_index < self.text.len_lines() - 1 {
                    " "
                } else {
                    "~"
                },
            );
        }
    }

    #[inline]
    fn draw_status_bar(&self, screen: &mut Screen, context: &Context, visual_cursor_x: usize) {
        let Context {
            ref frame,
            frame_id,
            focused,
            theme: EditorTheme {
                buffer: ref theme, ..
            },
            ..
        } = *context;
        let line_height = frame.origin.y + frame.size.height - 1;
        screen.clear_region(
            Rect::new(
                Position::new(frame.origin.x, line_height),
                Size::new(frame.size.width, 1),
            ),
            theme.status_base,
        );

        let mut offset = frame.origin.x;
        // Buffer number
        offset += screen.draw_str(
            offset,
            line_height,
            if focused {
                theme.status_frame_id_focused
            } else {
                theme.status_frame_id_unfocused
            },
            &format!(" {} ", frame_id),
        );

        // Has unsaved changes
        offset += screen.draw_str(
            offset,
            line_height,
            match self.has_unsaved_changes {
                ModifiedStatus::Unchanged => theme.status_is_not_modified,
                _ => theme.status_is_modified,
            },
            match self.has_unsaved_changes {
                ModifiedStatus::Unchanged => " - ",
                ModifiedStatus::Changed | ModifiedStatus::Saving(..) => " ☲ ",
                // ModifiedStatus::Saving(start_time) => [" | ", " / ", " - ", " \\ "]
                //     [((context.time - start_time).as_millis() / 250) as usize % 4],
            },
        );

        // File size
        offset += screen.draw_str(
            offset,
            line_height,
            theme.status_file_size,
            &format!(
                " {} ",
                SizeFormatterBinary::new(self.text.len_bytes() as u64)
            ),
        );

        // File name if buffer is backed by a file
        offset += screen.draw_str(
            offset,
            line_height,
            theme.status_file_name,
            &self
                .file_path
                .as_ref()
                .map(|path| format!("{} ", path.display()))
                .unwrap_or_else(String::new),
        );

        // Name of the current mode
        screen.draw_str(
            offset,
            line_height,
            theme.status_mode,
            &format!(" {}", self.mode.name),
        );

        // The current position the file right-aligned
        let current_line = self.text.char_to_line(self.cursor.range.start);
        let num_lines = self.text.len_lines();
        let line_status = format!(
            " {current_line}:{current_byte:>2} {percent:>3}% ",
            current_line = current_line,
            current_byte = visual_cursor_x,
            percent = if num_lines > 0 {
                100 * (current_line + 1) / num_lines
            } else {
                100
            }
        );
        screen.draw_str(
            frame.origin.x + frame.size.width.saturating_sub(line_status.len()),
            line_height,
            theme.status_position_in_file,
            &line_status,
        );
    }

    fn center_visual_cursor(&mut self, frame: &Rect) {
        let line_index = self.text.char_to_line(self.cursor.range.start);
        if line_index >= frame.size.height / 2 {
            let middle_vertical = line_index - frame.size.height / 2;

            if self.first_line != middle_vertical {
                self.first_line = middle_vertical;
            } else if self.first_line != line_index {
                self.first_line = line_index;
            }

            // In Emacs C-L is a 3 state ting
            // else if self.text.len_lines() > frame.size.height {
            //     self.first_line = line_index - frame.size.height;
            // }
        }
    }

    fn insert_characters(&mut self, characters: impl Iterator<Item = char>) -> OpaqueDiff {
        self.has_unsaved_changes = ModifiedStatus::Changed;
        let mut num_bytes = 0;
        characters.enumerate().for_each(|(offset, character)| {
            self.text
                .insert_char(self.cursor.range.start + offset, character);
            num_bytes += character.len_utf8();
        });
        OpaqueDiff::new(
            self.text.char_to_byte(self.cursor.range.start),
            0,
            num_bytes,
        )
    }

    fn insert_char(&mut self, character: char) -> OpaqueDiff {
        self.has_unsaved_changes = ModifiedStatus::Changed;
        self.text.insert_char(self.cursor.range.start, character);
        OpaqueDiff::new(self.cursor.range.start, 0, 1)
    }

    fn delete(&mut self) -> OpaqueDiff {
        if self.text.len_chars() == 0 {
            return OpaqueDiff::no_diff();
        }

        self.has_unsaved_changes = ModifiedStatus::Changed;
        self.text
            .remove(self.cursor.range.start..self.cursor.range.end);
        let diff = OpaqueDiff::new(
            self.cursor.range.start,
            self.cursor.range.end - self.cursor.range.start,
            0,
        );

        let grapheme_start = self.cursor.range.start;
        let grapheme_end = next_grapheme_boundary(&self.text.slice(..), self.cursor.range.start);
        if grapheme_start < grapheme_end {
            self.cursor.range = grapheme_start..grapheme_end
        } else {
            self.cursor.range = 0..1
        }
        diff
    }

    fn delete_line(&mut self) -> OpaqueDiff {
        if self.text.len_chars() == 0 {
            return OpaqueDiff::no_diff();
        }

        // Delete line
        self.has_unsaved_changes = ModifiedStatus::Changed;
        let line_index = self.text.char_to_line(self.cursor.range.start);
        let delete_range_start = self.text.line_to_char(line_index);
        let delete_range_end = self.text.line_to_char(line_index + 1);
        self.yanked_line = Some(Rope::from(
            self.text.slice(delete_range_start..delete_range_end),
        ));
        self.text.remove(delete_range_start..delete_range_end);
        let diff = OpaqueDiff::new(delete_range_start, delete_range_end - delete_range_start, 0);

        // Place cursor
        let grapheme_start = self.text.line_to_char(cmp::min(
            line_index,
            self.text.len_lines().saturating_sub(2),
        ));
        let grapheme_end = next_grapheme_boundary(&self.text.slice(..), grapheme_start);
        if grapheme_start != grapheme_end {
            self.cursor.range = grapheme_start..grapheme_end
        } else {
            self.cursor.range = 0..1
        }
        self.cursor.visual_horizontal_offset = None;

        diff
    }

    fn yank_line(&mut self) -> OpaqueDiff {
        if let Some(clipboard) = self.yanked_line.as_ref() {
            self.has_unsaved_changes = ModifiedStatus::Changed;
            let diff = OpaqueDiff::new(self.cursor.range.start, 0, clipboard.len_bytes());
            let mut cursor_start = self.cursor.range.start;
            for chunk in clipboard.chunks() {
                self.text.insert(cursor_start, chunk);
                cursor_start += chunk.chars().count();
                self.cursor.range =
                    cursor_start..next_grapheme_boundary(&self.text.slice(..), cursor_start);
            }
            return diff;
        }
        OpaqueDiff::no_diff()
    }

    fn copy_selection(&mut self) -> OpaqueDiff {
        self.yanked_line = Some(Rope::from(self.text.slice(self.cursor.selection())));
        self.cursor.select = None;
        OpaqueDiff::no_diff()
    }

    fn cut_selection(&mut self) -> OpaqueDiff {
        let selection = self.cursor.selection();
        let diff = OpaqueDiff::new(selection.start, selection.end - selection.start, 0);
        self.has_unsaved_changes = ModifiedStatus::Changed;
        self.yanked_line = Some(Rope::from(self.text.slice(selection)));
        self.text.remove(self.cursor.selection());
        self.cursor.select = None;

        let grapheme_start = cmp::min(
            self.cursor.range.start,
            prev_grapheme_boundary(&self.text.slice(..), self.text.len_chars()),
        );
        let grapheme_end = next_grapheme_boundary(&self.text.slice(..), grapheme_start);
        if grapheme_start != grapheme_end {
            self.cursor.range = grapheme_start..grapheme_end
        } else {
            self.cursor.range = 0..1
        }
        self.cursor.visual_horizontal_offset = None;
        diff
    }

    fn ensure_trailing_newline_with_content(&mut self) {
        if self.text.len_chars() == 0 || self.text.char(self.text.len_chars() - 1) != '\n' {
            self.text.insert_char(self.text.len_chars(), '\n');
        }
    }
}

impl Component for Buffer {
    #[inline]
    fn draw(&mut self, screen: &mut Screen, scheduler: &mut Scheduler, context: &Context) {
        {
            let Self {
                ref mut syntax,
                ref text,
                ..
            } = self;
            if let Some(syntax) = syntax.as_mut() {
                syntax.ensure_tree(scheduler, || text.clone()).unwrap();
            };
        }

        screen.clear_region(context.frame, context.theme.buffer.syntax.text);
        self.draw_line_info(screen, context);
        let visual_cursor_x = self.draw_text(
            screen,
            &context.set_frame(context.frame.inner_rect(SideOffsets2D::new(0, 0, 1, 1))),
        );
        self.draw_status_bar(screen, context, visual_cursor_x);
    }

    #[inline]
    fn handle_event(
        &mut self,
        key: Key,
        scheduler: &mut Scheduler,
        context: &Context,
    ) -> Result<()> {
        // Stateless
        match key {
            Key::Ctrl('p') | Key::Up => {
                self.cursor.move_up(&self.text);
            }
            Key::Ctrl('n') | Key::Down => {
                self.cursor.move_down(&self.text);
            }
            Key::Ctrl('b') | Key::Left => {
                self.cursor.move_left(&self.text);
            }
            Key::Ctrl('f') | Key::Right => {
                self.cursor.move_right(&self.text);
            }
            Key::Ctrl('v') | Key::PageDown => {
                for _ in 0..(context.frame.size.height - 1) {
                    self.cursor.move_down(&self.text);
                }
            }
            Key::Alt('v') | Key::PageUp => {
                for _ in 0..(context.frame.size.height - 1) {
                    self.cursor.move_up(&self.text);
                }
            }
            Key::Ctrl('a') | Key::Home => {
                self.cursor.move_to_start_of_line(&self.text);
            }
            Key::Ctrl('e') | Key::End => {
                self.cursor.move_to_end_of_line(&self.text);
            }
            Key::Alt('<') => {
                self.cursor.move_to_start_of_buffer(&self.text);
            }
            Key::Alt('>') => {
                self.cursor.move_to_end_of_buffer(&self.text);
            }
            Key::Ctrl('l') => self.center_visual_cursor(&context.frame),

            Key::Null => self.cursor.select = Some(self.cursor.range.start),
            Key::Ctrl('g') => self.cursor.select = None,
            Key::Alt('s') => {
                self.spawn_save_file(scheduler, context)?;
            }
            _ => {}
        };

        let diff = match key {
            Key::Ctrl('d') | Key::Delete => self.delete(),
            Key::Ctrl('k') => self.delete_line(),
            Key::Ctrl('y') => self.yank_line(),
            Key::Backspace => {
                if self.cursor.range.start > 0 {
                    self.cursor.move_left(&self.text);
                    self.delete()
                } else {
                    OpaqueDiff::no_diff()
                }
            }
            Key::Alt('w') => self.copy_selection(),
            Key::Ctrl('w') => self.cut_selection(),
            Key::Char('\t') if DISABLE_TABS => {
                let diff = self.insert_characters(iter::repeat(' ').take(TAB_WIDTH));
                self.cursor.move_right_n(&self.text, TAB_WIDTH);
                diff
            }
            Key::Char('\n') => {
                let diff = self.insert_char('\n');
                self.ensure_trailing_newline_with_content();
                self.cursor.move_down(&self.text);
                self.cursor.move_to_start_of_line(&self.text);
                diff
            }
            Key::Char(character) => {
                let diff = self.insert_char(character);
                self.ensure_trailing_newline_with_content();
                self.cursor.move_right(&self.text);
                diff
            }
            _ => OpaqueDiff::no_diff(),
        };

        if !diff.is_empty() && self.mode.language().is_some() {
            let changes = TreeSitterInputEdit {
                start_byte: diff.byte_index,
                old_end_byte: diff.byte_index + diff.old_length,
                new_end_byte: diff.byte_index + diff.new_length,
                // I don't use tree sitter's line/col tracking; I'm assuming
                // here that passing in dummy values doesn't cause any other
                // problem apart from incorrect line/col after editing a tree.
                start_position: TreeSitterPoint::new(0, 0),
                old_end_position: TreeSitterPoint::new(0, 0),
                new_end_position: TreeSitterPoint::new(0, 0),
            };

            {
                let Self {
                    ref mut syntax,
                    ref text,
                    ..
                } = self;
                if let Some(syntax) = syntax.as_mut() {
                    if let Some(tree) = syntax.tree.as_mut() {
                        tree.edit(&changes);
                    }
                    let text = text.clone();
                    syntax.spawn_parse_task(scheduler, text)?;
                };
            }
        }

        Ok(())
    }

    fn task_done(&mut self, task_result: TaskResult) -> Result<()> {
        let task_id = task_result.id;
        let payload = task_result.payload?;
        match payload {
            TaskKind::Buffer(BufferTask::SaveFile { text: new_text }) => {
                let current_line = self.text.char_to_line(self.cursor.range.start);
                let current_line_offset =
                    self.cursor.range.start - self.text.line_to_char(current_line);

                self.cursor = {
                    let new_line = cmp::min(current_line, new_text.len_lines().saturating_sub(1));
                    let new_line_offset = cmp::min(
                        current_line_offset,
                        new_text.line(new_line).len_chars().saturating_sub(1),
                    );
                    let grapheme_end = next_grapheme_boundary(
                        &new_text.slice(..),
                        new_text.line_to_char(new_line) + new_line_offset,
                    );
                    let grapheme_start = prev_grapheme_boundary(&new_text.slice(..), grapheme_end);
                    Cursor {
                        range: if grapheme_start != grapheme_end {
                            grapheme_start..grapheme_end
                        } else {
                            0..1
                        },
                        visual_horizontal_offset: None,
                        select: None,
                    }
                };

                self.text = new_text.clone();
                self.has_unsaved_changes = ModifiedStatus::Unchanged;
            }
            TaskKind::Buffer(BufferTask::ParseSyntax(parsed)) => {
                self.syntax
                    .as_mut()
                    .map(|syntax| syntax.handle_parse_syntax_done(task_id, parsed));
            }
        }

        Ok(())
    }
}

const DISABLE_TABS: bool = false;
