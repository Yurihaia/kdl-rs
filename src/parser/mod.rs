#![allow(unused)]

use std::{
    fmt::Display, num::{ParseFloatError, ParseIntError}, ops::Range,
};

use miette::{Severity, SourceSpan};
use winnow::{
    LocatingSlice, Parser, error::{AddContext, ErrMode, FromExternalError, FromRecoverableError, ParserError}, stream::{Location, Recoverable, Stream},
};

pub(crate) mod cursor;
pub(crate) mod kdl_v2;

type Input<'a> = Recoverable<LocatingSlice<&'a str>, ErrMode<KdlParseError>>;
type PResult<T> = Result<T, PError>;
type PError = ErrMode<KdlParseError>;

// More useful version of `Location`. Parser inputs over token streams
// have trouble interacting with offsets, so spanning needs to solely
// be done via external means. `Location` is the default but it doesn't
// allow an easy way to find a span in this manner. This is a somewhat
// awkward issue that winnow has with token streams.

/// Utilities for a parse stream which has a specific notion of "Text Offset/Location".
/// This bridges the gap between a parser that has an input of a sequence of tokens,
/// and a "Slice" that is the string text.
trait TextLocation: Stream {
    /// Return a text span from `start` to the current position of this stream.
    fn span_from_checkpoint(&self, start: &Self::Checkpoint) -> Range<usize>;
    /// Returns the text offset of a specific token.
    fn location_of(&self, token: &Self::Token) -> usize;
}

trait TraceExt<I: Stream, O, E: ParserError<I>>: Parser<I, O, E> + Sized {
    #[inline(always)]
    fn trace(self, name: impl Display) -> impl Parser<I, O, E> {
        winnow::combinator::trace(name, self)
    }
}

impl<I: Stream, O, E: ParserError<I>, P: Parser<I, O, E>> TraceExt<I, O, E> for P {}

#[derive(Debug, Clone, Default, Eq, PartialEq)]
struct KdlParseContext {
    message: Option<String>,
    label: Option<String>,
    help: Option<String>,
    severity: Option<Severity>,
}

impl KdlParseContext {
    fn msg(mut self, txt: impl AsRef<str>) -> Self {
        self.message = Some(txt.as_ref().to_string());
        self
    }

    fn lbl(mut self, txt: impl AsRef<str>) -> Self {
        self.label = Some(txt.as_ref().to_string());
        self
    }

    fn hlp(mut self, txt: impl AsRef<str>) -> Self {
        self.help = Some(txt.as_ref().to_string());
        self
    }

    fn sev(mut self, severity: Severity) -> Self {
        self.severity = Some(severity);
        self
    }
}

fn cx() -> KdlParseContext {
    Default::default()
}

#[derive(Debug, Default, Clone, Eq, PartialEq)]
pub(crate) struct KdlParseError {
    pub(crate) message: Option<String>,
    pub(crate) span: Option<SourceSpan>,
    pub(crate) label: Option<String>,
    pub(crate) help: Option<String>,
    pub(crate) severity: Option<Severity>,
}

impl<I: Stream> ParserError<I> for KdlParseError {
    type Inner = Self;
    fn from_input(_input: &I) -> Self {
        Self {
            message: None,
            span: None,
            label: None,
            help: None,
            severity: None,
        }
    }

    fn append(self, _input: &I, _token_start: &<I as Stream>::Checkpoint) -> Self {
        self
    }

    fn into_inner(self) -> Result<Self, Self> {
        Ok(self)
    }
}

impl<I: Stream> AddContext<I, KdlParseContext> for KdlParseError {
    fn add_context(
        mut self,
        _input: &I,
        _token_start: &<I as Stream>::Checkpoint,
        ctx: KdlParseContext,
    ) -> Self {
        self.message = ctx.message.or(self.message);
        self.label = ctx.label.or(self.label);
        self.help = ctx.help.or(self.help);
        self.severity = ctx.severity.or(self.severity);
        self
    }
}

impl<'a> FromExternalError<Input<'a>, ParseIntError> for KdlParseError {
    fn from_external_error(_: &Input<'a>, e: ParseIntError) -> Self {
        Self {
            span: None,
            message: Some(format!("{e}")),
            label: Some("invalid integer".into()),
            help: None,
            severity: Some(Severity::Error),
        }
    }
}

impl<'a> FromExternalError<Input<'a>, ParseFloatError> for KdlParseError {
    fn from_external_error(_input: &Input<'a>, e: ParseFloatError) -> Self {
        Self {
            span: None,
            label: Some("invalid float".into()),
            help: None,
            message: Some(format!("{e}")),
            severity: Some(Severity::Error),
        }
    }
}

struct NegativeUnsignedError;

impl<'a> FromExternalError<Input<'a>, NegativeUnsignedError> for KdlParseError {
    fn from_external_error(_input: &Input<'a>, _e: NegativeUnsignedError) -> Self {
        Self {
            span: None,
            message: Some("Tried to parse a negative number as an unsigned integer".into()),
            label: Some("negative unsigned int".into()),
            help: None,
            severity: Some(Severity::Error),
        }
    }
}

impl<I: Stream + TextLocation> FromRecoverableError<I, Self> for KdlParseError {
    #[inline]
    fn from_recoverable_error(
        token_start: &<I as Stream>::Checkpoint,
        _err_start: &<I as Stream>::Checkpoint,
        input: &I,
        mut e: Self,
    ) -> Self {
        e.span = e
            .span
            .or_else(|| Some(input.span_from_checkpoint(token_start).into()));
        e
    }
}
