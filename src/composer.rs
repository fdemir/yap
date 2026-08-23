use std::collections::VecDeque;

use unicode_width::UnicodeWidthStr;

use crate::security::terminal_safe_text;

pub(crate) const MAX_COMPOSER_BYTES: usize = 64 * 1024;
const MAX_HISTORY_BYTES: usize = 1024 * 1024;
const MAX_HISTORY_ENTRIES: usize = 100;

pub(crate) struct Composer {
    text: String,
    cursor: usize,
    preferred_column: Option<usize>,
    history: VecDeque<String>,
    history_bytes: usize,
    history_index: Option<usize>,
    history_draft: Option<HistoryDraft>,
}

struct HistoryDraft {
    text: String,
    cursor: usize,
}

impl Composer {
    pub(crate) fn new() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            preferred_column: None,
            history: VecDeque::new(),
            history_bytes: 0,
            history_index: None,
            history_draft: None,
        }
    }

    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    pub(crate) fn cursor(&self) -> usize {
        self.cursor
    }

    pub(crate) fn line_count(&self) -> usize {
        self.text.bytes().filter(|byte| *byte == b'\n').count() + 1
    }

    pub(crate) fn submit(&mut self) -> Option<String> {
        let prompt = self.text.trim().to_owned();
        if prompt.is_empty() {
            return None;
        }
        self.record_history(&prompt);
        self.text.clear();
        self.cursor = 0;
        self.preferred_column = None;
        self.history_index = None;
        self.history_draft = None;
        Some(prompt)
    }

    pub(crate) fn insert_character(&mut self, character: char) {
        let character = if character == '\r' { '\n' } else { character };
        if self.text.len().saturating_add(character.len_utf8()) > MAX_COMPOSER_BYTES {
            return;
        }
        self.prepare_edit();
        self.text.insert(self.cursor, character);
        self.cursor += character.len_utf8();
        self.preferred_column = None;
    }

    pub(crate) fn insert_text(&mut self, text: &str) {
        let available = MAX_COMPOSER_BYTES.saturating_sub(self.text.len());
        if available == 0 || text.is_empty() {
            return;
        }

        let mut inserted = String::with_capacity(available.min(text.len()));
        let mut characters = text.chars().peekable();
        while let Some(mut character) = characters.next() {
            if character == '\r' {
                if characters.peek() == Some(&'\n') {
                    characters.next();
                }
                character = '\n';
            }
            if inserted.len().saturating_add(character.len_utf8()) > available {
                break;
            }
            inserted.push(character);
        }
        if inserted.is_empty() {
            return;
        }

        self.prepare_edit();
        self.text.insert_str(self.cursor, &inserted);
        self.cursor += inserted.len();
        self.preferred_column = None;
    }

    pub(crate) fn backspace(&mut self) {
        let Some(previous) = previous_boundary(&self.text, self.cursor) else {
            return;
        };
        self.prepare_edit();
        self.text.replace_range(previous..self.cursor, "");
        self.cursor = previous;
        self.preferred_column = None;
    }

    pub(crate) fn delete(&mut self) {
        let Some(next) = next_boundary(&self.text, self.cursor) else {
            return;
        };
        self.prepare_edit();
        self.text.replace_range(self.cursor..next, "");
        self.preferred_column = None;
    }

    pub(crate) fn move_left(&mut self) {
        if let Some(previous) = previous_boundary(&self.text, self.cursor) {
            self.cursor = previous;
        }
        self.preferred_column = None;
    }

    pub(crate) fn move_right(&mut self) {
        if let Some(next) = next_boundary(&self.text, self.cursor) {
            self.cursor = next;
        }
        self.preferred_column = None;
    }

    pub(crate) fn move_home(&mut self) {
        self.cursor = line_bounds(&self.text, self.cursor).0;
        self.preferred_column = None;
    }

    pub(crate) fn move_end(&mut self) {
        self.cursor = line_bounds(&self.text, self.cursor).1;
        self.preferred_column = None;
    }

    pub(crate) fn move_vertical(&mut self, direction: i8) {
        let (line_start, line_end) = line_bounds(&self.text, self.cursor);
        let preferred = self
            .preferred_column
            .unwrap_or_else(|| composer_display_width(&self.text[line_start..self.cursor]));
        let target = if direction < 0 {
            if line_start == 0 {
                return;
            }
            let target_end = line_start - 1;
            let target_start = self.text[..target_end]
                .rfind('\n')
                .map_or(0, |index| index + 1);
            (target_start, target_end)
        } else {
            if line_end == self.text.len() {
                return;
            }
            let target_start = line_end + 1;
            let target_end = self.text[target_start..]
                .find('\n')
                .map_or(self.text.len(), |index| target_start + index);
            (target_start, target_end)
        };
        self.cursor = target.0 + byte_at_visual_column(&self.text[target.0..target.1], preferred);
        self.preferred_column = Some(preferred);
    }

    pub(crate) fn previous_history(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let index = match self.history_index {
            Some(index) => index.saturating_sub(1),
            None => {
                self.history_draft = Some(HistoryDraft {
                    text: self.text.clone(),
                    cursor: self.cursor,
                });
                self.history.len() - 1
            }
        };
        self.history_index = Some(index);
        self.text = self.history[index].clone();
        self.cursor = self.text.len();
        self.preferred_column = None;
    }

    pub(crate) fn next_history(&mut self) {
        let Some(index) = self.history_index else {
            return;
        };
        if index + 1 < self.history.len() {
            let next = index + 1;
            self.history_index = Some(next);
            self.text = self.history[next].clone();
            self.cursor = self.text.len();
        } else {
            let draft = self.history_draft.take().unwrap_or(HistoryDraft {
                text: String::new(),
                cursor: 0,
            });
            self.text = draft.text;
            self.cursor = draft.cursor.min(self.text.len());
            self.history_index = None;
        }
        self.preferred_column = None;
    }

    fn record_history(&mut self, prompt: &str) {
        if self.history.back().is_some_and(|entry| entry == prompt) {
            return;
        }
        while self.history.len() >= MAX_HISTORY_ENTRIES
            || self.history_bytes.saturating_add(prompt.len()) > MAX_HISTORY_BYTES
        {
            let Some(removed) = self.history.pop_front() else {
                break;
            };
            self.history_bytes = self.history_bytes.saturating_sub(removed.len());
        }
        self.history.push_back(prompt.to_owned());
        self.history_bytes = self.history_bytes.saturating_add(prompt.len());
    }

    fn prepare_edit(&mut self) {
        if self.history_index.take().is_some() {
            self.history_draft = None;
        }
    }
}

fn previous_boundary(text: &str, cursor: usize) -> Option<usize> {
    (cursor > 0).then(|| {
        text[..cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(index, _)| index)
    })
}

fn next_boundary(text: &str, cursor: usize) -> Option<usize> {
    if cursor >= text.len() {
        return None;
    }
    text[cursor..]
        .chars()
        .next()
        .map(|character| cursor + character.len_utf8())
}

fn line_bounds(text: &str, cursor: usize) -> (usize, usize) {
    let start = text[..cursor].rfind('\n').map_or(0, |index| index + 1);
    let end = text[cursor..]
        .find('\n')
        .map_or(text.len(), |index| cursor + index);
    (start, end)
}

fn byte_at_visual_column(line: &str, column: usize) -> usize {
    let mut width: usize = 0;
    for (index, character) in line.char_indices() {
        let mut encoded = [0; 4];
        let character_width = composer_display_width(character.encode_utf8(&mut encoded));
        if width.saturating_add(character_width) > column {
            return index;
        }
        width = width.saturating_add(character_width);
    }
    line.len()
}

fn composer_display_width(text: &str) -> usize {
    UnicodeWidthStr::width(terminal_safe_text(text).as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pasted_input_and_prompt_history_are_bounded() {
        let mut composer = Composer::new();
        composer.insert_text(&"x".repeat(MAX_COMPOSER_BYTES + 100));
        assert_eq!(composer.text().len(), MAX_COMPOSER_BYTES);
        composer.submit().expect("pasted prompt should submit");

        for index in 0..MAX_HISTORY_ENTRIES + 20 {
            composer.insert_text(&format!("prompt {index}"));
            composer.submit().expect("history prompt should submit");
        }

        assert!(composer.history.len() <= MAX_HISTORY_ENTRIES);
        assert!(composer.history_bytes <= MAX_HISTORY_BYTES);
    }

    #[test]
    fn edits_happen_at_unicode_character_boundaries() {
        let mut composer = Composer::new();
        composer.insert_text("aéb");
        composer.move_left();
        composer.insert_character('X');
        assert_eq!(composer.text(), "aéXb");

        composer.backspace();
        composer.delete();
        assert_eq!(composer.text(), "aé");
        assert_eq!(composer.cursor(), "aé".len());
    }

    #[test]
    fn vertical_movement_keeps_a_stable_preferred_column() {
        let mut composer = Composer::new();
        composer.insert_text("abc\nx\n12345");

        composer.move_vertical(-1);
        assert_eq!(composer.cursor(), "abc\nx".len());
        composer.move_vertical(-1);
        assert_eq!(composer.cursor(), "abc".len());
        composer.move_vertical(1);
        assert_eq!(composer.cursor(), "abc\nx".len());
        composer.move_vertical(1);
        assert_eq!(composer.cursor(), "abc\nx\n12345".len());

        composer.move_home();
        assert_eq!(composer.cursor(), "abc\nx\n".len());
        composer.move_end();
        assert_eq!(composer.cursor(), composer.text().len());
    }

    #[test]
    fn multiline_paste_line_endings_are_normalized() {
        let mut composer = Composer::new();
        composer.insert_text("one\r\ntwo\rthree");

        assert_eq!(composer.text(), "one\ntwo\nthree");
        assert_eq!(composer.line_count(), 3);
    }

    #[test]
    fn history_restores_the_draft_after_navigation() {
        let mut composer = Composer::new();
        for prompt in ["first", "second"] {
            composer.insert_text(prompt);
            composer.submit().expect("prompt should submit");
        }
        composer.insert_text("draft");

        composer.previous_history();
        assert_eq!(composer.text(), "second");
        composer.previous_history();
        assert_eq!(composer.text(), "first");
        composer.next_history();
        assert_eq!(composer.text(), "second");
        composer.next_history();
        assert_eq!(composer.text(), "draft");
        assert_eq!(composer.cursor(), "draft".len());
    }
}
