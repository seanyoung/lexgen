#![allow(clippy::should_implement_trait, clippy::type_complexity)]

use unicode_width::UnicodeWidthChar;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexerError<E> {
    pub location: Loc,
    pub kind: LexerErrorKind<E>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LexerErrorKind<E> {
    /// Lexer error, raised by lexgen-generated code
    InvalidToken,

    /// Custom error, raised by a semantic action
    Custom(E),
}

/// A location, used in errors
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Loc {
    pub line: u32,
    pub col: u32,
    pub byte_idx: usize,
}

impl Loc {
    const ZERO: Loc = Loc {
        line: 0,
        col: 0,
        byte_idx: 0,
    };
}

/// **Do not use**
// Possible outcomes of user action
pub enum SemanticActionResult<T> {
    // User action did not return a token, continue with lexing
    Continue,
    // User action returned a token, return it
    Return(T),
}

impl<T> SemanticActionResult<T> {
    pub fn map_token<F, T1>(self, f: F) -> SemanticActionResult<T1>
    where
        F: Fn(T) -> T1,
    {
        match self {
            SemanticActionResult::Continue => SemanticActionResult::Continue,
            SemanticActionResult::Return(t) => SemanticActionResult::Return(f(t)),
        }
    }
}

/// Common parts in lexers generated by lexgen.
///
/// **Fields are used by lexgen-generated code and should not be used directly.**
pub struct Lexer<'input, Token, State, Error, Wrapper> {
    // Current lexer state
    pub __state: usize,

    // Set after end-of-input is handled by a rule, or by default in `Init` rule
    pub __done: bool,

    // Which lexer state to switch to on successful match
    pub __initial_state: usize,

    user_state: State,

    // User-provided input string. Does not change after initialization.
    input: &'input str,

    // Start location of `iter`. We update this as we backtrack and update `iter`.
    iter_loc: Loc,

    // Character iterator. `Peekable` is used in the handler's `peek` method. Note that we can't
    // use byte index returned by this directly, as we re-initialize this field when backtracking.
    // Add `iter_byte_idx` to the byte index before using. When resetting, update `iter_byte_idx`.
    iter: std::iter::Peekable<std::str::Chars<'input>>,

    // Start of the current match
    current_match_start: Loc,

    // End of the current match
    current_match_end: Loc,

    // If we skipped an accepting state, this holds the triple:
    //
    // - Skipped match start (byte index in `input`)
    // - Semantic action (a function name)
    // - Skipped match end (exclusive, byte index in `input`)
    last_match: Option<(
        Loc,
        for<'lexer> fn(&'lexer mut Wrapper) -> SemanticActionResult<Result<Token, Error>>,
        Loc,
    )>,
}

impl<'input, T, S: Default, E, W> Lexer<'input, T, S, E, W> {
    pub fn new(input: &'input str) -> Self {
        Self::new_with_state(input, Default::default())
    }
}

impl<'input, T, S, E, W> Lexer<'input, T, S, E, W> {
    pub fn new_with_state(input: &'input str, state: S) -> Self {
        Self {
            __state: 0,
            __done: false,
            __initial_state: 0,
            user_state: state,
            input,
            iter_loc: Loc::ZERO,
            iter: input.chars().peekable(),
            current_match_start: Loc::ZERO,
            current_match_end: Loc::ZERO,
            last_match: None,
        }
    }

    // Read the next chracter
    pub fn next(&mut self) -> Option<char> {
        match self.iter.next() {
            None => None,
            Some(char) => {
                self.current_match_end.byte_idx += char.len_utf8();
                if char == '\n' {
                    self.current_match_end.line += 1;
                    self.current_match_end.col = 0;
                } else if char == '\t' {
                    self.current_match_end.col += 4; // TODO: Make this configurable?
                } else {
                    self.current_match_end.col += UnicodeWidthChar::width(char).unwrap_or(1) as u32;
                }
                Some(char)
            }
        }
    }

    pub fn peek(&mut self) -> Option<char> {
        self.iter.peek().copied()
    }

    // On success returns semantic action function for the last match
    pub fn backtrack(
        &mut self,
    ) -> Result<for<'lexer> fn(&'lexer mut W) -> SemanticActionResult<Result<T, E>>, LexerError<E>>
    {
        match self.last_match.take() {
            None => Err(LexerError {
                location: self.current_match_start,
                kind: LexerErrorKind::InvalidToken,
            }),
            Some((match_start, semantic_action, match_end)) => {
                self.__done = false;
                self.current_match_start = match_start;
                self.current_match_end = match_end;
                self.iter = self.input[match_end.byte_idx..].chars().peekable();
                self.iter_loc = match_end;
                Ok(semantic_action)
            }
        }
    }

    pub fn reset_accepting_state(&mut self) {
        self.last_match = None;
    }

    pub fn set_accepting_state(
        &mut self,
        semantic_action_fn: for<'lexer> fn(&'lexer mut W) -> SemanticActionResult<Result<T, E>>,
    ) {
        self.last_match = Some((
            self.current_match_start,
            semantic_action_fn,
            self.current_match_end,
        ));
    }

    pub fn reset_match(&mut self) {
        self.current_match_start = self.current_match_end;
    }

    pub fn match_(&self) -> &'input str {
        &self.input[self.current_match_start.byte_idx..self.current_match_end.byte_idx]
    }

    pub fn match_loc(&self) -> (Loc, Loc) {
        (self.current_match_start, self.current_match_end)
    }

    pub fn state(&mut self) -> &mut S {
        &mut self.user_state
    }
}
