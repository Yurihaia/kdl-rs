// TODO: MAKE SURE TO REMOVE THIS AFTER DEV
#![allow(unused)]
#![allow(unreachable_pub)]

use std::{
    cell::Cell,
    fmt, iter, mem,
    num::{ParseFloatError, ParseIntError},
    ops::Range,
    slice,
    str::Chars,
};

use miette::{Severity, SourceSpan};
use winnow::{
    LocatingSlice, Parser, RecoverableParser,
    ascii::escaped,
    combinator::{
        alt, cut_err, delimited, dispatch, empty, eof, fail, iterator, not, opt, peek, repeat,
        separated, seq, todo, trace,
    },
    error::{
        AddContext, ContextError, ErrMode, FromExternalError, FromRecoverableError, Needed,
        ParserError,
    },
    stream::{
        Accumulate, AsChar, ContainsToken, Location, Offset, Recoverable, Stream, StreamIsPartial,
    },
    token::{any, literal, none_of, one_of, take_while},
};

use crate::{
    KdlDocument, KdlEntry, KdlIdentifier, KdlNode, KdlNodeFormat, KdlValue,
    parser::{KdlParseError, PError, PResult, TextLocation, TraceExt, cursor::Cursor, cx},
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
}

#[derive(Copy, Clone, Debug)]
pub struct Checkpoint<'src>(&'src [Token<'src>]);

impl<'src> Offset for Checkpoint<'src> {
    fn offset_from(&self, start: &Self) -> TokenOffset {
        self.0.offset_from(&start.0)
    }
}

impl<'src> TokenStream<'src> {
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

static NEWLINES: [&str; 8] = [
    "\u{000D}\u{000A}",
    "\u{000D}",
    "\u{000A}",
    "\u{0085}",
    "\u{000B}",
    "\u{000C}",
    "\u{2028}",
    "\u{2029}",
];

static NEWLINES_AND_SPACES: &str = concat!(
    "\u{0009}\u{0002}\u{00A0}\u{1680}\u{2000}\u{2001}\u{2002}\u{2003}",
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
    let bom = opt(Bom).parse_next(input)?;
    let (mut doc, _span) = nodes
        .map(KdlDocumentAcc::finish)
        .with_span()
        .parse_next(input)?;
    #[cfg(feature = "span")]
    {
        doc.span = _span.into();
    }
    if let Some((fmt, bom)) = doc.format_mut().zip(bom) {
        fmt.leading = format!("{bom}{}", fmt.leading);
    }
    Ok(doc)
}

#[test]
fn idk() {
    let text = "test #null /-{} {} /-{}";
    let mut start = 0;
    let mut tk = |kind: TokenKind, len: usize| {
        let span = start..(start + len);
        start += len;
        Token::new(kind, text, span)
    };
    let tokens = &[
        tk(QuotedString, 4),
        // tk(IdentString, 4),
        // tk(Space, 1),
        // tk(Null, 5),
        // tk(Space, 1),
        // tk(Slashdash, 2),
        // tk(LCurly, 1),
        // tk(RCurly, 1),
        // tk(Space, 1),
        // tk(LCurly, 1),
        // tk(RCurly, 1),
        // tk(Space, 1),
        // tk(Slashdash, 2),
        // tk(LCurly, 1),
        // tk(RCurly, 1),
    ];
    let mut stream = TokenStream {
        slice: tokens,
        initial: tokens,
        text,
    };
    let document = document.parse(stream).unwrap();
    println!("document:\n{}", document);
}

#[derive(Debug)]
pub struct AccEntry<'src, T> {
    span: Range<TextOffset>,
    text: &'src str,
    kind: AccKind<T>,
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
    repeat(
        0..,
        alt((
            base_node.map(AccKind::Normal),
            slashdashed(base_node).map(AccKind::Slashdash),
            line_spaces.map(|_| AccKind::Whitespace),
        ))
        .with_span()
        .with_taken()
        .map(AccEntry::from),
    )
    .trace("nodes")
    .parse_next(input)
}

pub fn base_node(input: &mut TokenStream<'_>) -> PResult<KdlNode> {
    trace("base-node", |input: &mut TokenStream<'_>| {
        let _start = input.checkpoint();

        let (before_ty_name, ty, after_ty_name) = opt(annotation)
            .parse_next(input)?
            .map(|(b, i, a)| (b, Some(i), a))
            .unwrap_or_default();
        let after_ty = node_space0.take().parse_next(input)?;
        let name = identifier.parse_next(input)?;
        let (entries, after_entries) = entries.map(KdlEntriesAcc::finish).parse_next(input)?;

        let (before_children, children, after_children) =
            children.map(KdlChildrenAcc::finish).parse_next(input)?;

        let before_terminator = node_space0.take().parse_next(input)?;

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
                before_terminator: before_terminator.into(),
                terminator: String::new(),
                trailing: String::new(),
            }),
            #[cfg(feature = "span")]
            span: input.span_from_checkpoint(&_start).into(),
        })
    })
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
    LCurly.parse_next(input)?;
    RCurly.parse_next(input)?;
    Ok(KdlDocument::default())
}

pub fn string(input: &mut TokenStream<'_>) -> PResult<String> {
    dispatch! {peek_kind;
        IdentString => any.take().map(From::from),
        QuotedString => cut_err(any.take().and_then(parse_quoted_string)),
        RawQuotedString => any.take().map(From::from),
        RawMultiString => any.take().map(From::from),
        _ => fail,
    }
    .parse_next(input)
}

struct OptCharAcc(String);
impl From<OptCharAcc> for String {
    fn from(value: OptCharAcc) -> Self {
        value.0
    }
}
impl Accumulate<Option<char>> for OptCharAcc {
    fn initial(capacity: Option<usize>) -> Self {
        OptCharAcc(capacity.map(String::with_capacity).unwrap_or_default())
    }

    fn accumulate(&mut self, acc: Option<char>) {
        if let Some(c) = acc {
            self.0.push(c);
        }
    }
}

fn parse_quoted_string(input: &mut &str) -> PResult<String> {
    let mut cursor = Cursor::new(input);
    cursor
        .expect('\"')
        .ok_or_else(|| ParserError::from_input(&cursor.text()))?;
    *input = cursor.text();
    // delimited(
    //     '\"',
    //     escaped(none_of('\\').map(Some), '\\', string_escape),
    //     '\"',
    // )
    // .map(OptCharAcc::into)
    // .parse_next(input)
    Ok(Default::default())
}

fn parse_multi_string(input: &mut &str) -> PResult<String> {
    delimited(
        "\"\"\"",
        escaped(none_of('\\').map(Some), '\\', string_escape),
        "\"\"\"",
    )
    .map(OptCharAcc::into)
    .parse_next(input)
}

fn string_escape(input: &mut &str) -> PResult<Option<char>> {
    dispatch! {any;
        '\\' => empty.value(Some('\\')),
        '"' => empty.value(Some('"')),
        'b' => empty.value(Some('\u{0008}')),
        'n' => empty. value(Some('\n')),
        'f' => empty.value(Some('\u{000C}')),
        'r' => empty.value(Some('\r')),
        't' => empty.value(Some('\t')),
        's' => empty.value(Some(' ')),
        'u' => ('{', take_while(1..=6, AsChar::is_hex_digit), '}')
            .verify_map(|(_, hex, _)| char::from_u32(u32::from_str_radix(hex, 16).unwrap()))
            .map(Some),
        c if NEWLINES_AND_SPACES.contains(c) => {
            take_while(0.., |c| NEWLINES_AND_SPACES.contains(c)).value(None)
        },
        // Invalid escape characters will get errored by the lexer.
        // TODO: maybe instead send the error here? needs to be
        // recoverable though.
        _ => fail,
    }
    .parse_next(input)
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
