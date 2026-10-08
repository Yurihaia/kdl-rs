// TODO: MAKE SURE TO REMOVE THIS AFTER DEV
#![allow(unused)]
#![allow(unreachable_pub)]

use std::{fmt, iter, mem, ops::Range, slice};

use miette::{Severity, SourceSpan};
use winnow::{
    Parser,
    combinator::{alt, cut_err, delimited, dispatch, eof, fail, opt, repeat, trace},
    error::{FromRecoverableError, Needed, ParserError},
    stream::{Accumulate, ContainsToken, Location, Offset, Recover, Stream, StreamIsPartial},
    token::{any, one_of},
};

use crate::{
    KdlDocument, KdlEntry, KdlIdentifier, KdlNode, KdlNodeFormat, KdlValue,
    parser::{
        KdlParseError, PError, PResult, SubtokenParse, TextLocation, TraceExt, cursor::Cursor, cx,
        failure_from_errs,
    },
};

// IMPORTANT
// token offsets and text offsets are two very different things
// these type aliases exist to clarify which `usize`s are which.
//
// TokenOffset is the nth token in the token stream
// TextOffset is the nth byte in the source text
//
// TokenOffsets are not useful outside of this source file.

type TokenOffset = usize;
type TextOffset = usize;

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum TokenKind {
    True,
    False,
    Null,

    IdentString,
    QuotedString,
    MultiString,
    RawQuotedString,
    RawMultiString,

    HexNumber,
    OctNumber,
    BinNumber,
    DecNumber,
    Inf,
    NegInf,
    Nan,

    LParen,
    RParen,
    LCurly,
    RCurly,

    Equals,
    Semicolon,

    SingleComment,
    MultiComment,
    Slashdash,

    Space,
    Newline,
    Escline,
    Bom,

    Unknown,
}

impl fmt::Display for TokenKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "tk::{:?}", *self)
    }
}

use TokenKind::*;

#[derive(Copy, Clone, PartialEq, Eq, Hash)]
pub struct Token<'src> {
    pub kind: TokenKind,
    pub error: bool,
    pub src: &'src str,
}

impl<'src> Token<'src> {
    pub fn new(kind: TokenKind, text: &'src str, span: Range<TextOffset>) -> Self {
        Self {
            kind,
            src: &text[span],
            error: false,
        }
    }
}

impl<'src> fmt::Display for Token<'src> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.src)
    }
}

impl<'src> fmt::Debug for Token<'src> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.src)
    }
}

impl<'src> Parser<TokenStream<'src>, Token<'src>, PError> for TokenKind {
    fn parse_next(&mut self, input: &mut TokenStream<'src>) -> PResult<Token<'src>> {
        trace(*self, |input: &mut TokenStream<'src>| {
            input
                .next_token()
                .filter(|t| t.kind == *self)
                .ok_or_else(|| ParserError::from_input(input))
        })
        .parse_next(input)
        // input
        //     .next_token()
        //     .filter(|t| t.kind == *self)
        //     .ok_or_else(|| ParserError::from_input(input))
    }
}

impl<'src> PartialEq<TokenKind> for Token<'src> {
    fn eq(&self, other: &TokenKind) -> bool {
        self.kind == *other
    }
}
impl<'src> PartialEq<Token<'src>> for TokenKind {
    fn eq(&self, other: &Token<'src>) -> bool {
        *self == other.kind
    }
}
impl<'src, const N: usize> ContainsToken<Token<'src>> for [TokenKind; N] {
    fn contains_token(&self, token: Token<'src>) -> bool {
        self.contains(&token.kind)
    }
}

// Essentially winnow's TokenSlice, but with the `Location` impl changed.
// The offsets in `Stream` generally refer to a static token offset,
// and not the variable text offset. To get a text offset, use
// `Location` or `SpanFromCheckpoint`.
#[derive(Clone, Debug)]
pub struct TokenStream<'src> {
    slice: &'src [Token<'src>],
    initial: &'src [Token<'src>],
    text: &'src str,
    errors: Vec<KdlParseError>,
}

#[derive(Copy, Clone, Debug)]
pub struct Checkpoint<'src>(&'src [Token<'src>]);

impl<'src> Offset for Checkpoint<'src> {
    fn offset_from(&self, start: &Self) -> TokenOffset {
        self.0.offset_from(&start.0)
    }
}

impl<'src> TokenStream<'src> {
    fn new(tokens: &'src [Token<'src>], text: &'src str) -> Self {
        Self {
            slice: tokens,
            initial: tokens,
            text,
            errors: Vec::new(),
        }
    }

    fn token_slice_to_str(&self, subslice: &'src [Token<'src>]) -> &'src str {
        if subslice.is_empty() {
            // Sort of awkward. Find the index where the empty subslice should be,
            // then find the string location associated with it
            let offset = subslice.offset_from(&self.initial);
            // If the subslice is the EOF subslice, return the EOF string subslice.
            // otherwise, return the empty string from the next token's src.
            self.initial
                .get(offset)
                .map(|s| &s.src[..0])
                .unwrap_or_else(|| &self.text[self.text.len()..])
        } else {
            let start = subslice.first().unwrap().src.offset_from(&self.text);
            let end = {
                let last_src = subslice.last().unwrap().src;
                last_src.offset_from(&self.text) + last_src.len()
            };
            &self.text[start..end]
        }
    }
}

impl<'src> TextLocation for TokenStream<'src> {
    fn span_from_checkpoint(&self, checkpoint: &Self::Checkpoint) -> Range<usize> {
        if checkpoint.0.is_empty() {
            return self.text.len()..self.text.len();
        }
        let start = checkpoint.0.first().unwrap().src.offset_from(&self.text);
        let end = {
            let last_src = checkpoint.0.last().unwrap().src;
            last_src.offset_from(&self.text) + last_src.len()
        };
        start..end
    }

    fn location_of(&self, token: &Self::Token) -> usize {
        token.src.offset_from(&self.text)
    }
}

impl<'src> Stream for TokenStream<'src> {
    type Token = Token<'src>;

    type Slice = &'src str;

    type IterOffsets = iter::Enumerate<iter::Copied<slice::Iter<'src, Self::Token>>>;

    type Checkpoint = Checkpoint<'src>;

    fn iter_offsets(&self) -> Self::IterOffsets {
        self.slice.iter().copied().enumerate()
    }

    fn eof_offset(&self) -> TokenOffset {
        self.slice.eof_offset()
    }

    fn next_token(&mut self) -> Option<Self::Token> {
        self.slice.next_token()
    }

    fn peek_token(&self) -> Option<Self::Token> {
        self.slice.peek_token()
    }

    fn offset_for<P>(&self, predicate: P) -> Option<TokenOffset>
    where
        P: Fn(Self::Token) -> bool,
    {
        self.slice.offset_for(predicate)
    }

    fn offset_at(&self, tokens: TokenOffset) -> Result<TokenOffset, Needed> {
        self.slice.offset_at(tokens)
    }

    fn next_slice(&mut self, offset: TokenOffset) -> Self::Slice {
        let slice = self.slice.next_slice(offset);
        self.token_slice_to_str(slice)
    }

    fn peek_slice(&self, offset: TokenOffset) -> Self::Slice {
        self.token_slice_to_str(self.slice.peek_slice(offset))
    }

    fn checkpoint(&self) -> Self::Checkpoint {
        Checkpoint(self.slice)
    }

    fn reset(&mut self, checkpoint: &Self::Checkpoint) {
        self.slice = checkpoint.0;
    }

    fn trace(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.slice.trace(f)
    }
}

impl<'src, E: ParserError<Self>> Recover<E> for TokenStream<'src>
where
    KdlParseError: FromRecoverableError<Self, E>,
{
    fn record_err(
        &mut self,
        token_start: &Self::Checkpoint,
        err_start: &Self::Checkpoint,
        mut err: E,
    ) -> Result<(), E> {
        self.errors.push(KdlParseError::from_recoverable_error(
            token_start,
            err_start,
            &*self,
            err,
        ));
        Ok(())
    }

    fn is_recovery_supported() -> bool {
        true
    }
}

impl<'src> StreamIsPartial for TokenStream<'src> {
    type PartialState = ();

    fn complete(&mut self) -> Self::PartialState {}

    fn restore_partial(&mut self, _: Self::PartialState) {}

    fn is_partial_supported() -> bool {
        false
    }
}

impl<'src> Location for TokenStream<'src> {
    fn previous_token_end(&self) -> TextOffset {
        // Tokens are always flush with eachother
        self.current_token_start()
    }

    fn current_token_start(&self) -> TextOffset {
        if let Some(token_text) = self.slice.first() {
            token_text.src.offset_from(&self.text)
        } else {
            self.text.len()
        }
    }
}

impl<'src> Offset<Checkpoint<'src>> for TokenStream<'src> {
    fn offset_from(&self, start: &Checkpoint<'_>) -> TokenOffset {
        self.slice.offset_from(&start.0)
    }
}

// TODO: Don't love the repetition here, but concat can't join two constants.
static NEWLINES: &str = "\u{000D}\u{000A}\u{0085}\u{000B}\u{000C}\u{2028}\u{2029}";

static SPACES: &str = concat!(
    "\u{0009}\u{0020}\u{00A0}\u{1680}\u{2000}\u{2001}\u{2002}\u{2003}",
    "\u{2004}\u{2005}\u{2006}\u{2007}\u{2008}\u{2009}\u{200A}\u{202F}",
    "\u{205F}\u{3000}",
);

static NEWLINES_AND_SPACES: &str = concat!(
    "\u{0009}\u{0020}\u{00A0}\u{1680}\u{2000}\u{2001}\u{2002}\u{2003}",
    "\u{2004}\u{2005}\u{2006}\u{2007}\u{2008}\u{2009}\u{200A}\u{202F}",
    "\u{205F}\u{3000}",
    "\u{000D}\u{000A}\u{0085}\u{000B}\u{000C}\u{2028}\u{2029}",
);

fn peek_kind(input: &mut TokenStream<'_>) -> PResult<TokenKind> {
    input
        .peek_token()
        .map(|v| v.kind)
        .ok_or_else(|| ParserError::from_input(input))
}

pub struct KdlDocumentAcc {
    result: KdlDocument,
    trailing: String,
}

impl KdlDocumentAcc {
    fn finish(mut self) -> KdlDocument {
        if self.result.nodes.is_empty() {
            self.result.format_mut().unwrap().leading = self.trailing;
            self.result
        } else {
            self.result.format_mut().unwrap().trailing = self.trailing;
            self.result
        }
    }
}

impl<'src> Accumulate<AccEntry<'src, KdlNode>> for KdlDocumentAcc {
    fn initial(capacity: Option<usize>) -> Self {
        Self {
            result: KdlDocument {
                nodes: capacity.map(Vec::with_capacity).unwrap_or_default(),
                format: Some(Default::default()),
                #[cfg(feature = "span")]
                span: 0.into(),
            },
            trailing: String::new(),
        }
    }

    fn accumulate(&mut self, acc: AccEntry<'src, KdlNode>) {
        match acc.kind {
            AccKind::Normal(mut node) => {
                // Put the acquired whitespace to the start of the
                // new node before adding it to the document.
                let fmt = node.format_mut().unwrap();
                self.trailing.push_str(&fmt.leading);
                fmt.leading = mem::take(&mut self.trailing);
                self.result.nodes.push(node);
            }
            AccKind::Slashdash(_) | AccKind::Whitespace => {
                self.trailing.push_str(acc.text);
            }
        }
    }
}

pub fn document(input: &mut TokenStream<'_>) -> PResult<KdlDocument> {
    let _start = input.checkpoint();
    let bom = opt(Bom).parse_next(input)?;
    let mut doc = nodes.map(KdlDocumentAcc::finish).parse_next(input)?;
    #[cfg(feature = "span")]
    {
        doc.span = input.span_from_checkpoint(&_start).into();
    }
    if let Some((fmt, bom)) = doc.format_mut().zip(bom) {
        fmt.leading = format!("{bom}{}", fmt.leading);
    }
    Ok(doc)
}

#[test]
fn idk() -> miette::Result<()> {
//     let meow = r###"
//     """
//     hello
//     meow
//         wah
// xd    """
// "###;
//     let val =
//         parse_multiline_string(meow.trim()).map_err(|errs| failure_from_errs(errs, meow.trim()))?;
//     println!("{}", val);

    let meow = r###"
    ##"""hello\"r"""""#
"###;
    let val =
        parse_raw_quoted_string(meow.trim()).map_err(|errs| failure_from_errs(errs, meow.trim()))?;
    println!("{}", val);

    return Ok(());
    // todo!();

    let text = r#""aa\u{}\u{q}\u{D800}\u{1234567}\ { test; test }"#;
    let mut start = 0;
    let mut tk = |kind: TokenKind, len: usize| {
        let span = start..(start + len);
        start += len;
        Token::new(kind, text, span)
    };
    let tokens = &[
        tk(QuotedString, 32),
        tk(Space, 1),
        tk(LCurly, 1),
        tk(Space, 1),
        tk(IdentString, 4),
        tk(Semicolon, 1),
        tk(Space, 1),
        tk(IdentString, 4),
        tk(Space, 1),
        tk(RCurly, 1),
    ];
    let mut stream = TokenStream::new(tokens, text);
    let document = match document.parse_next(&mut stream) {
        Ok(d) => d,
        Err(e) => {
            return Err(failure_from_errs(stream.errors, text).into());
        }
    };
    println!("{:?}", document.nodes[0].name.value());
    println!("\ndocument:\n{}\n\noriginal:\n{}\n", document, text);

    Ok(())
}

#[derive(Debug)]
pub struct AccEntry<'src, T> {
    span: Range<TextOffset>,
    text: &'src str,
    kind: AccKind<T>,
}

impl<'src, T> AccEntry<'src, T> {
    pub fn new_normal(text: &'src str, span: Range<TextOffset>, value: T) -> Self {
        Self {
            text,
            span,
            kind: AccKind::Normal(value),
        }
    }

    pub fn new_slashdash(text: &'src str, span: Range<TextOffset>, value: T) -> Self {
        Self {
            text,
            span,
            kind: AccKind::Slashdash(value),
        }
    }

    pub fn new_whitespace(text: &'src str, span: Range<TextOffset>) -> Self {
        Self {
            text,
            span,
            kind: AccKind::Whitespace,
        }
    }
}

#[derive(Debug)]
pub enum AccKind<T> {
    Normal(T),
    Slashdash(T),
    Whitespace,
}

impl<'src, T> From<((AccKind<T>, Range<TextOffset>), &'src str)> for AccEntry<'src, T> {
    fn from(((kind, span), text): ((AccKind<T>, Range<TextOffset>), &'src str)) -> Self {
        AccEntry { span, text, kind }
    }
}

pub fn nodes<'src, A>(input: &mut TokenStream<'src>) -> PResult<A>
where
    A: Accumulate<AccEntry<'src, KdlNode>>,
{
    // This function is evil but everything is awkward otherwise.
    let mut acc = A::initial(None);

    /// Normal inter-node whitespace
    let mut ws = opt(line_spaces)
        .map(|_| AccKind::<KdlNode>::Whitespace)
        .with_span()
        .with_taken()
        .map(AccEntry::from);
    // A base node followed by maybe pre-terminator whitespace.
    // The whitespace may be treated as trailing whitespace if
    // there is no terminator to follow.
    let mut nd = (
        alt((
            base_node
                .with_span()
                .with_taken()
                .map(|((v, s), t)| (v, false, s, t)),
            slashdashed(base_node)
                .with_span()
                .with_taken()
                .map(|((v, s), t)| (v, true, s, t)),
        )),
        node_space0.span().with_taken(),
    );

    // Parse and commit an initial whitespace span
    acc.accumulate(ws.parse_next(input)?);
    // Now parse a node. Do not commit it until checks if there is a following terminator
    let mut checkpoint = input.checkpoint();
    let (mut node, mut trail) = nd.parse_next(input)?;
    loop {
        if let Some(term) = opt(node_terminator).parse_next(input)? {
            let fmt = node.0.format_mut().unwrap();
            fmt.before_terminator.push_str(trail.1);
            fmt.terminator = term.into();
            let span = input.span_from_checkpoint(&checkpoint);
            let text = &input.text[span.clone()];
            #[cfg(feature = "span")]
            {
                node.0.span = span.clone().into();
            }
            acc.accumulate(if node.1 {
                AccEntry::new_slashdash(text, span, node.0)
            } else {
                AccEntry::new_normal(text, span, node.0)
            });
        } else {
            // No terminator was parsed, so end the node iteration.
            // This trailing whitespace should be sent to the accumulator
            // instead of the parsed node.
            acc.accumulate(if node.1 {
                AccEntry::new_slashdash(node.3, node.2, node.0)
            } else {
                AccEntry::new_normal(node.3, node.2, node.0)
            });
            acc.accumulate(AccEntry::new_whitespace(trail.1, trail.0));
            break;
        }

        // A node + terminator pair was just parsed and committed.
        // All that remains is try to parse new lines spaces + a new node.
        acc.accumulate(ws.parse_next(input)?);
        // TODO: maybe instead of opt'ing this, actually check for }/eof?
        if let Some((new_node, new_trail)) = opt(nd.by_ref()).parse_next(input)? {
            node = new_node;
            trail = new_trail;
        } else {
            break;
        }
    }

    // TODO: Maybe parse another set of trailing whitespace? It will only matter when
    // breaking due to not terminator, but if that happens, it *should* get eaten up
    // by the node-spaces + terminator.

    Ok(acc)
}

/// A node up until the terminator. Should not be called by anything else
/// because the span and terminator data (which are logically part of the node)
/// are not actually filled in yet.
fn base_node(input: &mut TokenStream<'_>) -> PResult<KdlNode> {
    trace("base-node", |input: &mut TokenStream<'_>| {
        let (before_ty_name, ty, after_ty_name) = opt(annotation)
            .parse_next(input)?
            .map(|(b, i, a)| (b, Some(i), a))
            .unwrap_or_default();
        let after_ty = node_space0.take().parse_next(input)?;
        let name = identifier.parse_next(input)?;
        let (entries, after_entries) = entries.map(KdlEntriesAcc::finish).parse_next(input)?;

        let (before_children, children, after_children) =
            children.map(KdlChildrenAcc::finish).parse_next(input)?;

        Ok(KdlNode {
            ty,
            name,
            entries,
            children,
            format: Some(KdlNodeFormat {
                leading: String::new(),
                before_ty_name: before_ty_name.into(),
                after_ty_name: after_ty_name.into(),
                after_ty: after_ty.into(),
                before_children: format!("{}{}", after_entries, before_children),
                before_terminator: after_children,
                terminator: String::new(),
                trailing: String::new(),
            }),
            #[cfg(feature = "span")]
            span: (0..0).into(),
        })
    })
    .parse_next(input)
}

fn node_terminator<'src>(input: &mut TokenStream<'src>) -> PResult<&'src str> {
    alt((line_terminator.void(), Semicolon.void()))
        .take()
        .parse_next(input)
}

pub fn identifier(input: &mut TokenStream<'_>) -> PResult<KdlIdentifier> {
    string
        .with_span()
        .with_taken()
        .map(|((v, s), r)| KdlIdentifier {
            value: v,
            repr: Some(r.into()),
            span: s.into(),
        })
        .parse_next(input)
}

pub fn annotation<'src>(
    input: &mut TokenStream<'src>,
) -> PResult<(&'src str, KdlIdentifier, &'src str)> {
    delimited(
        LParen,
        cut_err((node_space0.take(), identifier, node_space0.take())),
        cut_err(RParen),
    )
    .trace("type")
    .parse_next(input)
}

pub struct KdlEntriesAcc {
    result: Vec<KdlEntry>,
    trailing: String,
}

impl KdlEntriesAcc {
    fn finish(mut self) -> (Vec<KdlEntry>, String) {
        (self.result, self.trailing)
    }
}

impl<'src> Accumulate<AccEntry<'src, KdlEntry>> for KdlEntriesAcc {
    fn initial(capacity: Option<usize>) -> Self {
        Self {
            result: Vec::new(),
            trailing: String::new(),
        }
    }

    fn accumulate(&mut self, acc: AccEntry<'src, KdlEntry>) {
        match acc.kind {
            AccKind::Normal(mut entry) => {
                // Put the acquired whitespace to the start of the
                // new entry.
                let fmt = entry.format_mut().unwrap();
                self.trailing.push_str(&fmt.leading);
                fmt.leading = mem::take(&mut self.trailing);

                self.result.push(entry);
            }
            AccKind::Slashdash(_) | AccKind::Whitespace => {
                self.trailing.push_str(acc.text);
            }
        }
    }
}

pub fn entries<'src, A>(input: &mut TokenStream<'src>) -> PResult<A>
where
    A: Accumulate<AccEntry<'src, KdlEntry>>,
{
    // There has to be a better way to do this that can handle heterogenous tuples.
    repeat(
        0..,
        alt((
            (
                node_space0
                    .map(|_| AccKind::Whitespace)
                    .with_span()
                    .with_taken(),
                slashdashed(entry)
                    .map(AccKind::Slashdash)
                    .with_span()
                    .with_taken(),
            ),
            (
                node_space1
                    .map(|_| AccKind::Whitespace)
                    .with_span()
                    .with_taken(),
                entry.map(AccKind::Normal).with_span().with_taken(),
            ),
        )),
    )
    .fold(
        || A::initial(None),
        |mut acc, (ws, node)| {
            acc.accumulate(ws.into());
            acc.accumulate(node.into());
            acc
        },
    )
    .trace("entries")
    .parse_next(input)
}

pub fn entry(input: &mut TokenStream<'_>) -> PResult<KdlEntry> {
    let mut value = KdlEntry::new(KdlValue::Null);
    value.format = Some(Default::default());
    Null.value(value).parse_next(input)
}

pub struct KdlChildrenAcc {
    leading: String,
    document: Option<KdlDocument>,
    trailing: String,
}

impl KdlChildrenAcc {
    fn finish(mut self) -> (String, Option<KdlDocument>, String) {
        (self.leading, self.document, self.trailing)
    }
}

impl<'src> Accumulate<AccEntry<'src, KdlDocument>> for KdlChildrenAcc {
    fn initial(capacity: Option<usize>) -> Self {
        Self {
            leading: String::new(),
            document: None,
            trailing: String::new(),
        }
    }

    fn accumulate(&mut self, acc: AccEntry<'src, KdlDocument>) {
        match acc.kind {
            AccKind::Normal(mut entry) => {
                self.document = Some(entry);
            }
            AccKind::Whitespace | AccKind::Slashdash(_) => {
                if self.document.is_some() {
                    self.trailing.push_str(acc.text);
                } else {
                    self.leading.push_str(acc.text);
                }
            }
        }
    }
}

pub fn children<'src, A>(input: &mut TokenStream<'src>) -> PResult<A>
where
    A: Accumulate<AccEntry<'src, KdlDocument>>,
{
    trace("children", |input: &mut TokenStream<'src>| {
        let mut slashed_children = |acc: A, input: &mut TokenStream<'src>| {
            // Annoying workaround for `init` in `fold` needing to be FnMut.
            let mut acc = Some(acc);
            repeat(
                0..,
                (
                    node_space0
                        .map(|_| AccKind::<KdlDocument>::Whitespace)
                        .with_span()
                        .with_taken(),
                    slashdashed(braced_child_nodes)
                        .map(AccKind::Slashdash)
                        .with_span()
                        .with_taken(),
                ),
            )
            .fold(
                || acc.take().unwrap(),
                |mut acc, (ws, cl)| {
                    acc.accumulate(ws.into());
                    acc.accumulate(cl.into());
                    acc
                },
            )
            .parse_next(input)
        };
        let mut acc = slashed_children(A::initial(None), input)?;

        let real_child = opt((
            node_space0
                .map(|_| AccKind::<KdlDocument>::Whitespace)
                .with_span()
                .with_taken(),
            braced_child_nodes
                .map(AccKind::Normal)
                .with_span()
                .with_taken(),
        ))
        .parse_next(input)?;

        if let Some((ws, cl)) = real_child {
            acc.accumulate(ws.into());
            acc.accumulate(cl.into());

            slashed_children(acc, input)
        } else {
            Ok(acc)
        }
    })
    .parse_next(input)
}

/// Specifically the { ... } bit. Children refers to a list of child_lists, slashdashed
/// and unslashdashed
pub fn braced_child_nodes(input: &mut TokenStream<'_>) -> PResult<KdlDocument> {
    let _start = input.checkpoint();
    LCurly.parse_next(input)?;
    let mut doc = nodes.map(KdlDocumentAcc::finish).parse_next(input)?;
    RCurly.parse_next(input)?;
    #[cfg(feature = "span")]
    {
        doc.span = input.span_from_checkpoint(&_start).into();
    }

    Ok(doc)
}

pub fn string(input: &mut TokenStream<'_>) -> PResult<String> {
    dispatch! {peek_kind;
        IdentString => any.take().map(From::from),
        QuotedString => any.take().subtoken_parse(parse_quoted_string),
        MultiString => any.take().subtoken_parse(parse_multiline_string),
        RawQuotedString => any.take().subtoken_parse(parse_raw_quoted_string),
        RawMultiString => any.take().subtoken_parse(parse_raw_multiline_string),
        _ => fail,
    }
    .parse_next(input)
}

fn parse_quoted_string(input: &str) -> Result<String, Vec<KdlParseError>> {
    let bad_eof = |_| cx().msg("TODO: Expected closing quote");
    let no_esc = |_| cx().msg("TODO: Expected escape character");

    let mut cursor = Cursor::new(input);
    let mut buffer = String::with_capacity(input.len() - 2);
    // String token would never be lexed without a leading quote
    assert!(cursor.check('"').is_ok());

    loop {
        match cursor.char().with_error(bad_eof).fail()? {
            '"' => break,
            '\\' => {
                let start = cursor.position();
                buffer.push(match cursor.char().with_error(no_esc).fail()? {
                    // Simple escapes
                    'b' => '\u{0008}',
                    'n' => '\n',
                    'f' => '\u{000C}',
                    'r' => '\r',
                    't' => '\t',
                    's' => ' ',
                    '\\' => '\\',
                    '"' => '"',
                    // Unicode escaping
                    'u' => match parse_unicode_escape(&mut cursor) {
                        Some(c) => c,
                        None => continue,
                    },
                    // Whitespace escaping
                    c if NEWLINES_AND_SPACES.contains(c) => {
                        cursor.eat_while(|c| NEWLINES_AND_SPACES.contains(c));
                        continue;
                    }
                    // Invalid escape character
                    c => {
                        cursor.error_from(start, cx().msg("TODO: Invalid escape"));
                        continue;
                    }
                })
            }
            c => buffer.push(c),
        }
    }

    cursor.into_errors()?;

    Ok(buffer)
}

fn parse_raw_quoted_string(input: &str) -> Result<String, Vec<KdlParseError>> {
    let hashless_input = input.trim_matches('#');
    let start_hashes = hashless_input.as_ptr().addr() - input.as_ptr().addr();
    let end_hashes = input.len() - hashless_input.len() - start_hashes;

    println!("{} {}", start_hashes, end_hashes);

    let mut cursor = Cursor::new(&input[..(input.len() - end_hashes - 3)]);

    if start_hashes != end_hashes {
        cursor.add_raw_error(KdlParseError::from_span_and_ctx(
            (0..start_hashes).into(),
            cx().msg("TODO: Unbalanced hashes"),
        ));
    }
    if !hashless_input.ends_with("\"\"\"") {
        return Err(vec![KdlParseError {
            span: Some((0..(start_hashes + 3)).into()),
            message: Some("TODO: Unclosed raw string".into()),
            ..Default::default()
        }]);
    }
    // Move past the hashes + 3 quotes (these always get lexed correctly)
    cursor.advance(start_hashes + 3);

    let text = cursor.text();

    cursor.into_errors()?;

    Ok(text.into())
}

// Pretty sure this is correct, but the function is really complicated
fn parse_multiline_string(input: &str) -> Result<String, Vec<KdlParseError>> {
    let no_esc = |_| cx().msg("TODO: Expected escape character");
    let no_nl = |_| cx().msg("TODO: Multiline string must start with a newline");
    let bad_dedent = |_| cx().msg("TODO: Dedent error");
    // Do the initial unterminated string check. These tokens consumes basically
    // everything in their path, so most other errors will be nonsense.
    // The ending isn't very interesting as well, so just span it to the initial 3 quotes.
    // TODO: Maybe add another diagnostic with some help showing the end?
    if !input.ends_with("\"\"\"") {
        return Err(vec![KdlParseError {
            span: Some((0..3).into()),
            message: Some("TODO: Unclosed multiline string".into()),
            ..Default::default()
        }]);
    }

    // Just strip out the final quotes from the cursor because we already checked it.
    // The same can't be done for the initial part because of error spanning.
    let mut cursor = Cursor::new(&input[..(input.len() - 3)]);
    // TODO: Maybe don't bother with capacity? These strings are likely
    // to be significantly smaller than their actual token lengths.
    let mut buffer = String::with_capacity(input.len() - 6);
    // Two double quotes would just get lexed as an empty string,
    // and given that its a valid token heuristics can't be done.
    // Hence, this token is guaranteed to start with the 3 quotes.
    cursor.advance(3);

    /// A multiline string needs to start with a newline after the quotes,
    /// but its not "fatal" if it doesn't.
    let init_nl = cursor
        .check_fn(|c| NEWLINES.contains(c))
        .with_error(no_nl)
        .pass();
    if init_nl == Ok('\r') && cursor.peek() == Some('\n') {
        cursor.eat();
    }

    // First we need to scan for the ending whitespace. If an invalid dedent is found,
    // just pretend there is no dedent so its possible to collect errors from the rest
    // of the string.
    let dedent = get_dedent_whitespace(&input[3..(input.len() - 3)]).unwrap_or_else(|mut err| {
        // An error with the initial newlines will cause a dedent error, but
        // that error is a bit more cryptic so no point in pushing it.
        if init_nl.is_err() {
            return "";
        }
        err.span = err.span.map(|v| (v.offset() + 3, v.len()).into());
        cursor.add_raw_error(err);
        ""
    });

    // The start of this loop is now in the context of a new line.
    while !cursor.eof() {
        let buffer_line_start = buffer.len();
        let mut only_whitespace = true;
        // Save the dedent error for later so it can be added only if this is not
        // a blank line
        let dedent_result = cursor.check_str(dedent).inner();

        while let Some(c) = cursor.eat() {
            match c {
                '\\' => {
                    only_whitespace = false;
                    let start = cursor.position();
                    buffer.push(match cursor.char().with_error(no_esc).fail()? {
                        // Simple escapes
                        'b' => '\u{0008}',
                        'n' => '\n',
                        'f' => '\u{000C}',
                        'r' => '\r',
                        't' => '\t',
                        's' => ' ',
                        '\\' => '\\',
                        '"' => '"',
                        // Unicode escaping
                        'u' => match parse_unicode_escape(&mut cursor) {
                            Some(c) => c,
                            None => continue,
                        },
                        // Whitespace escaping
                        c if NEWLINES_AND_SPACES.contains(c) => {
                            let res = cursor.eat_while(|c| NEWLINES_AND_SPACES.contains(c));
                            eprintln!("ough {:?}", format!("{}{}", c, res));
                            continue;
                        }
                        // Invalid escape character
                        c => {
                            cursor.error_from(start, cx().msg("TODO: Invalid escape"));
                            continue;
                        }
                    })
                }
                c if NEWLINES.contains(c) => {
                    buffer.push('\n');
                    break;
                }
                c if SPACES.contains(c) => buffer.push(c),
                c => {
                    buffer.push(c);
                    only_whitespace = false;
                }
            }
        }

        // If only whitespace were read from the line and actual data has been pushed,
        // clear the line and append a newline.
        if only_whitespace && buffer.len() > buffer_line_start {
            buffer.truncate(buffer_line_start);
            buffer.push('\n');
        } else if let Err((e, span)) = dedent_result {
            cursor.add_raw_error(KdlParseError::from_span_and_ctx(span, bad_dedent(e)));
        }
    }

    // In every valid multiline string, there is a final newline, so pop that.
    buffer.pop();

    cursor.into_errors()?;

    Ok(buffer)
}

fn parse_unicode_escape(cursor: &mut Cursor<'_>) -> Option<char> {
    // TODO: Might be useful to organize errors elsewhere?
    // They can get pretty cluttered when having good heuristics
    let no_lcur = |_| cx().msg("TODO: Missing {");
    let no_rcur = |c: Option<char>| match c {
        Some(c) if c.is_ascii_hexdigit() => {
            cx().msg("TODO: Unicode escape can only have 6 hex digits")
        }
        Some(c) => cx().msg(format_args!("TODO: '{c}' is not a hex digit")),
        None => cx().msg("TODO: Missing }"),
    };
    let bad_char = || cx().msg("TODO: Invalid char representation");
    let bad_hex = |s: &str| match s.chars().next_back() {
        Some('}') => cx().msg("TODO: Unicode escape must have at least 1 hex digit"),
        Some(c) if !c.is_ascii_hexdigit() => {
            cx().msg(format_args!("TODO: '{c}' is not a hex digit"))
        }
        _ => panic!(),
    };

    cursor.check('{').with_error(no_lcur).pass().ok()?;
    // Should this variable be inlined?
    let hex_start = cursor.position();
    let hex = cursor
        .eat_while_range(1..=6, |c| char::is_ascii_hexdigit(&c))
        .with_error(bad_hex)
        .pass()
        .ok()?;
    // While breaking here will not eat the trailing },
    // it doesn't really matter considering the string will
    // not be converted into a real value
    let hex_end = cursor.position();

    cursor.check('}').with_error(no_rcur).pass().ok()?;

    // Is this too weird?
    char::from_u32(u32::from_str_radix(hex, 16).unwrap()).or_else(|| {
        cursor.error_range(hex_start, hex_end, bad_char());
        None
    })
}

// Really convoluted function, but I'm 99% sure its *actually* spec compliant. Even the playground
// (at the time of writing) isn't totally spec compliant when dealing with some convoluted inputs.
fn get_dedent_whitespace(input: &str) -> Result<&str, KdlParseError> {
    // The input already has the quotes stripped.
    let mut chars = input[..].char_indices();

    let mut end_idx = input.len();
    let mut nl_idx = None;
    // `err_idx` is the index *after* the character the loop breaks on and can be used as an error
    // end location.
    let err_idx = loop {
        // Move to the first non-space character. If there is no remaining non-space character,
        // it means the start of the string was reached.
        let Some((idx, ch)) = chars.rfind(|v| !SPACES.contains(v.1)) else {
            break 0;
        };
        match ch {
            // This whitespace is maybe being ws escaped. We'll need to count
            // the number of consecutive backslashes to be sure.
            '\\' => {
                let mut count = 1;
                for (_, c) in chars.clone().rev() {
                    if c != '\\' {
                        break;
                    }
                    count += 1;
                }

                // If an even number of consecutive backslashes were found,
                // this is not actually a whitespace escape, and instead a bunch
                // backslashes and someone trying to limit test the parser.
                if count % 2 == 0 {
                    break idx + ch.len_utf8();
                }
                // This was a ws escape, so clear the found newline and reset the
                // end index.
                nl_idx = None;
                end_idx = idx;

                if count > 1 {
                    // This was a bunch of escapes *followed* by a ws escape. This construct
                    // is never valid, so just clear nl_idx and break from the loop.
                    // Also advance the char iterator to simulate finding the next
                    // non-special character.
                    break chars
                        .next_back()
                        .map(|(i, c)| i + c.len_utf8())
                        .unwrap_or(0);
                }
            }
            // If we found a newline, mark the important newline. Note that because this scans
            // backwards, \r\n newlines will get marked on the \n, which is correct.
            c if NEWLINES.contains(c) => {
                nl_idx = nl_idx.or(Some(idx));
            }
            // Otherwise, we found a non-whitespace character that cannot be a ws escape.
            _ => break idx + ch.len_utf8(),
        }
    };

    let Some(nl_idx) = nl_idx else {
        let line_start_idx = chars
            .rfind(|(_, c)| NEWLINES.contains(*c))
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        // No valid newline index was found, which means no dedent whitespace exists.
        return Err(KdlParseError::from_span_and_ctx(
            (line_start_idx..err_idx).into(),
            cx().msg("The last line must be only whitespace"),
        ));
    };

    Ok(&input[(nl_idx + 1)..end_idx])
}

fn parse_raw_multiline_string(input: &str) -> Result<String, Vec<KdlParseError>> {
    let no_nl = |_| cx().msg("TODO: Multiline string must start with a newline");
    let bad_dedent = |_| cx().msg("TODO: Dedent error");
    // This kinda sucks
    let hashless_input = input.trim_matches('#');
    let start_hashes = hashless_input.as_ptr().addr() - input.as_ptr().addr();
    let end_hashes = input.len() - hashless_input.len() - start_hashes;

    let mut cursor = Cursor::new(&input[..(input.len() - end_hashes - 3)]);

    if start_hashes != end_hashes {
        cursor.add_raw_error(KdlParseError::from_span_and_ctx(
            (0..start_hashes).into(),
            cx().msg("TODO: Unbalanced hashes"),
        ));
    }
    if !hashless_input.ends_with("\"\"\"") {
        return Err(vec![KdlParseError {
            span: Some((0..(start_hashes + 3)).into()),
            message: Some("TODO: Unclosed multiline string".into()),
            ..Default::default()
        }]);
    }

    let mut buffer = String::with_capacity(hashless_input.len() - 6);
    // Move past the hashes + 3 quotes (these always get lexed correctly)
    cursor.advance(start_hashes + 3);
    // Check for the starting newline
    let init_nl = cursor
        .check_fn(|c| NEWLINES.contains(c))
        .with_error(no_nl)
        .pass();
    if init_nl == Ok('\r') && cursor.peek() == Some('\n') {
        cursor.eat();
    }

    let dedent = get_raw_dedent_whitespace(&hashless_input[3..(hashless_input.len() - 3)])
        .unwrap_or_else(|mut err| {
            // An error with the initial newlines will cause a dedent error, but
            // that error is a bit more cryptic so no point in pushing it.
            if init_nl.is_err() {
                return "";
            }
            err.span = err
                .span
                .map(|v| (v.offset() + start_hashes + 3, v.len()).into());
            cursor.add_raw_error(err);
            ""
        });

    while !cursor.eof() {
        let buffer_line_start = buffer.len();
        let mut only_whitespace = true;
        let dedent_result = cursor.check_str(dedent).inner();

        while let Some(c) = cursor.eat() {
            if NEWLINES.contains(c) {
                buffer.push('\n');
                break;
            } else if !SPACES.contains(c) {
                only_whitespace = false;
            }
            buffer.push(c);
        }

        if only_whitespace && buffer.len() > buffer_line_start {
            buffer.truncate(buffer_line_start);
            buffer.push('\n');
        } else if let Err((e, span)) = dedent_result {
            cursor.add_raw_error(KdlParseError::from_span_and_ctx(span, bad_dedent(e)));
        }
    }

    buffer.pop();
    cursor.into_errors()?;

    Ok(buffer)
}

// Much simpler
fn get_raw_dedent_whitespace(input: &str) -> Result<&str, KdlParseError> {
    // The input already has the quotes stripped.
    let mut chars = input[..].char_indices();

    let (idx, c) = chars.rfind(|v| !SPACES.contains(v.1)).unwrap_or((0, '\0'));
    if !NEWLINES.contains(c) {
        let line_start_idx = chars
            .rfind(|(_, c)| NEWLINES.contains(*c))
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);

        return Err(KdlParseError::from_span_and_ctx(
            (line_start_idx..(idx + c.len_utf8())).into(),
            cx().msg("The last line must be only whitespace"),
        ));
    }

    Ok(&input[(idx + c.len_utf8())..])
}

pub fn line_spaces(input: &mut TokenStream<'_>) -> PResult<()> {
    repeat(1.., alt((line_terminator, node_space))).parse_next(input)
}

pub fn line_terminator(input: &mut TokenStream<'_>) -> PResult<()> {
    one_of([Newline, SingleComment]).void().parse_next(input)
}

pub fn node_space(input: &mut TokenStream<'_>) -> PResult<()> {
    alt(((wsp, opt(escline)).void(), escline)).parse_next(input)
}

pub fn node_space1(input: &mut TokenStream<'_>) -> PResult<()> {
    repeat(1.., node_space).parse_next(input)
}

pub fn node_space0(input: &mut TokenStream<'_>) -> PResult<()> {
    repeat(0.., node_space).parse_next(input)
}

pub fn ws(input: &mut TokenStream<'_>) -> PResult<()> {
    one_of([Space, MultiComment]).void().parse_next(input)
}

pub fn wss(input: &mut TokenStream<'_>) -> PResult<()> {
    repeat(0.., ws).map(|()| ()).parse_next(input)
}

pub fn wsp(input: &mut TokenStream<'_>) -> PResult<()> {
    repeat(1.., ws).map(|()| ()).parse_next(input)
}

pub fn escline(input: &mut TokenStream<'_>) -> PResult<()> {
    (
        Escline,
        cut_err((wss, alt((line_terminator, eof.void())), wss)),
    )
        .void()
        .trace("escline")
        .parse_next(input)
}

pub fn slashdashed<O>(
    mut parser: impl for<'src> Parser<TokenStream<'src>, O, PError>,
) -> impl for<'src> Parser<TokenStream<'src>, O, PError> {
    move |input: &mut TokenStream<'_>| {
        (Slashdash, opt(line_spaces), parser.by_ref())
            .trace("slashdash")
            .parse_next(input)
            .map(|(_, _, o)| o)
    }
}
