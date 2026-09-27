//! What fits at the cursor in a script, for an editor's completion menu
//! (`csvm --inkline-mode`).
//!
//! [`find`] reads the script's text up to the cursor once, with the
//! parser's own rules for quotes, comments, stages, `join ( … )` groups
//! and `fn … { }` bodies, and tells what kind of word fits there. The
//! script need not parse: a half-typed one gets a place too.

use std::ops::Range;

use crate::parse::{
    CommandWord, bracket_kind, command_word, is_ident, split_first_word, strip_comments,
    strip_comments_noting, take_token,
};

/// What fits at the cursor in a script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Place {
    /// The first word of a stage: commands and the script's `fn` names.
    StageStart,
    /// A flag of this command (`colour` is named `color`).
    Flag(String),
    /// A column.
    Column,
    /// An expression: columns and functions.
    Expression,
    /// Nothing fits: a string, a number, a comment, a count, a colour, a
    /// new name, or no word yet right after a closing backtick, quote or
    /// bracket.
    Nothing,
}

/// Where the cursor's stage is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Group {
    /// The pipeline itself.
    Top,
    /// A `join ( … )` group, with the file named right after its `)`, when
    /// the line has it.
    Join { file: Option<String> },
    /// An `fn … { }` body.
    FnBody,
}

/// What [`find`] finds at the cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// What kind of word fits there.
    pub place: Place,
    /// The byte range of the word the cursor is in (maybe empty), with any
    /// `-`/`--` before a flag and the backticks of a quoted name.
    pub word: Range<usize>,
    /// The run of stages the cursor is in.
    pub group: Group,
    /// The text of the stages before the cursor's one in its group, joined
    /// with ` | `, for working out the columns there. At the top and in a
    /// `join` group, when there are such stages and the script starts with
    /// `fn` definitions, those definitions come first, so a fragment call
    /// among the stages can be expanded. Comments are left out.
    pub stages_before: String,
    /// The names of the `fn` definitions in the script, before the cursor,
    /// each once.
    pub fns: Vec<String>,
}

/// What fits at byte `at` of `script` (at most its length; an offset
/// inside a character counts as that character's start). One pass over the
/// text before `at`, then a look ahead for the end of the word and, inside
/// a `join` group, for the file after the group. Never panics, whatever the
/// script.
pub fn find(script: &str, at: usize) -> Found {
    let mut at = at.min(script.len());
    while !script.is_char_boundary(at) {
        at -= 1;
    }
    let mut in_comment = false;
    let text = strip_comments_noting(&script[..at], |range| in_comment = range.end == at);
    let mut scan = Scan::new(&text, script.as_bytes());
    scan.run();

    let (mut word, head_end) = match scan.quote {
        Some((b'`', open)) => (open..backtick_end(script, at), open),
        _ => {
            let core = core_word(script, at);
            (core.clone(), core.start)
        }
    };
    let (stages, in_bracket) = scan.innermost();
    let head = text.get(stages.start..head_end).unwrap_or("");
    let place = match scan.quote {
        _ if in_comment => Place::Nothing,
        Some((b'`', _)) => {
            let head = Head::read(head);
            // A quoted name is never a command.
            match head.command() {
                Some(_) => place_of(&head, in_bracket, false),
                None => Place::Nothing,
            }
        }
        Some(_) => Place::Nothing,
        // An item here would be glued onto the name, string or bracket
        // that just closed.
        None if word.is_empty() && after_closing_mark(script, at) => Place::Nothing,
        None => {
            let head = Head::read(head);
            match head.flag_start() {
                Some(len) if flag_place(&head) => {
                    word.start -= len;
                    Place::Flag(head.command().unwrap_or_default().to_string())
                }
                _ => {
                    let number = script
                        .as_bytes()
                        .get(word.start)
                        .is_some_and(u8::is_ascii_digit);
                    place_of(&head, in_bracket, number)
                }
            }
        }
    };
    let group = match stages.holder {
        Holder::Top => Group::Top,
        Holder::FnBody => Group::FnBody,
        Holder::Join => Group::Join {
            file: scan.join_file(script, at, in_comment),
        },
    };
    let stages_before = scan.stages_before();
    Found {
        place,
        word,
        group,
        stages_before,
        fns: scan.fns,
    }
}

/// Whether `b` is part of a word: an ASCII letter, digit or `_`.
fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Whether the byte before `at` closes a name, a string or a bracket: a
/// `` ` ``, `"`, `'`, `)`, `]` or `}`. Only called outside a quote, where
/// a quote mark before the cursor is always a closing one.
fn after_closing_mark(script: &str, at: usize) -> bool {
    at.checked_sub(1)
        .and_then(|i| script.as_bytes().get(i))
        .is_some_and(|b| b"`\"')]}".contains(b))
}

/// The run of word bytes around `at`.
fn core_word(script: &str, at: usize) -> Range<usize> {
    let bytes = script.as_bytes();
    let start = bytes[..at]
        .iter()
        .rposition(|&b| !is_word_byte(b))
        .map_or(0, |i| i + 1);
    let end = bytes[at..]
        .iter()
        .position(|&b| !is_word_byte(b))
        .map_or(bytes.len(), |i| at + i);
    start..end
}

/// The end of a backticked name the cursor is in: just past its closing
/// backtick on the same line, else the end of the word bytes at `at`.
fn backtick_end(script: &str, at: usize) -> usize {
    let rest = &script.as_bytes()[at..];
    match rest.iter().position(|&b| b == b'`' || b == b'\n') {
        Some(i) if rest[i] == b'`' => at + i + 1,
        _ => core_word(script, at).end,
    }
}

// --- the scan ---------------------------------------------------------------

/// What holds a run of stages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Holder {
    Top,
    Join,
    FnBody,
}

/// A run of stages: the whole script, a `join` group or an `fn` body.
#[derive(Debug)]
struct Stages {
    holder: Holder,
    /// The byte ranges of the stages before the current one.
    done: Vec<Range<usize>>,
    /// Where the current stage starts.
    start: usize,
    /// What the current stage's first word makes of a bracket, read at its
    /// first bracket.
    command: Option<CommandWord>,
}

impl Stages {
    fn new(holder: Holder, start: usize) -> Stages {
        Stages {
            holder,
            done: Vec::new(),
            start,
            command: None,
        }
    }

    /// A new stage starts at `start`.
    fn restart(&mut self, start: usize) {
        self.start = start;
        self.command = None;
    }
}

/// The brackets left open, innermost last, each with its kind (see
/// `bracket_kind`) and what it holds.
struct Nest<T> {
    open: Vec<(usize, T)>,
    /// Indices into `open`, one stack per kind, each oldest first.
    by_kind: [Vec<usize>; 3],
}

impl<T> Nest<T> {
    fn new() -> Nest<T> {
        Nest {
            open: Vec::new(),
            by_kind: [Vec::new(), Vec::new(), Vec::new()],
        }
    }

    fn open(&mut self, kind: usize, value: T) {
        self.by_kind[kind].push(self.open.len());
        self.open.push((kind, value));
    }

    /// Closes the innermost open bracket of `kind`, and any left open
    /// inside it; a closer with nothing of its kind open is passed over.
    fn close(&mut self, kind: usize) -> Option<T> {
        let at = self.by_kind[kind].pop()?;
        for stack in &mut self.by_kind {
            while stack.last().is_some_and(|&i| i > at) {
                stack.pop();
            }
        }
        self.open.truncate(at + 1);
        self.open.pop().map(|(_, value)| value)
    }
}

/// A left-to-right scan of the comment-free text before the cursor.
struct Scan<'s> {
    text: &'s str,
    /// The whole script, to see the byte after the text.
    script: &'s [u8],
    /// The whole script's stages.
    top: Stages,
    /// Each open bracket: `Some` for a `join` group or an `fn` body.
    nest: Nest<Option<Stages>>,
    /// The quote mark of the string the scan is in, and where it opened.
    quote: Option<(u8, usize)>,
    fns: Vec<String>,
    /// Where the script's leading `fn` definitions end, if it has any.
    defs_end: Option<usize>,
}

impl<'s> Scan<'s> {
    fn new(text: &'s str, script: &'s [u8]) -> Scan<'s> {
        Scan {
            text,
            script,
            top: Stages::new(Holder::Top, 0),
            nest: Nest::new(),
            quote: None,
            fns: Vec::new(),
            defs_end: None,
        }
    }

    fn run(&mut self) {
        let bytes = self.text.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            let c = bytes[i];
            i += 1;
            match self.quote {
                Some((q, _)) => {
                    if c == q {
                        self.quote = None;
                    }
                }
                None => match c {
                    b'\'' | b'"' | b'`' => self.quote = Some((c, i - 1)),
                    b'|' if self.script.get(i) == Some(&b'|') => i += 1,
                    b'|' => {
                        if let Some(stages) = self.stages_mut() {
                            let start = stages.start;
                            stages.done.push(start..i - 1);
                            stages.restart(i);
                        }
                    }
                    b'(' | b'[' | b'{' => self.open(c, i - 1),
                    b')' | b']' | b'}' => self.close(c, i),
                    _ => {}
                },
            }
        }
    }

    /// The stages the scan is directly in, when it is not in a plain bracket.
    fn stages_mut(&mut self) -> Option<&mut Stages> {
        match self.nest.open.last_mut() {
            None => Some(&mut self.top),
            Some((_, stages)) => stages.as_mut(),
        }
    }

    /// The bracket `c` at byte `at` opens a `join` group, an `fn` body, or
    /// a plain bracket.
    fn open(&mut self, c: u8, at: usize) {
        let text = self.text;
        let at_top = self.nest.open.is_empty();
        let holder = self.stages_mut().and_then(|stages| {
            let from = stages.start;
            let word = *stages
                .command
                .get_or_insert_with(|| command_word(text[from..at].trim_start()));
            match (c, word) {
                (b'(', CommandWord::Join) => Some(Holder::Join),
                (b'{', CommandWord::Fn) => Some(Holder::FnBody),
                _ => None,
            }
        });
        if holder == Some(Holder::FnBody) && at_top {
            let head = text[self.top.start..at].trim_start();
            let (_, rest) = split_first_word(head);
            let name = rest.split('(').next().unwrap_or("").trim();
            if is_ident(name) && !self.fns.iter().any(|f| f == name) {
                self.fns.push(name.to_string());
            }
        }
        let stages = holder.map(|holder| Stages::new(holder, at + 1));
        self.nest.open(bracket_kind(c), stages);
    }

    /// The closing bracket `c`, ending before byte `after`, closes the
    /// innermost bracket of its kind. After an `fn` body a new stage
    /// starts, as the parser reads on after the body's `}`.
    fn close(&mut self, c: u8, after: usize) {
        let closed = self.nest.close(bracket_kind(c));
        if let Some(Some(Stages {
            holder: Holder::FnBody,
            ..
        })) = closed
        {
            if self.nest.open.is_empty() && self.top.done.is_empty() {
                self.defs_end = Some(after);
            }
            if let Some(stages) = self.stages_mut() {
                stages.restart(after);
            }
        }
    }

    /// The stages the cursor is in, and whether it is inside a plain
    /// bracket within them.
    fn innermost(&self) -> (&Stages, bool) {
        let mut in_bracket = false;
        for (_, stages) in self.nest.open.iter().rev() {
            match stages {
                Some(stages) => return (stages, in_bracket),
                None => in_bracket = true,
            }
        }
        (&self.top, in_bracket)
    }

    /// The text of the stages before the cursor's in its group.
    fn stages_before(&self) -> String {
        let (stages, _) = self.innermost();
        let before: Vec<&str> = stages
            .done
            .iter()
            .map(|range| self.text[range.clone()].trim())
            .filter(|stage| !stage.is_empty())
            .collect();
        let joined = before.join(" | ");
        match self.defs_end {
            Some(end) if !joined.is_empty() && stages.holder != Holder::FnBody => {
                format!("{}\n{joined}", self.text[..end].trim())
            }
            _ => joined,
        }
    }

    /// The file named right after the `)` of the `join` group the cursor is
    /// in, reading on from byte `at` of `script`: `None` when the group is
    /// not closed or no file follows it.
    fn join_file(&self, script: &str, at: usize, in_comment: bool) -> Option<String> {
        let rest = &script[at..];
        let skip = if in_comment {
            rest.find('\n')?
        } else if let Some((q, _)) = self.quote {
            rest.bytes().position(|b| b == q)? + 1
        } else {
            0
        };
        let rest = strip_comments(&rest[skip..]);
        // The brackets open from the group itself inward.
        let from = self
            .nest
            .open
            .iter()
            .rposition(|(_, stages)| stages.is_some())?;
        let mut nest = Nest::new();
        for &(kind, _) in &self.nest.open[from..] {
            nest.open(kind, ());
        }
        let mut quote = None;
        for (i, c) in rest.bytes().enumerate() {
            match quote {
                Some(q) if c == q => quote = None,
                Some(_) => {}
                None => match c {
                    b'\'' | b'"' | b'`' => quote = Some(c),
                    b'(' | b'[' | b'{' => nest.open(bracket_kind(c), ()),
                    b')' | b']' | b'}' => {
                        nest.close(bracket_kind(c));
                        if nest.open.is_empty() {
                            return file_after_group(&rest[i + 1..]);
                        }
                    }
                    _ => {}
                },
            }
        }
        None
    }
}

/// The file at the start of `after`, the text after a `join` group's `)`.
fn file_after_group(after: &str) -> Option<String> {
    let after = after.trim_start();
    if after.is_empty() || after.starts_with(['|', ',', ')', ']', '}']) {
        return None;
    }
    let (file, _) = take_token(after);
    // An unquoted file ends at a `,`, where the next item starts; a path
    // with a comma in it is quoted.
    let file = if after.starts_with(['\'', '"', '`']) {
        file
    } else {
        file.split(',').next().unwrap_or("")
    };
    (!file.is_empty() && file != "on").then(|| file.to_string())
}

// --- the stage's words before the cursor ------------------------------------

/// One word of a stage.
struct Word<'s> {
    text: &'s str,
    /// A `,` separates it from the word before.
    after_comma: bool,
}

/// The words of the cursor's stage before the word the cursor is in.
struct Head<'s> {
    /// The words that end before it, the command first.
    words: Vec<Word<'s>>,
    /// The part of the cursor's word before the cursor's run of word
    /// characters (`a=` in `rename a=b`, `(` in `select (a`).
    glued: &'s str,
    /// `glued` comes right after a blank.
    glued_after_blank: bool,
    /// A `,` separates the cursor's word from the word before.
    after_comma: bool,
}

impl<'s> Head<'s> {
    /// Splits `head` at blanks and commas outside quotes and brackets.
    fn read(head: &'s str) -> Head<'s> {
        let mut words = Vec::new();
        let mut start: Option<usize> = None;
        let mut comma = false;
        let mut word_comma = false;
        let mut quote: Option<char> = None;
        let mut depth = 0usize;
        let mut after_blank = true;
        let mut glued_after_blank = true;
        for (i, c) in head.char_indices() {
            if quote.is_none() && depth == 0 && (c == ',' || c.is_whitespace()) {
                if let Some(from) = start.take() {
                    words.push(Word {
                        text: &head[from..i],
                        after_comma: word_comma,
                    });
                }
                comma |= c == ',';
                after_blank = c.is_whitespace();
                continue;
            }
            if start.is_none() {
                start = Some(i);
                word_comma = std::mem::take(&mut comma);
                glued_after_blank = after_blank;
            }
            match quote {
                Some(q) if c == q => quote = None,
                Some(_) => {}
                None => match c {
                    '\'' | '"' | '`' => quote = Some(c),
                    '(' | '[' | '{' => depth += 1,
                    ')' | ']' | '}' => depth = depth.saturating_sub(1),
                    _ => {}
                },
            }
        }
        match start {
            Some(from) => Head {
                words,
                glued: &head[from..],
                glued_after_blank,
                after_comma: word_comma,
            },
            None => Head {
                words,
                glued: "",
                glued_after_blank: after_blank,
                after_comma: comma,
            },
        }
    }

    /// The stage's command, an alias named as its command (`colour` as
    /// `color`).
    fn command(&self) -> Option<&'s str> {
        self.words
            .first()
            .map(|w| crate::help::find_command(w.text).map_or(w.text, |c| c.name))
    }

    /// The words after the command.
    fn args(&self) -> &[Word<'s>] {
        self.words.get(1..).unwrap_or_default()
    }

    /// The length of the flag text before the cursor's word characters,
    /// when the word starts after a blank and reads as a flag being typed:
    /// its dashes (`-`, `--`), or a long flag up to a `-` inside its name
    /// (`--color-` in `--color-b`).
    fn flag_start(&self) -> Option<usize> {
        let glued = self.glued;
        let flag = glued.starts_with('-') && glued.bytes().all(|b| b == b'-' || is_word_byte(b));
        (self.glued_after_blank && flag).then_some(glued.len())
    }
}

/// The flags of `command` that take the word after them as their value.
fn value_flags(command: &str) -> &'static [&'static str] {
    match command {
        "head" | "tail" => &["-n", "--lines"],
        "fmt" => &["-p", "--precision"],
        "join" => &["-L", "--lsuffix", "-R", "--rsuffix"],
        "graph" => &[
            "-W",
            "--width",
            "-H",
            "--height",
            "-s",
            "--scale",
            "-x",
            "--xrange",
            "-y",
            "--yrange",
            "--xlabel",
            "--ylabel",
            "-b",
            "--bins",
            "-t",
            "--title",
            "-r",
            "--ramp",
            "-c",
            "--color-by",
        ],
        _ => &[],
    }
}

/// Whether a word is a flag: a `-` and more.
fn is_flag(word: &str) -> bool {
    word.starts_with('-') && word != "-"
}

/// Reads `words` as flags and their values: how many of them lead the
/// list, and whether the last of those is a flag still waiting for its
/// value. `graph` takes flags anywhere, so there every word is read.
fn read_flags(command: &str, words: &[Word]) -> (usize, Option<&'static str>) {
    let takes = value_flags(command);
    let mut waiting = None;
    for (n, word) in words.iter().enumerate() {
        if waiting.take().is_some() {
            continue;
        }
        if is_flag(word.text) {
            waiting = takes.iter().copied().find(|f| *f == word.text);
        } else if command != "graph" {
            return (n, None);
        }
    }
    (words.len(), waiting)
}

/// Whether the cursor's dash word is in its command's flag place: after
/// its other flags (anywhere for `graph`), not a flag's value.
fn flag_place(head: &Head) -> bool {
    let Some(command) = head.command() else {
        return false;
    };
    let args = head.args();
    let (flags, waiting) = read_flags(command, args);
    flags == args.len() && waiting.is_none()
}

/// What fits for a word that is not a flag. `number`: the word starts with
/// a digit, outside backticks.
fn place_of(head: &Head, in_bracket: bool, number: bool) -> Place {
    let Some(command) = head.command() else {
        return if head.glued.is_empty() {
            Place::StageStart
        } else {
            Place::Nothing
        };
    };
    if number {
        return Place::Nothing;
    }
    let args = head.args();
    match command {
        "select" => Place::Expression,
        "add" if has_eq(args, head.glued) => Place::Expression,
        "cols" | "uniq" | "stats" => Place::Column,
        "sort" | "rename" if after_eq(head) => Place::Nothing,
        "sort" | "rename" => Place::Column,
        "color" => color_place(head),
        "agg" if in_bracket || args.iter().any(|w| w.text == "by") => Place::Column,
        "graph" => graph_place(head),
        // The name after a key's `=` is a column of the file joined, not of
        // the stream.
        "join" if after_eq(head) => Place::Nothing,
        "join" => join_place(head),
        _ => Place::Nothing,
    }
}

/// Whether an `=` outside quotes comes in `words` or `glued`.
fn has_eq(words: &[Word], glued: &str) -> bool {
    words.iter().map(|w| w.text).chain([glued]).any(|text| {
        let mut quote = None;
        text.chars().any(|c| match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
                false
            }
            None if matches!(c, '\'' | '"' | '`') => {
                quote = Some(c);
                false
            }
            None => c == '=',
        })
    })
}

/// Whether the cursor's word is after the `=` of a `NAME=VALUE` item: in
/// the same item, or after a word ending in `=` (`a = b` is one item).
fn after_eq(head: &Head) -> bool {
    head.glued.contains('=')
        || (head.glued.is_empty()
            && !head.after_comma
            && head.args().last().is_some_and(|w| w.text.ends_with('=')))
}

/// `color COLOUR EXPR`, `color -c COL COLOUR EXPR` or
/// `color -g COLS [RAMP] [LO HI]`.
fn color_place(head: &Head) -> Place {
    let args = head.args();
    let (flag, positionals) = match args.first().map(|w| w.text) {
        Some(flag @ ("-c" | "-g")) => (flag, &args[1..]),
        _ => ("", args),
    };
    let n = positionals.len();
    match flag {
        // Columns until a ramp (it has a `:`) or a bound (a number).
        "-g" => {
            let past_columns = positionals
                .iter()
                .any(|w| w.text.contains(':') || w.text.parse::<f64>().is_ok());
            if past_columns || head.glued.contains(':') {
                Place::Nothing
            } else {
                Place::Column
            }
        }
        "-c" => match n {
            0 => Place::Column,
            1 => Place::Nothing,
            _ => Place::Expression,
        },
        _ if n == 0 => Place::Nothing,
        _ => Place::Expression,
    }
}

/// `graph [KIND] COLS [FLAGS]`: a column, but for a flag's value, of which
/// only `-c`'s is a column.
fn graph_place(head: &Head) -> Place {
    let (_, waiting) = read_flags("graph", head.args());
    let flag = match waiting {
        Some(flag) => Some(flag),
        None if is_flag(head.glued) => head.glued.split_once('=').map(|(flag, _)| flag),
        None => None,
    };
    match flag {
        Some("-c" | "--color-by") | None => Place::Column,
        Some(_) => Place::Nothing,
    }
}

/// `join [FLAGS] ITEM[, ITEM...]`, each `ITEM` being `[(SUB)] FILE [on
/// KEYS]`: a column after an `on`, and in a later item with no group and
/// no `on`, which the parser reads as more keys.
fn join_place(head: &Head) -> Place {
    let args = head.args();
    let (flags, _) = read_flags("join", args);
    let mut on_before = false;
    let mut on = false;
    let mut first: Option<&str> = None;
    for word in &args[flags..] {
        if word.after_comma {
            on_before |= on;
            on = false;
            first = None;
        }
        first.get_or_insert(word.text);
        on |= word.text == "on";
    }
    if head.after_comma {
        on_before |= on;
        on = false;
        first = None;
    }
    let group = first.unwrap_or(head.glued).starts_with('(');
    if on || (on_before && !group) {
        Place::Column
    } else {
        Place::Nothing
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(marked: &str) -> Found {
        let at = marked.find('@').expect("a cursor mark");
        find(&marked.replacen('@', "", 1), at)
    }

    #[test]
    fn the_first_word_of_a_stage_is_a_command() {
        for s in [
            "@",
            "so@",
            "sort id | h@",
            "sort id |\n  @",
            "fn f(n) { he@ }",
            "join (so@ ) b.csv",
        ] {
            assert_eq!(at(s).place, Place::StageStart, "{s}");
        }
        assert_eq!(at("sort id | h@").word, 10..11);
    }

    #[test]
    fn a_dash_word_after_a_command_is_a_flag() {
        assert_eq!(at("fmt -@").place, Place::Flag("fmt".into()));
        assert_eq!(at("fmt -s --st@").place, Place::Flag("fmt".into()));
        assert_eq!(at("fmt --st@").word, 4..8);
    }

    #[test]
    fn columns_go_where_commands_take_them() {
        for s in [
            "sort am@",
            "sort id am@",
            "cols a,b@",
            "uniq @",
            "stats x@",
            "rename a@=b",
            "agg sum(am@) by r",
            "agg sum(a) by re@",
            "color -g am@",
            "color -c am@ red x > 1",
            "graph hist am@",
            "join b.csv on i@",
            "sort `first na@`",
        ] {
            assert_eq!(at(s).place, Place::Column, "{s}");
        }
        assert_eq!(at("sort `first na@`").word, 5..15);
    }

    #[test]
    fn expressions_take_columns_and_functions() {
        for s in [
            "select am@",
            "select a > 1 && le@",
            "add x = am@",
            "color red am@",
            "color -c a red b@",
        ] {
            assert_eq!(at(s).place, Place::Expression, "{s}");
        }
    }

    #[test]
    fn nothing_fits_in_strings_numbers_counts_and_new_names() {
        for s in [
            "select a == 'x@'",
            "select a == \"x@\"",
            "head 1@",
            "select a > 1@",
            "add ne@ = 1",
            "color re@ a > 1",
            "rename a=b@",
            "sort a=n@",
            "# a comment@",
            "sort a # so@",
        ] {
            assert_eq!(at(s).place, Place::Nothing, "{s}");
        }
    }

    #[test]
    fn nothing_fits_right_after_a_closing_mark() {
        for s in [
            "cols id,`first name`@",
            "select len(amount)@",
            "select id == \"x\"@",
            "select id == 'x'@",
            "select [a]@",
            "fn f() { uniq }@",
            "sort `a`@ | head",
        ] {
            assert_eq!(at(s).place, Place::Nothing, "{s}");
        }
        // A word after the mark, or a space, still gets its place.
        assert_eq!(at("select len(amount)a@").place, Place::Expression);
        assert_eq!(at("cols id,`first name` @").place, Place::Column);
    }

    #[test]
    fn a_half_typed_script_still_finds_its_place() {
        assert_eq!(at("select (a > 1 && le@").place, Place::Expression);
        assert_eq!(at("sort a | @").place, Place::StageStart);
        assert_eq!(at("join (sort i@").place, Place::Column);
        assert_eq!(at("select a == 'x@").place, Place::Nothing);
    }

    #[test]
    fn the_stages_before_and_the_group_are_found() {
        let f = at("rename a=b | cols -v c | sort @");
        assert_eq!(f.group, Group::Top);
        assert_eq!(f.stages_before.replace(' ', ""), "renamea=b|cols-vc");
        let f = at("join (rename a=b | sort @) right.csv on a");
        assert_eq!(
            f.group,
            Group::Join {
                file: Some("right.csv".into())
            }
        );
        assert_eq!(f.stages_before.trim(), "rename a=b");
        assert_eq!(at("fn f(n) { sort @ }").group, Group::FnBody);
    }

    #[test]
    fn every_value_flag_is_a_flag_of_its_command() {
        for help in crate::help::COMMANDS {
            for flag in value_flags(help.name) {
                assert!(
                    help.flags.iter().any(|f| f.names.contains(flag)),
                    "{} {flag}",
                    help.name
                );
            }
        }
    }

    #[test]
    fn flags_follow_each_commands_grammar() {
        // A flag's value is not a flag place.
        assert_eq!(at("fmt -p 2 -@").place, Place::Flag("fmt".into()));
        assert_eq!(at("join -L _x -@").place, Place::Flag("join".into()));
        assert_eq!(at("join -L -@").place, Place::Nothing);
        // graph takes flags after its columns too.
        assert_eq!(at("graph hist a -@").place, Place::Flag("graph".into()));
        assert_eq!(at("graph hist a -@").word, 13..14);
        assert_eq!(at("colour -@").place, Place::Flag("color".into()));
        // After a positional the dash is not a flag, and not in the word.
        let f = at("select a > -b@");
        assert_eq!((f.place, f.word), (Place::Expression, 12..13));
        assert_eq!(at("color -c a -@").place, Place::Nothing);
    }

    #[test]
    fn graph_color_join_and_agg_details() {
        assert_eq!(at("graph -c am@").place, Place::Column);
        assert_eq!(at("graph hist a -c=am@").place, Place::Column);
        assert_eq!(at("graph hist a -t ti@").place, Place::Nothing);
        assert_eq!(at("graph @").place, Place::Column);
        assert_eq!(at("color -g a b@").place, Place::Column);
        assert_eq!(at("color -g a green:r@").place, Place::Nothing);
        assert_eq!(at("color -g a green:red x@").place, Place::Nothing);
        assert_eq!(at("join a.csv on k, j@").place, Place::Column);
        assert_eq!(at("join a.csv, b@").place, Place::Nothing);
        assert_eq!(at("join a.csv on k, (sort x) b@").place, Place::Nothing);
        assert_eq!(at("join (sort x) b.csv on k@").place, Place::Column);
        // The right key after `=` names a column of the joined file.
        assert_eq!(at("join b.csv on id=cu@").place, Place::Nothing);
        assert_eq!(at("join b.csv on id = @").place, Place::Nothing);
        assert_eq!(at("join b.csv on id=cu, @").place, Place::Column);
        // `--color-by`, typed up to a `-` inside its name, is a flag.
        let f = at("graph line x y --color-b@");
        assert_eq!((f.place, f.word), (Place::Flag("graph".into()), 15..24));
        assert_eq!(at("graph line x y --color-@").word, 15..23);
        assert_eq!(at("graph line x y --color-by @").place, Place::Column);
        assert_eq!(at("graph line x y --color-by=am@").place, Place::Column);
        // A flag's value with a `-` in it is not a flag.
        assert_eq!(at("graph line x y -t=my-ti@").place, Place::Nothing);
        assert_eq!(at("agg cou@").place, Place::Nothing);
        assert_eq!(at("agg total=sum(am@").place, Place::Column);
        assert_eq!(at("sort a = n@").place, Place::Nothing);
        assert_eq!(at("rename a=b c@").place, Place::Column);
        assert_eq!(at("add `new name` = am@").place, Place::Expression);
        assert_eq!(at("`fi@").place, Place::Nothing);
    }

    #[test]
    fn or_and_plain_brackets_do_not_split_stages() {
        let f = at("select a || b@");
        assert_eq!((f.place, f.stages_before.as_str()), (Place::Expression, ""));
        assert_eq!(at("select (a | b@").place, Place::Expression);
        assert_eq!(at("select a |@| b").place, Place::Expression);
        assert_eq!(at("select a == '|' | so@").place, Place::StageStart);
        assert_eq!(at("sort a # x | y\n| @").stages_before, "sort a");
    }

    #[test]
    fn a_join_groups_file_is_read_after_its_bracket() {
        let file = |s| match at(s).group {
            Group::Join { file } => file,
            other => panic!("{s}: {other:?}"),
        };
        assert_eq!(
            file("join (sort (a) @) 'my file.csv' on a"),
            Some("my file.csv".into())
        );
        assert_eq!(file("join (sort '@') b.csv"), Some("b.csv".into()));
        assert_eq!(file("join (sort a # )\n @) b.csv"), Some("b.csv".into()));
        assert_eq!(file("join (sort @"), None);
        assert_eq!(file("join (sort @) | cols a"), None);
        assert_eq!(file("join (sort @) on a"), None);
        // A `,` ends an unquoted file: another item follows.
        assert_eq!(
            file("join (sort @) a.csv, b.csv on k"),
            Some("a.csv".into())
        );
        assert_eq!(file("join (sort @) a.csv,b.csv on k"), Some("a.csv".into()));
        assert_eq!(file("join (sort @) 'a,b.csv' on k"), Some("a,b.csv".into()));
        // The stages after the group's `)` are the top's again.
        assert_eq!(at("join (sort a) b.csv on a | @").group, Group::Top);
    }

    #[test]
    fn the_fn_definitions_come_before_the_stages() {
        let f = at("fn p(n) { cols n } # defs\np(a) | sort @");
        assert_eq!(f.stages_before, "fn p(n) { cols n }\np(a)");
        assert_eq!(f.fns, ["p"]);
        assert_eq!(at("fn p(n) { cols n }\nsort @").stages_before, "");
        let f = at("fn p(n) { cols n | sort @ }");
        assert_eq!(
            (f.group, f.stages_before.as_str()),
            (Group::FnBody, "cols n")
        );
        assert_eq!(at("fn p(n@").place, Place::Nothing);
    }

    #[test]
    fn any_offset_in_any_script_finds_a_place() {
        let scripts = [
            "fn p(n) { cols n }\njoin -l (sort `a é` | select x == 'ü|' ) \"f g.csv\" on a, b # c\n| graph -c",
            "select ((a > 1 && (b",
            "é`ü'\"#|{[(}])",
            "add x = '",
        ];
        for script in scripts {
            for at in 0..=script.len() + 2 {
                let f = find(script, at);
                assert!(
                    f.word.start <= f.word.end && f.word.end <= script.len(),
                    "{script} {at}"
                );
                assert!(
                    script.is_char_boundary(f.word.start) && script.is_char_boundary(f.word.end)
                );
            }
        }
        // An offset inside a character counts as that character's start.
        assert_eq!(find("sort é", 6), find("sort é", 5));
    }

    #[test]
    fn fn_names_are_found() {
        assert_eq!(
            at("fn prep(n) { cols n }\nfn tidy() { uniq }\n@").fns,
            ["prep", "tidy"]
        );
        // A name defined twice is listed once.
        assert_eq!(at("fn p() { uniq }\nfn p() { head }\n@").fns, ["p"]);
    }
}
