//! 带光标的单行输入框:行内编辑和 pop 框字段共用。

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

/// 带光标的单行输入:支持左右/Home/End 移动、任意位置插入、前后删除。
#[derive(Default)]
pub(super) struct Input {
    pub(super) buf: String,
    /// 光标位置(字符下标,不是字节)。
    pub(super) pos: usize,
}

impl Input {
    pub(super) fn with_str(s: &str) -> Self {
        Self {
            buf: s.to_string(),
            pos: s.chars().count(),
        }
    }

    pub(super) fn chars(&self) -> usize {
        self.buf.chars().count()
    }

    /// 光标的字节偏移。
    pub(super) fn byte_pos(&self) -> usize {
        self.buf
            .char_indices()
            .nth(self.pos)
            .map(|(i, _)| i)
            .unwrap_or(self.buf.len())
    }

    pub(super) fn left(&mut self) {
        self.pos = self.pos.saturating_sub(1);
    }

    pub(super) fn right(&mut self) {
        if self.pos < self.chars() {
            self.pos += 1;
        }
    }

    pub(super) fn home(&mut self) {
        self.pos = 0;
    }

    pub(super) fn end(&mut self) {
        self.pos = self.chars();
    }

    pub(super) fn insert(&mut self, c: char) {
        let b = self.byte_pos();
        self.buf.insert(b, c);
        self.pos += 1;
    }

    /// 删光标前的字符(Backspace)。
    pub(super) fn backspace(&mut self) {
        if self.pos == 0 {
            return;
        }
        let b = self.byte_pos();
        let prev = self.buf[..b]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
        self.buf.replace_range(prev..b, "");
        self.pos -= 1;
    }

    /// 删光标处的字符(Delete)。
    pub(super) fn delete(&mut self) {
        let b = self.byte_pos();
        let Some(c) = self.buf[b..].chars().next() else {
            return;
        };
        self.buf.replace_range(b..b + c.len_utf8(), "");
    }

    /// 渲染为 spans。`cursor` 时光标处字符用反显(粗块光标)包住;光标在末尾时
    /// 画一个反显空格块。
    pub(super) fn spans(&self, cursor: bool, base: Style) -> Vec<Span<'static>> {
        if !cursor {
            return vec![Span::styled(self.buf.clone(), base)];
        }
        let b = self.byte_pos();
        let mut spans = vec![Span::styled(self.buf[..b].to_string(), base)];
        match self.buf[b..].chars().next() {
            Some(c) => {
                spans.push(Span::styled(
                    c.to_string(),
                    base.add_modifier(Modifier::REVERSED)
                        .add_modifier(Modifier::BOLD),
                ));
                spans.push(Span::styled(self.buf[b + c.len_utf8()..].to_string(), base));
            }
            None => spans.push(Span::styled(
                " ",
                base.bg(Color::LightBlue).add_modifier(Modifier::BOLD),
            )),
        }
        spans
    }
}

#[cfg(test)]
mod tests {
    use super::{Input, Style};

    #[test]
    fn insert_at_cursor_and_move() {
        let mut i = Input::default();
        for c in "abc".chars() {
            i.insert(c);
        }
        assert_eq!(i.buf, "abc");
        i.left();
        i.insert('X');
        assert_eq!(i.buf, "abXc");
        assert_eq!(i.pos, 3);
    }

    #[test]
    fn left_at_start_is_noop_and_backspace_removes_before_cursor() {
        let mut i = Input::with_str("ab");
        i.left();
        i.left();
        i.left();
        assert_eq!(i.pos, 0);
        i.insert('X');
        assert_eq!(i.buf, "Xab");
        i.backspace();
        assert_eq!(i.buf, "ab");
        assert_eq!(i.pos, 0);
    }

    #[test]
    fn delete_removes_at_cursor() {
        let mut i = Input::with_str("abc");
        i.left();
        i.delete(); // 删掉 c
        assert_eq!(i.buf, "ab");
        i.left();
        i.delete(); // 删掉 b
        assert_eq!(i.buf, "a");
        assert_eq!(i.pos, 1);
        i.delete(); // 光标在末尾,no-op
        assert_eq!(i.buf, "a");
    }

    #[test]
    fn home_end_and_multibyte() {
        let mut i = Input::with_str("中文ab");
        i.home();
        i.right();
        i.insert('X');
        assert_eq!(i.buf, "中X文ab");
        i.end();
        assert_eq!(i.pos, i.chars());
        i.backspace();
        assert_eq!(i.buf, "中X文a");
        // 块状光标:反显包住光标处字符;光标在末尾时是色块占位。
        // 不在编辑态时只有整段文本。
        let spans = i.spans(true, Style::default());
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content, "中X文a");
        assert_eq!(spans[1].content, " ");
        let plain = i.spans(false, Style::default());
        assert_eq!(plain.len(), 1);
        assert_eq!(plain[0].content, "中X文a");
    }
}
