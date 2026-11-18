//! Infer what each `?` of a query is compared with or assigned to, so its bind argument can be
//! checked at compile time. TiDB reports every parameter as a string, so the server cannot tell.
//!
//! The analysis is lexical and deliberately narrow. Where it is unsure it leaves a parameter
//! unchecked, never wrong. Recognised, at the top level of the statement (or inside plain
//! parenthesised conditions):
//! - `col <op> ?` and `? <op> col`, where `<op>` is a comparison or `[NOT] LIKE` (LIKE is only
//!   checked against a text or binary column: its pattern is text whatever the column);
//! - `col [NOT] IN (?, ...)` and `col [NOT] BETWEEN ? AND ?`;
//! - `SET col = ?` (UPDATE, `INSERT ... SET`, `ON DUPLICATE KEY UPDATE`);
//! - `INSERT INTO t (cols) VALUES (?, ...), ...`;
//! - `LIMIT ?`, `LIMIT ?, ?` and `OFFSET ?`.
//!
//! Comparisons only count in the FROM (join conditions), WHERE and SET parts of a statement,
//! because the column types come from a probe, `SELECT <columns> FROM <the statement's FROM>`,
//! which the server describes. Subqueries, function arguments, the select list, HAVING, UNION
//! and CTEs are left unchecked, and so is a whole statement with an executable comment (`/*! ... */`).

/// What a placeholder is matched against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Site {
    /// The column expression `Analysis::partners[i]`.
    Column(usize),
    /// The other side of `[NOT] LIKE` is `Analysis::partners[i]`. LIKE compares as text
    /// whatever the column, so this only says something when the column is text or binary.
    Like(usize),
    /// An unsigned integer (`LIMIT`/`OFFSET`).
    Count,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Analysis {
    /// One entry per `?` placeholder.
    pub sites: Vec<Option<Site>>,
    /// Column expressions, as written in the query.
    pub partners: Vec<String>,
    /// The table references the partners resolve in (the FROM clause, or the INSERT table).
    pub from: Option<String>,
}

impl Analysis {
    /// The probe that describes partner `i` (or all of them, with `None`).
    pub fn probe(&self, only: Option<usize>) -> Option<String> {
        let from = self.from.as_ref()?;
        let select = match only {
            Some(i) => self.partners[i].clone(),
            None if self.partners.is_empty() => return None,
            None => self.partners.join(", "),
        };
        Some(format!("SELECT {select} FROM {from}"))
    }
}

pub fn analyze(sql: &str) -> Analysis {
    let (toks, executable_comment) = tokenize(sql);
    let placeholders = toks.iter().filter(|t| t.kind == Kind::Placeholder).count();
    let mut a = Analysis { sites: vec![None; placeholders], ..Analysis::default() };
    if toks.is_empty() || executable_comment {
        return a;
    }
    Analyzer { sql, toks: &toks, ctx: Vec::new(), a: &mut a }.run();
    a
}

// ---- tokens

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Word,
    /// `` `quoted` ``, or a word right after a dot (`t.order`): an identifier, never a keyword.
    Ident,
    Literal,
    Placeholder,
    Op,
    Open,
    Close,
    Comma,
    Dot,
    Semi,
}

#[derive(Debug, Clone, Copy)]
struct Tok {
    kind: Kind,
    start: usize,
    end: usize,
}

/// The tokens, and whether an executable comment (`/*! ... */`, `/*T![feature] ... */`) was
/// skipped: TiDB runs its content, so the tokens are then not the whole statement.
fn tokenize(sql: &str) -> (Vec<Tok>, bool) {
    let b = sql.as_bytes();
    let mut toks = Vec::new();
    let mut executable_comment = false;
    let mut i = 0;
    let quoted_end = |mut i: usize, q: u8| {
        // `i` is just past the opening quote; a doubled quote is an escaped quote,
        // and in strings (not identifiers) so is a backslash.
        while i < b.len() {
            if b[i] == b'\\' && q != b'`' {
                i += 2;
            } else if b[i] == q {
                if b.get(i + 1) == Some(&q) {
                    i += 2;
                } else {
                    return i + 1;
                }
            } else {
                i += 1;
            }
        }
        b.len()
    };
    while i < b.len() {
        let c = b[i];
        let start = i;
        let kind = match c {
            _ if c.is_ascii_whitespace() => {
                i += 1;
                continue;
            }
            b'#' => {
                i = b[i..].iter().position(|&c| c == b'\n').map_or(b.len(), |p| i + p);
                continue;
            }
            b'-' if b.get(i + 1) == Some(&b'-') && b.get(i + 2).is_none_or(|c| c.is_ascii_whitespace()) => {
                i = b[i..].iter().position(|&c| c == b'\n').map_or(b.len(), |p| i + p);
                continue;
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                executable_comment |= b[i + 2..].starts_with(b"!") || b[i + 2..].starts_with(b"T!");
                i = sql[i + 2..].find("*/").map_or(b.len(), |p| i + 2 + p + 2);
                continue;
            }
            b'\'' | b'"' => {
                i = quoted_end(i + 1, c);
                Kind::Literal
            }
            b'`' => {
                i = quoted_end(i + 1, c);
                Kind::Ident
            }
            b'?' => {
                i += 1;
                Kind::Placeholder
            }
            b'(' => {
                i += 1;
                Kind::Open
            }
            b')' => {
                i += 1;
                Kind::Close
            }
            b',' => {
                i += 1;
                Kind::Comma
            }
            b';' => {
                i += 1;
                Kind::Semi
            }
            b'.' if !b.get(i + 1).is_some_and(u8::is_ascii_digit) => {
                i += 1;
                Kind::Dot
            }
            _ if c.is_ascii_digit() || c == b'.' => {
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'.' || b[i] == b'_') {
                    i += 1;
                }
                Kind::Literal
            }
            _ if c.is_ascii_alphabetic() || c == b'_' || c == b'$' || c >= 0x80 => {
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$' || b[i] >= 0x80) {
                    i += 1;
                }
                if toks.last().is_some_and(|t: &Tok| t.kind == Kind::Dot) { Kind::Ident } else { Kind::Word }
            }
            _ => {
                let three = sql.get(i..i + 3);
                let two = sql.get(i..i + 2);
                i += if three == Some("<=>") {
                    3
                } else if matches!(two, Some("<=" | ">=" | "<>" | "!=" | ":=" | "||" | "&&" | "<<" | ">>" | "->")) {
                    2
                } else {
                    1
                };
                Kind::Op
            }
        };
        toks.push(Tok { kind, start, end: i });
    }
    (toks, executable_comment)
}

// ---- parenthesis contexts

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ctx {
    /// Statement level, or a parenthesised condition.
    Plain,
    /// The list of `IN (...)`, opened at this token index.
    InList(usize),
    /// A subquery, a function call's arguments, a column or VALUES list: not analysed here.
    Opaque,
}

/// Words after which `(` groups a condition rather than calling a function.
const GROUPING: &[&str] = &["WHERE", "AND", "OR", "XOR", "NOT", "ON", "WHEN", "THEN", "ELSE", "HAVING", "SET"];

struct Analyzer<'a> {
    sql: &'a str,
    toks: &'a [Tok],
    ctx: Vec<Ctx>,
    a: &'a mut Analysis,
}

/// Clauses whose comparisons are checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Clause {
    Other,
    From,
    Where,
    Set,
}

impl Analyzer<'_> {
    fn text(&self, i: usize) -> &str {
        &self.sql[self.toks[i].start..self.toks[i].end]
    }

    fn is(&self, i: usize, word: &str) -> bool {
        self.toks.get(i).is_some_and(|t| t.kind == Kind::Word) && self.text(i).eq_ignore_ascii_case(word)
    }

    fn is_any(&self, i: usize, words: &[&str]) -> bool {
        words.iter().any(|w| self.is(i, w))
    }

    fn kind(&self, i: usize) -> Option<Kind> {
        self.toks.get(i).map(|t| t.kind)
    }

    fn compute_contexts(&mut self) {
        let mut stack: Vec<Ctx> = Vec::new();
        let mut ctx = Vec::with_capacity(self.toks.len());
        for i in 0..self.toks.len() {
            let current = stack.last().copied().unwrap_or(Ctx::Plain);
            match self.toks[i].kind {
                Kind::Open => {
                    ctx.push(current);
                    let inner = if current != Ctx::Plain || self.is_any(i + 1, &["SELECT", "WITH", "VALUES", "TABLE"]) {
                        Ctx::Opaque
                    } else if i > 0 && self.is(i - 1, "IN") {
                        Ctx::InList(i)
                    } else if i == 0 || self.is_any(i - 1, GROUPING) {
                        Ctx::Plain
                    } else {
                        match self.toks[i - 1].kind {
                            Kind::Op | Kind::Open | Kind::Comma => Ctx::Plain,
                            _ => Ctx::Opaque,
                        }
                    };
                    stack.push(inner);
                }
                Kind::Close => {
                    stack.pop();
                    ctx.push(stack.last().copied().unwrap_or(Ctx::Plain));
                }
                _ => ctx.push(current),
            }
        }
        self.ctx = ctx;
    }

    /// Parenthesis depth of each token (0 = the statement's top level).
    fn depths(&self) -> Vec<usize> {
        let mut depth = 0usize;
        self.toks
            .iter()
            .map(|t| match t.kind {
                Kind::Open => {
                    depth += 1;
                    depth - 1
                }
                Kind::Close => {
                    depth = depth.saturating_sub(1);
                    depth
                }
                _ => depth,
            })
            .collect()
    }

    fn run(&mut self) {
        self.compute_contexts();
        let depth = self.depths();
        let top = |i: usize| depth[i] == 0;
        // One statement only, without set operations.
        let n = self.toks.len();
        let last = if self.toks[n - 1].kind == Kind::Semi { n - 1 } else { n };
        if (0..last)
            .any(|i| top(i) && (self.toks[i].kind == Kind::Semi || self.is_any(i, &["UNION", "INTERSECT", "EXCEPT"])))
        {
            return;
        }
        let toks = &self.toks[..last];
        let mut clause = vec![Clause::Other; toks.len()];
        let first = self.text(0).to_ascii_uppercase();
        let mut from: Option<(usize, usize)> = None; // token range of the table references
        match first.as_str() {
            "SELECT" => {
                let Some(f) = (0..last).find(|&i| top(i) && self.is(i, "FROM")) else {
                    self.limits(last, &depth);
                    return;
                };
                let end = (f + 1..last)
                    .find(|&i| {
                        top(i)
                            && self.is_any(
                                i,
                                &["WHERE", "GROUP", "HAVING", "ORDER", "LIMIT", "WINDOW", "FOR", "LOCK", "INTO"],
                            )
                    })
                    .unwrap_or(last);
                from = Some((f + 1, end));
                self.clauses(&mut clause, f, last, &depth);
            }
            "UPDATE" => {
                let mut s = 1;
                while self.is_any(s, &["LOW_PRIORITY", "IGNORE"]) {
                    s += 1;
                }
                let Some(set) = (s..last).find(|&i| top(i) && self.is(i, "SET")) else { return };
                from = Some((s, set));
                for c in &mut clause[s..set] {
                    *c = Clause::From;
                }
                self.clauses(&mut clause, set, last, &depth);
            }
            "DELETE" => {
                if (0..last).any(|i| top(i) && self.is(i, "USING")) {
                    return;
                }
                let Some(f) = (0..last).find(|&i| top(i) && self.is(i, "FROM")) else { return };
                let end =
                    (f + 1..last).find(|&i| top(i) && self.is_any(i, &["WHERE", "ORDER", "LIMIT"])).unwrap_or(last);
                from = Some((f + 1, end));
                self.clauses(&mut clause, f, last, &depth);
            }
            "INSERT" | "REPLACE" => {
                self.insert(last, &depth, &mut clause, &mut from);
            }
            _ => return,
        }
        if let Some((s, e)) = from.filter(|(s, e)| s < e) {
            self.a.from = Some(self.sql[self.toks[s].start..self.toks[e - 1].end].to_owned());
        }
        self.limits(last, &depth);
        if self.a.from.is_none() {
            return;
        }
        let mut p = 0;
        for i in 0..last {
            if self.toks[i].kind != Kind::Placeholder {
                continue;
            }
            p += 1;
            if self.a.sites[p - 1].is_some() || clause[i] == Clause::Other {
                continue;
            }
            if let Some((column, like)) = self.comparison(i) {
                let idx = self.partner(column);
                self.a.sites[p - 1] = Some(if like { Site::Like(idx) } else { Site::Column(idx) });
            }
        }
    }

    /// Mark the clause of every token from `start` (a clause keyword) to `end`, following the
    /// top-level clause keywords.
    fn clauses(&self, clause: &mut [Clause], start: usize, end: usize, depth: &[usize]) {
        let mut current = Clause::Other;
        for (i, c) in clause.iter_mut().enumerate().take(end).skip(start) {
            if depth[i] == 0 && self.toks[i].kind == Kind::Word {
                let w = self.text(i).to_ascii_uppercase();
                current = match w.as_str() {
                    "FROM" => Clause::From,
                    "WHERE" => Clause::Where,
                    "SET" => Clause::Set,
                    // Not VALUE/VALUES: `value` is a legal column name (`SET value = ?, b = ?`), and
                    // the rows of INSERT ... VALUES are parenthesised, so never compared anyway.
                    "GROUP" | "HAVING" | "ORDER" | "LIMIT" | "WINDOW" | "FOR" | "LOCK" | "INTO" | "SELECT" => {
                        Clause::Other
                    }
                    // `ON DUPLICATE KEY UPDATE col = ?` assigns like SET.
                    "UPDATE" if i >= 2 && self.is(i - 1, "KEY") => Clause::Set,
                    "UPDATE" => Clause::Other,
                    _ => current,
                };
            }
            *c = current;
        }
    }

    fn insert(&mut self, last: usize, depth: &[usize], clause: &mut [Clause], from: &mut Option<(usize, usize)>) {
        let mut i = 1;
        while self.is_any(i, &["LOW_PRIORITY", "DELAYED", "HIGH_PRIORITY", "IGNORE", "INTO"]) {
            i += 1;
        }
        let Some(table_end) = self.colref_forward(i) else { return };
        *from = Some((i, table_end));
        if (table_end..last).any(|j| depth[j] == 0 && self.is_any(j, &["SELECT", "TABLE", "WITH"])) {
            return; // INSERT ... SELECT
        }
        self.clauses(clause, table_end, last, depth);
        // Optional column list, then VALUES rows.
        let mut j = table_end;
        let mut columns = Vec::new();
        if self.kind(j) == Some(Kind::Open) {
            j += 1;
            loop {
                let Some(end) = self.colref_forward(j).filter(|&e| e == j + 1) else { return };
                columns.push(self.text(j).to_owned());
                j = end;
                match self.kind(j) {
                    Some(Kind::Comma) => j += 1,
                    Some(Kind::Close) => {
                        j += 1;
                        break;
                    }
                    _ => return,
                }
            }
        }
        if columns.is_empty() || !self.is_any(j, &["VALUES", "VALUE"]) {
            return;
        }
        j += 1;
        let placeholder_index =
            |toks: &[Tok], at: usize| toks[..at].iter().filter(|t| t.kind == Kind::Placeholder).count();
        while self.kind(j) == Some(Kind::Open) {
            let mut col = 0;
            j += 1;
            loop {
                // An element that is exactly `?`.
                if self.kind(j) == Some(Kind::Placeholder)
                    && matches!(self.kind(j + 1), Some(Kind::Comma | Kind::Close))
                    && let Some(name) = columns.get(col)
                {
                    let idx = self.partner(name.clone());
                    self.a.sites[placeholder_index(self.toks, j)] = Some(Site::Column(idx));
                }
                // Skip to the end of this element.
                let mut level = 0usize;
                while let Some(k) = self.kind(j) {
                    match k {
                        Kind::Open => level += 1,
                        Kind::Close if level == 0 => break,
                        Kind::Close => level -= 1,
                        Kind::Comma if level == 0 => break,
                        _ => {}
                    }
                    j += 1;
                }
                match self.kind(j) {
                    Some(Kind::Comma) => {
                        j += 1;
                        col += 1;
                    }
                    Some(Kind::Close) => {
                        j += 1;
                        break;
                    }
                    _ => return,
                }
            }
            if self.kind(j) == Some(Kind::Comma) {
                j += 1;
            } else {
                break;
            }
        }
    }

    /// `LIMIT ?`, `LIMIT ?, ?`, `OFFSET ?` at the top level.
    fn limits(&mut self, last: usize, depth: &[usize]) {
        let mut p = 0;
        for i in 0..last {
            if self.toks[i].kind != Kind::Placeholder {
                continue;
            }
            p += 1;
            if depth[i] != 0 || i == 0 {
                continue;
            }
            let after_limit = self.is_any(i - 1, &["LIMIT", "OFFSET"])
                || (i >= 3
                    && self.kind(i - 1) == Some(Kind::Comma)
                    && self.kind(i - 2) == Some(Kind::Placeholder)
                    && self.is(i - 3, "LIMIT"));
            if after_limit {
                self.a.sites[p - 1] = Some(Site::Count);
            }
        }
    }

    fn partner(&mut self, text: String) -> usize {
        match self.a.partners.iter().position(|p| *p == text) {
            Some(i) => i,
            None => {
                self.a.partners.push(text);
                self.a.partners.len() - 1
            }
        }
    }

    // ---- comparisons

    fn is_ident(&self, i: usize) -> bool {
        match self.kind(i) {
            Some(Kind::Ident) => true,
            Some(Kind::Word) => !self.is_any(i, NOT_IDENT),
            _ => false,
        }
    }

    /// `a`, `t.a` or `db.t.a` starting at `i`: the index just past it.
    fn colref_forward(&self, i: usize) -> Option<usize> {
        if !self.is_ident(i) {
            return None;
        }
        let mut end = i + 1;
        for _ in 0..2 {
            if self.kind(end) == Some(Kind::Dot) && self.is_ident(end + 1) {
                end += 2;
            }
        }
        Some(end)
    }

    /// The same, ending at `i` (inclusive): the index of its first token.
    fn colref_backward(&self, i: usize) -> Option<usize> {
        if !self.is_ident(i) {
            return None;
        }
        let mut start = i;
        for _ in 0..2 {
            if start >= 2 && self.kind(start - 1) == Some(Kind::Dot) && self.is_ident(start - 2) {
                start -= 2;
            }
        }
        Some(start)
    }

    fn boundary_before(&self, i: Option<usize>) -> bool {
        let Some(i) = i else { return true };
        match self.kind(i) {
            Some(Kind::Open) => self.ctx[i + 1] == Ctx::Plain,
            Some(Kind::Comma) => true,
            Some(Kind::Op) => matches!(self.text(i), "&&" | "||"),
            Some(Kind::Word) => self.is_any(i, BEFORE) && !(self.is(i, "AND") && self.closes_between(i)),
            _ => false,
        }
    }

    /// Whether the `AND` at `i` is the one of `x BETWEEN y AND z`. What follows it is then `z`,
    /// not a new condition: `a BETWEEN 1 AND b = ?` is `(a BETWEEN 1 AND b) = ?`.
    fn closes_between(&self, i: usize) -> bool {
        let mut level = 0usize;
        for j in (0..i).rev() {
            match self.toks[j].kind {
                Kind::Close => level += 1,
                Kind::Open if level == 0 => return false,
                Kind::Open => level -= 1,
                Kind::Word if level == 0 && self.is(j, "BETWEEN") => return true,
                Kind::Word if level == 0 && self.is_any(j, &["AND", "OR", "XOR", "WHERE", "ON", "SET"]) => {
                    return false;
                }
                _ => {}
            }
        }
        false
    }

    fn boundary_after(&self, i: usize) -> bool {
        match self.kind(i) {
            None | Some(Kind::Close | Kind::Comma | Kind::Semi) => true,
            Some(Kind::Op) => matches!(self.text(i), "&&" | "||"),
            Some(Kind::Word) => self.is_any(i, AFTER),
            _ => false,
        }
    }

    /// Length of a comparison operator starting at `i`.
    fn operator_at(&self, i: usize) -> Option<usize> {
        match self.kind(i)? {
            Kind::Op if matches!(self.text(i), "=" | "<>" | "!=" | "<" | "<=" | ">" | ">=" | "<=>") => Some(1),
            Kind::Word if self.is(i, "LIKE") => Some(1),
            Kind::Word if self.is(i, "NOT") && self.is(i + 1, "LIKE") => Some(2),
            _ => None,
        }
    }

    /// The column placeholder `i` is compared with, assigned to, or listed against, and whether
    /// the comparison is a `LIKE`.
    fn comparison(&self, i: usize) -> Option<(String, bool)> {
        let colref = |s: usize, e: usize| (self.sql[self.toks[s].start..self.toks[e].end].to_owned(), false);
        let like = |(column, _): (String, bool), op_end: usize| (column, self.is(op_end - 1, "LIKE"));
        match self.ctx[i] {
            Ctx::Opaque => None,
            Ctx::InList(open) => {
                // `col [NOT] IN (?, ?, ...)`, each element exactly `?`.
                if !matches!(self.kind(i - 1), Some(Kind::Open | Kind::Comma))
                    || !matches!(self.kind(i + 1), Some(Kind::Comma | Kind::Close))
                {
                    return None;
                }
                let before_in = if open.checked_sub(2).is_some_and(|k| self.is(k, "NOT")) {
                    open.checked_sub(3)?
                } else {
                    open.checked_sub(2)?
                };
                let start = self.colref_backward(before_in)?;
                self.boundary_before(start.checked_sub(1)).then(|| colref(start, before_in))
            }
            Ctx::Plain => {
                // `col [NOT] BETWEEN ? AND ?`
                let between = if self.is(i.checked_sub(1)?, "BETWEEN") && self.is(i + 1, "AND") {
                    Some((i - 1, i + 2))
                } else if i >= 3
                    && self.is(i - 1, "AND")
                    && self.kind(i - 2) == Some(Kind::Placeholder)
                    && self.is(i - 3, "BETWEEN")
                {
                    Some((i - 3, i))
                } else {
                    None
                };
                if let Some((kw, second)) = between {
                    if self.kind(second) != Some(Kind::Placeholder) || !self.boundary_after(second + 1) {
                        return None;
                    }
                    let end = if kw.checked_sub(1).is_some_and(|k| self.is(k, "NOT")) {
                        kw.checked_sub(2)?
                    } else {
                        kw.checked_sub(1)?
                    };
                    let start = self.colref_backward(end)?;
                    return self.boundary_before(start.checked_sub(1)).then(|| colref(start, end));
                }
                // `col <op> ?`
                if i >= 2 {
                    let op_start = if self.is(i - 1, "LIKE") && self.is(i - 2, "NOT") { i - 2 } else { i - 1 };
                    if self.operator_at(op_start).is_some_and(|len| op_start + len == i)
                        && op_start >= 1
                        && let Some(start) = self.colref_backward(op_start - 1)
                        && self.boundary_before(start.checked_sub(1))
                        && self.boundary_after(i + 1)
                    {
                        return Some(like(colref(start, op_start - 1), i));
                    }
                }
                // `? <op> col`
                let len = self.operator_at(i + 1)?;
                let end = self.colref_forward(i + 1 + len)?;
                (self.boundary_before(i.checked_sub(1)) && self.boundary_after(end))
                    .then(|| like(colref(i + 1 + len, end - 1), i + 1 + len))
            }
        }
    }
}

/// Words that end a condition on the left of a comparison.
const BEFORE: &[&str] = &["WHERE", "AND", "OR", "XOR", "NOT", "ON", "SET", "WHEN", "THEN", "ELSE", "UPDATE"];
/// Words that end a condition on the right of a comparison.
const AFTER: &[&str] = &[
    "AND",
    "OR",
    "XOR",
    "ORDER",
    "GROUP",
    "HAVING",
    "LIMIT",
    "WINDOW",
    "FOR",
    "LOCK",
    "WHERE",
    "THEN",
    "ELSE",
    "END",
    "WHEN",
    "ESCAPE",
    "JOIN",
    "INNER",
    "LEFT",
    "RIGHT",
    "CROSS",
    "NATURAL",
    "STRAIGHT_JOIN",
    "ON",
    "USING",
];
/// Words that are never a column name in these positions.
const NOT_IDENT: &[&str] = &[
    "NULL", "TRUE", "FALSE", "NOT", "AND", "OR", "XOR", "IS", "IN", "LIKE", "BETWEEN", "WHERE", "SET", "SELECT",
    "FROM", "ON", "WHEN", "THEN", "ELSE", "END", "CASE", "UPDATE", "VALUES", "INTERVAL", "BINARY", "DEFAULT", "EXISTS",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn sites(sql: &str) -> Vec<Option<String>> {
        let a = analyze(sql);
        a.sites
            .iter()
            .map(|s| match s {
                Some(Site::Column(i) | Site::Like(i)) => Some(a.partners[*i].clone()),
                Some(Site::Count) => Some("#".into()),
                None => None,
            })
            .collect()
    }

    fn s(v: &[Option<&str>]) -> Vec<Option<String>> {
        v.iter().map(|o| o.map(str::to_owned)).collect()
    }

    #[test]
    fn placeholders_in_literals_and_comments_are_not_counted() {
        let sql = "SELECT '?', \"?\", `?`, 'it''s ?', 'a\\'?' -- ?\n # ?\n /* ? */ FROM t WHERE a = ?";
        let a = analyze(sql);
        assert_eq!(a.sites.len(), 1);
        assert_eq!(sites(sql), s(&[Some("a")]));
        // `--` needs a space to start a comment: `a--?` is `a - (-?)`
        assert_eq!(analyze("SELECT a--?").sites.len(), 1);
    }

    #[test]
    fn select_conditions() {
        assert_eq!(
            sites("SELECT * FROM users u JOIN posts p ON p.user_id = u.id AND p.kind = ? WHERE u.id = ? AND ? < u.age"),
            s(&[Some("p.kind"), Some("u.id"), Some("u.age")])
        );
        assert_eq!(
            sites(
                "SELECT id FROM t WHERE (a = ? OR `b` LIKE ?) AND c NOT LIKE ? AND d IN (?, ?) AND e NOT BETWEEN ? AND ?"
            ),
            s(&[Some("a"), Some("`b`"), Some("c"), Some("d"), Some("d"), Some("e"), Some("e")])
        );
        assert_eq!(
            sites("SELECT a FROM t WHERE x <=> ? ORDER BY a LIMIT ? OFFSET ?"),
            s(&[Some("x"), Some("#"), Some("#")])
        );
        assert_eq!(sites("SELECT a FROM db.t WHERE db.t.a = ? LIMIT ?, ?"), s(&[Some("db.t.a"), Some("#"), Some("#")]));
        let a = analyze("SELECT a FROM t1 JOIN t2 USING (k) WHERE t1.a = ?");
        assert_eq!(a.from.as_deref(), Some("t1 JOIN t2 USING (k)"));
        assert_eq!(a.probe(None).as_deref(), Some("SELECT t1.a FROM t1 JOIN t2 USING (k)"));
    }

    #[test]
    fn unclear_positions_are_left_unchecked() {
        // select list, expressions, function arguments, subqueries, HAVING
        assert_eq!(sites("SELECT ? + 1 AS x FROM t"), s(&[None]));
        assert_eq!(sites("SELECT a FROM t WHERE a + b = ? AND c = ? + 1 AND d = LOWER(?)"), s(&[None, None, None]));
        assert_eq!(sites("SELECT a FROM t WHERE a IN (SELECT b FROM u WHERE c = ?)"), s(&[None]));
        assert_eq!(sites("SELECT a, COUNT(*) AS n FROM t GROUP BY a HAVING n > ?"), s(&[None]));
        assert_eq!(sites("SELECT a FROM t WHERE COALESCE(a, ?) = b"), s(&[None]));
        assert_eq!(sites("SELECT a FROM t WHERE a = ? COLLATE utf8mb4_bin"), s(&[None]));
        assert_eq!(sites("SELECT a FROM t WHERE a IN (?, ? + 1)"), s(&[Some("a"), None]));
        // no FROM: nothing to resolve against
        assert_eq!(sites("SELECT ? = 1"), s(&[None]));
        // set operations, CTEs, several statements
        assert_eq!(sites("SELECT a FROM t WHERE a = ? UNION SELECT a FROM u WHERE a = ?"), s(&[None, None]));
        assert_eq!(sites("WITH x AS (SELECT 1) SELECT * FROM x WHERE a = ?"), s(&[None]));
        assert_eq!(sites("SELECT a FROM t WHERE a = ?; SELECT 1"), s(&[None]));
    }

    #[test]
    fn writes() {
        assert_eq!(
            sites("UPDATE t SET a = ?, b = b + 1, `c` = ? WHERE id = ? LIMIT ?"),
            s(&[Some("a"), Some("`c`"), Some("id"), Some("#")])
        );
        assert_eq!(sites("DELETE FROM t WHERE id = ? AND x IS NULL"), s(&[Some("id")]));
        assert_eq!(
            sites("INSERT INTO t (a, `b`, c) VALUES (?, NOW(), ?), (?, ?, LOWER(?)) ON DUPLICATE KEY UPDATE c = ?"),
            s(&[Some("a"), Some("c"), Some("a"), Some("`b`"), None, Some("c")])
        );
        assert_eq!(sites("INSERT INTO t SET a = ?, b = ?"), s(&[Some("a"), Some("b")]));
        assert_eq!(sites("INSERT IGNORE INTO db.t (a) VALUE (?)"), s(&[Some("a")]));
        // no column list, or INSERT ... SELECT
        assert_eq!(sites("INSERT INTO t VALUES (?, ?)"), s(&[None, None]));
        assert_eq!(sites("INSERT INTO t (a) SELECT b FROM u WHERE c = ?"), s(&[None]));
        let a = analyze("INSERT INTO db.t (a) VALUES (?)");
        assert_eq!(a.probe(None).as_deref(), Some("SELECT a FROM db.t"));
    }

    #[test]
    fn a_word_after_a_dot_is_an_identifier() {
        assert_eq!(
            sites("SELECT t.from FROM t WHERE t.order = ? AND t.limit = ?"),
            s(&[Some("t.order"), Some("t.limit")])
        );
        assert_eq!(analyze("SELECT t.from FROM t WHERE t.where = ?").from.as_deref(), Some("t"));
        assert_eq!(
            sites("UPDATE t SET t.group = ?, t.set = ? WHERE t.null = ?"),
            s(&[Some("t.group"), Some("t.set"), Some("t.null")])
        );
        assert_eq!(sites("SELECT a FROM t WHERE ? = t.select LIMIT ?"), s(&[Some("t.select"), Some("#")]));
    }

    #[test]
    fn a_column_named_value_does_not_end_the_clause() {
        assert_eq!(sites("UPDATE t SET value = ?, b = ? WHERE c = ?"), s(&[Some("value"), Some("b"), Some("c")]));
        assert_eq!(sites("SELECT a FROM t WHERE value = ? AND b = ?"), s(&[Some("value"), Some("b")]));
        assert_eq!(sites("INSERT INTO t (value, b) VALUES (?, ?)"), s(&[Some("value"), Some("b")]));
        assert_eq!(
            sites("INSERT INTO t (a, b) VALUES (?, ?) ON DUPLICATE KEY UPDATE a = VALUES(a) + ?, b = ?"),
            s(&[Some("a"), Some("b"), None, Some("b")])
        );
    }

    #[test]
    fn like_patterns_are_told_apart_from_values() {
        let a = analyze("SELECT a FROM t WHERE a LIKE ? AND b NOT LIKE ? AND ? LIKE c AND d = ?");
        assert_eq!(a.sites, [Some(Site::Like(0)), Some(Site::Like(1)), Some(Site::Like(2)), Some(Site::Column(3))]);
    }

    #[test]
    fn the_and_of_between_does_not_start_a_condition() {
        // `(a BETWEEN 1 AND b) = ?` and `(a BETWEEN 1 AND ?) = b`: neither `?` is compared with `b`
        assert_eq!(sites("SELECT a FROM t WHERE a BETWEEN 1 AND b = ?"), s(&[None]));
        assert_eq!(sites("SELECT a FROM t WHERE a BETWEEN 1 AND ? = b"), s(&[None]));
        assert_eq!(sites("SELECT a FROM t WHERE a BETWEEN 1 AND b IN (?)"), s(&[None]));
        assert_eq!(sites("SELECT a FROM t WHERE a BETWEEN 1 AND b BETWEEN ? AND ?"), s(&[None, None]));
        // the next AND is a real one
        assert_eq!(
            sites("SELECT a FROM t WHERE a BETWEEN (1 + 1) AND 2 AND b = ? AND c BETWEEN ? AND ? AND d = ?"),
            s(&[Some("b"), Some("c"), Some("c"), Some("d")])
        );
    }

    #[test]
    fn executable_comments_disable_the_analysis() {
        // TiDB runs the content of `/*! */` (and of `/*T![feature] */`): the real condition is `a + b = ?`
        assert_eq!(sites("SELECT a FROM t WHERE /*! a + */ b = ?"), s(&[None]));
        assert_eq!(sites("SELECT a FROM t WHERE /*T![x] a + */ b = ?"), s(&[None]));
        assert_eq!(sites("SELECT /*+ USE_INDEX(t, i) */ a FROM t WHERE '/*!' = ? AND b = ?"), s(&[None, Some("b")]));
    }
}
