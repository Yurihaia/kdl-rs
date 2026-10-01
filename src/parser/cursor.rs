use std::str::Chars;

#[derive(Debug, Clone)]
pub(crate) struct Cursor<'t> {
    chars: Chars<'t>,
    src: &'t str,
}

impl<'t> Cursor<'t> {
    pub(crate) fn new(src: &'t str) -> Self {
        Self {
            chars: src.chars(),
            src,
        }
    }

    pub(crate) fn expect_str(&mut self, s: &str) -> Option<()> {
        if self.text().starts_with(s) {
            self.chars = self.text()[s.len()..].chars();
            Some(())
        } else {
            None
        }
    }

    pub(crate) fn expect(&mut self, c: char) -> Option<()> {
        if self.peek().is_some_and(|v| v == c) {
            self.eat();
            Some(())
        } else {
            None
        }
    }

    pub(crate) fn text(&self) -> &'t str {
        self.chars.as_str()
    }

    pub(crate) fn peek(&self) -> Option<char> {
        self.chars.clone().next()
    }

    pub(crate) fn eat(&mut self) -> Option<char> {
        self.chars.next()
    }

    pub(crate) fn eof(&self) -> bool {
        self.text().is_empty()
    }

    pub(crate) fn offset(&self) -> usize {
        let start = self.src.as_ptr().addr();
        let current = self.text().as_ptr().addr();
        // Make sure the result actually makes sense.
        debug_assert!(
            current
                .checked_sub(start)
                .is_some_and(|v| v <= self.src.len())
        );

        current - start
    }

    pub(crate) fn eof_slice(&self) -> &'t str {
        &self.src[self.src.len()..self.src.len()]
    }

    pub(crate) fn eat_while(&mut self, mut f: impl FnMut(char) -> bool) {
        while self.peek().is_some_and(&mut f) {
            self.eat();
        }
    }

    pub(crate) fn eat_until_char(&mut self, c: char) {
        let str = self.text();
        self.chars = str
            .find(c)
            .map(|idx| &str[idx..])
            .unwrap_or_else(|| self.eof_slice())
            .chars()
    }
}
