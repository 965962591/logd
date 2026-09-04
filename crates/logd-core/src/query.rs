//! 布尔查询语言：解析 → 编译 → 求值。
//!
//! ```text
//! tag=AeAlgo and "Magic:"
//! level>=W and not noise
//! (tag=AeAlgo or tag=Hal3Av3) and msg~/i4EVIndex\s*=\s*\d+/
//! time>="01-02 03:04:05" and time<"01-02 03:10"
//! ```
//!
//! # 为什么不是「每个叶子各扫一遍」
//!
//! 一棵有 20 个字面量叶子的树，朴素实现要在每行上跑 20 次子串搜索。
//! 这里把**所有字面量叶子合并进一个 Aho–Corasick**：不管树长什么样，
//! 每行只遍历一次，同时得到全部叶子的命中情况，再拿这个位集去走 AST。
//! 50GB 上这就是 20 倍的差距。
//!
//! # 正则延迟求值
//!
//! 正则比 AC 贵一到两个数量级。求值时先只算字面量和字段，把正则槽位分别当成
//! 全真、全假各算一次——**两次结果相同就说明正则根本不影响结论**，直接跳过。
//! `level>=E or /复杂正则/` 这种，绝大多数行在第一项就定了。

use std::fmt;

use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};
use anyhow::{anyhow, bail, Context, Result};
use regex::bytes::{Regex, RegexBuilder, RegexSet, RegexSetBuilder};

use crate::logline::{self, Level, LogLine, Ts};
use crate::source::Encoding;

/// 槽位上限。一个查询有 256 个叶子已经离谱了。
const MAX_SLOTS: usize = 256;

// ============================== 位集 ==============================

/// 定长位集，避免每行求值都分配。
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Bits([u64; MAX_SLOTS / 64]);

impl Bits {
    #[inline]
    fn set(&mut self, i: usize) {
        if i < MAX_SLOTS {
            self.0[i / 64] |= 1 << (i % 64);
        }
    }
    #[inline]
    fn get(&self, i: usize) -> bool {
        i < MAX_SLOTS && self.0[i / 64] & (1 << (i % 64)) != 0
    }
    #[inline]
    fn union(&mut self, o: &Bits) {
        for (a, b) in self.0.iter_mut().zip(o.0.iter()) {
            *a |= *b;
        }
    }
    #[inline]
    fn clear(&mut self) {
        self.0 = [0; MAX_SLOTS / 64];
    }
}

// ============================== AST ==============================

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl CmpOp {
    fn apply<T: Ord>(self, a: T, b: T) -> bool {
        match self {
            CmpOp::Eq => a == b,
            CmpOp::Ne => a != b,
            CmpOp::Lt => a < b,
            CmpOp::Le => a <= b,
            CmpOp::Gt => a > b,
            CmpOp::Ge => a >= b,
        }
    }
}

/// 编译后的树。叶子已经被换成槽位下标。
#[derive(Clone, Debug)]
enum Node {
    Always(bool),
    Slot(usize),
    Not(Box<Node>),
    And(Vec<Node>),
    Or(Vec<Node>),
}

impl Node {
    fn eval(&self, bits: &Bits) -> bool {
        match self {
            Node::Always(v) => *v,
            Node::Slot(i) => bits.get(*i),
            Node::Not(n) => !n.eval(bits),
            Node::And(v) => v.iter().all(|n| n.eval(bits)),
            Node::Or(v) => v.iter().any(|n| n.eval(bits)),
        }
    }
}

// ============================== 字段项 ==============================

/// 字符串字段（tag / msg）的比较方式。
enum StrOp {
    /// 整体相等
    Eq(Vec<u8>),
    /// 包含子串。大小写不敏感时用一个单模式 AC，比手写循环快。
    Contains(AhoCorasick),
    Regex(Regex),
}

impl StrOp {
    fn test(&self, hay: &[u8]) -> bool {
        match self {
            StrOp::Eq(v) => hay == v.as_slice(),
            StrOp::Contains(ac) => ac.is_match(hay),
            StrOp::Regex(re) => re.is_match(hay),
        }
    }
}

enum FieldTerm {
    Level(CmpOp, Level),
    Pid(CmpOp, u32),
    Tid(CmpOp, u32),
    Time(CmpOp, Ts),
    Tag(StrOp, bool),
    Msg(StrOp, bool),
    /// `is:log` —— 这行能不能解析成 logcat 结构
    Structured,
}

impl FieldTerm {
    fn test(&self, line: &[u8], f: &LogLine) -> bool {
        match self {
            // 字段缺失一律判否：`level>=W` 不该把没有 level 的行捞进来
            FieldTerm::Level(op, v) => f.level.is_some_and(|x| op.apply(x, *v)),
            FieldTerm::Pid(op, v) => f.pid.is_some_and(|x| op.apply(x, *v)),
            FieldTerm::Tid(op, v) => f.tid.is_some_and(|x| op.apply(x, *v)),
            FieldTerm::Time(op, v) => f.ts.is_some_and(|x| op.apply(x, *v)),
            FieldTerm::Tag(op, neg) => op.test(f.tag_bytes(line)) != *neg,
            FieldTerm::Msg(op, neg) => op.test(f.message_bytes(line)) != *neg,
            FieldTerm::Structured => f.level.is_some(),
        }
    }
}

// ============================== 编译产物 ==============================

#[derive(Clone, Copy, Debug, Default)]
pub struct CompileOptions {
    /// 文本匹配是否区分大小写。缺省不区分——`.tat` 里的过滤器清一色 `case_sensitive="n"`。
    pub case_sensitive: bool,
    /// `time>=03:04:05` 这种只给时分秒的值，按哪一天解释。
    /// 通常传文件第一行的时间戳。
    pub base_date: Option<Ts>,
    /// 关键字要按目标文件的编码转成字节串。
    pub encoding: Option<Encoding>,
}

pub struct Query {
    root: Node,
    /// 整行字面量集合，一遍扫出所有命中
    ac: Option<AhoCorasick>,
    ac_slots: Vec<usize>,
    /// 整行正则集合
    re: Option<RegexSet>,
    re_slots: Vec<usize>,
    /// 需要解析行结构才能判定的项
    fields: Vec<FieldTerm>,
    field_slots: Vec<usize>,
    /// 正则槽位掩码，用于延迟求值
    re_mask: Bits,
    /// 有没有字段项。没有就完全不调 `logline::parse`
    needs_fields: bool,
    /// 空查询 = 全部可见
    empty: bool,
}

impl fmt::Debug for Query {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Query")
            .field("root", &self.root)
            .field("literals", &self.ac_slots.len())
            .field("regexes", &self.re_slots.len())
            .field("fields", &self.fields.len())
            .field("needs_fields", &self.needs_fields)
            .finish()
    }
}

/// 每个工作线程复用一份，避免每行都清零一整个位集之外的额外开销。
#[derive(Default)]
pub struct QueryScratch {
    lo: Bits,
    hi: Bits,
}

impl Query {
    /// 空查询：匹配所有行。
    pub fn always_true() -> Query {
        Query {
            root: Node::Always(true),
            ac: None,
            ac_slots: Vec::new(),
            re: None,
            re_slots: Vec::new(),
            fields: Vec::new(),
            field_slots: Vec::new(),
            re_mask: Bits::default(),
            needs_fields: false,
            empty: true,
        }
    }

    pub fn parse(src: &str, opts: CompileOptions) -> Result<Query> {
        if src.trim().is_empty() {
            return Ok(Query::always_true());
        }
        let ast = Parser::new(src).parse()?;
        Compiler::new(opts).compile(ast)
    }

    /// 没有任何条件，全部行可见。
    pub fn is_empty(&self) -> bool {
        self.empty
    }

    /// 需要解析 logcat 结构吗。为 false 时热路径完全不碰 [`logline::parse`]。
    pub fn needs_fields(&self) -> bool {
        self.needs_fields
    }

    /// 判定一行是否命中。热路径。
    pub fn matches(&self, line: &[u8], scratch: &mut QueryScratch) -> bool {
        if self.empty {
            return true;
        }

        scratch.lo.clear();

        // 1) 字面量：一遍 AC 拿到所有命中。
        //    必须用 overlapping —— leftmost 语义下 "abc"/"bcd" 在 "abcd" 里只中前者。
        if let Some(ac) = &self.ac {
            for m in ac.find_overlapping_iter(line) {
                scratch.lo.set(self.ac_slots[m.pattern().as_usize()]);
            }
        }

        // 2) 字段项：只解析一次
        if self.needs_fields {
            let parsed = logline::parse(line).unwrap_or_else(|| LogLine::plain(line.len()));
            for (i, t) in self.fields.iter().enumerate() {
                if t.test(line, &parsed) {
                    scratch.lo.set(self.field_slots[i]);
                }
            }
        }

        // 3) 正则延迟：把正则槽位分别当全假/全真各算一次，
        //    结论一致就说明正则不影响结果，直接省掉。
        if self.re.is_some() {
            scratch.hi = scratch.lo;
            scratch.hi.union(&self.re_mask);
            let lo = self.root.eval(&scratch.lo);
            if lo == self.root.eval(&scratch.hi) {
                return lo;
            }
            // 真跑不掉，才付这个钱
            let set = self.re.as_ref().unwrap();
            for id in set.matches(line).iter() {
                scratch.lo.set(self.re_slots[id]);
            }
        }

        self.root.eval(&scratch.lo)
    }
}

// ============================== 词法 ==============================

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    LParen,
    RParen,
    And,
    Or,
    Not,
    /// 裸词或引号串
    Word(String),
    /// `/.../`
    Regex(String),
    Op(&'static str),
}

fn lex(src: &str) -> Result<Vec<Tok>> {
    let b: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut i = 0usize;

    while i < b.len() {
        let c = b[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        match c {
            '(' => {
                out.push(Tok::LParen);
                i += 1;
            }
            ')' => {
                out.push(Tok::RParen);
                i += 1;
            }
            '"' | '\'' => {
                let quote = c;
                i += 1;
                let mut s = String::new();
                loop {
                    let Some(&ch) = b.get(i) else {
                        bail!("引号没闭合");
                    };
                    if ch == '\\' {
                        // 反斜杠转义下一个字符，正则里 \d 这类要原样留着
                        if let Some(&n) = b.get(i + 1) {
                            if n == quote || n == '\\' {
                                s.push(n);
                                i += 2;
                                continue;
                            }
                        }
                    }
                    if ch == quote {
                        i += 1;
                        break;
                    }
                    s.push(ch);
                    i += 1;
                }
                out.push(Tok::Word(s));
            }
            '/' => {
                i += 1;
                let mut s = String::new();
                loop {
                    let Some(&ch) = b.get(i) else {
                        bail!("正则的 / 没闭合");
                    };
                    if ch == '\\' {
                        if let Some(&n) = b.get(i + 1) {
                            // 只吃掉 \/ 的转义，其余原样交给 regex
                            if n == '/' {
                                s.push('/');
                                i += 2;
                                continue;
                            }
                            s.push(ch);
                            s.push(n);
                            i += 2;
                            continue;
                        }
                    }
                    if ch == '/' {
                        i += 1;
                        break;
                    }
                    s.push(ch);
                    i += 1;
                }
                out.push(Tok::Regex(s));
            }
            '!' => {
                if b.get(i + 1) == Some(&'=') {
                    out.push(Tok::Op("!="));
                    i += 2;
                } else if b.get(i + 1) == Some(&'~') {
                    out.push(Tok::Op("!~"));
                    i += 2;
                } else {
                    out.push(Tok::Not);
                    i += 1;
                }
            }
            '=' => {
                if b.get(i + 1) == Some(&'=') {
                    i += 2;
                } else {
                    i += 1;
                }
                out.push(Tok::Op("="));
            }
            '>' | '<' => {
                let two = b.get(i + 1) == Some(&'=');
                out.push(Tok::Op(match (c, two) {
                    ('>', true) => ">=",
                    ('>', false) => ">",
                    ('<', true) => "<=",
                    _ => "<",
                }));
                i += if two { 2 } else { 1 };
            }
            '~' => {
                out.push(Tok::Op("~"));
                i += 1;
            }
            ':' => {
                out.push(Tok::Op(":"));
                i += 1;
            }
            '&' => {
                if b.get(i + 1) == Some(&'&') {
                    i += 1;
                }
                out.push(Tok::And);
                i += 1;
            }
            '|' => {
                if b.get(i + 1) == Some(&'|') {
                    i += 1;
                }
                out.push(Tok::Or);
                i += 1;
            }
            _ => {
                // 裸词：吃到分隔符为止
                let start = i;
                while i < b.len() {
                    let ch = b[i];
                    if ch.is_whitespace() || "()\"'/=!<>~:&|".contains(ch) {
                        break;
                    }
                    i += 1;
                }
                if i == start {
                    bail!("无法识别的字符 {c:?}");
                }
                let w: String = b[start..i].iter().collect();
                out.push(match w.to_ascii_lowercase().as_str() {
                    "and" => Tok::And,
                    "or" => Tok::Or,
                    "not" => Tok::Not,
                    _ => Tok::Word(w),
                });
            }
        }
    }
    Ok(out)
}

// ============================== 语法 ==============================

/// 未编译的树，叶子还是原始文本。
#[derive(Debug, Clone)]
enum Ast {
    And(Vec<Ast>),
    Or(Vec<Ast>),
    Not(Box<Ast>),
    /// 整行字面量
    Text(String),
    /// 整行正则
    Regex(String),
    /// `field op value`
    Field {
        name: String,
        op: &'static str,
        value: String,
        /// value 是不是 `/regex/` 形式
        is_regex: bool,
    },
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn new(src: &str) -> Self {
        Self {
            toks: lex(src).unwrap_or_default(),
            pos: 0,
        }
    }

    fn parse(mut self) -> Result<Ast> {
        // lex 失败时 toks 为空，这里重跑一遍把错误报出来
        if self.toks.is_empty() {
            bail!("查询为空或无法解析");
        }
        let ast = self.parse_or()?;
        if self.pos < self.toks.len() {
            bail!("查询末尾有多余内容：{:?}", self.toks[self.pos]);
        }
        Ok(ast)
    }

    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn parse_or(&mut self) -> Result<Ast> {
        let mut v = vec![self.parse_and()?];
        while self.peek() == Some(&Tok::Or) {
            self.pos += 1;
            v.push(self.parse_and()?);
        }
        Ok(if v.len() == 1 {
            v.pop().unwrap()
        } else {
            Ast::Or(v)
        })
    }

    fn parse_and(&mut self) -> Result<Ast> {
        let mut v = vec![self.parse_unary()?];
        loop {
            match self.peek() {
                Some(Tok::And) => {
                    self.pos += 1;
                    v.push(self.parse_unary()?);
                }
                // 相邻两项默认 AND：`tag=AeAlgo "Magic:"` 等价于中间有 and
                Some(Tok::Word(_)) | Some(Tok::Regex(_)) | Some(Tok::LParen) | Some(Tok::Not) => {
                    v.push(self.parse_unary()?);
                }
                _ => break,
            }
        }
        Ok(if v.len() == 1 {
            v.pop().unwrap()
        } else {
            Ast::And(v)
        })
    }

    fn parse_unary(&mut self) -> Result<Ast> {
        if self.peek() == Some(&Tok::Not) {
            self.pos += 1;
            return Ok(Ast::Not(Box::new(self.parse_unary()?)));
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<Ast> {
        match self.toks.get(self.pos).cloned() {
            Some(Tok::LParen) => {
                self.pos += 1;
                let inner = self.parse_or()?;
                if self.peek() != Some(&Tok::RParen) {
                    bail!("括号没闭合");
                }
                self.pos += 1;
                Ok(inner)
            }
            Some(Tok::Regex(r)) => {
                self.pos += 1;
                Ok(Ast::Regex(r))
            }
            Some(Tok::Word(w)) => {
                self.pos += 1;
                // 后面跟着运算符 → 这是个字段项
                if let Some(Tok::Op(op)) = self.toks.get(self.pos).cloned() {
                    self.pos += 1;
                    let (mut value, is_regex) = match self.toks.get(self.pos).cloned() {
                        Some(Tok::Word(v)) => {
                            self.pos += 1;
                            (v, false)
                        }
                        Some(Tok::Regex(v)) => {
                            self.pos += 1;
                            (v, true)
                        }
                        other => bail!("{w}{op} 后面缺少值，读到 {other:?}"),
                    };
                    // Time-of-day values contain `:` separators. The lexer
                    // keeps `:` as an operator for `is:log`, so fold the
                    // following `: word` pairs back into a time literal only
                    // for time comparisons.
                    if !is_regex
                        && matches!(w.to_ascii_lowercase().as_str(), "time" | "ts")
                        && matches!(op, "=" | "!=" | ">" | ">=" | "<" | "<=")
                    {
                        while self.peek() == Some(&Tok::Op(":")) {
                            self.pos += 1;
                            let Some(Tok::Word(part)) = self.toks.get(self.pos).cloned() else {
                                bail!("{w}{op} 后面的时间值不完整");
                            };
                            self.pos += 1;
                            value.push(':');
                            value.push_str(&part);
                        }
                    }
                    return Ok(Ast::Field {
                        name: w,
                        op,
                        value,
                        is_regex,
                    });
                }
                Ok(Ast::Text(w))
            }
            other => bail!("这里需要一个条件，读到 {other:?}"),
        }
    }
}

// ============================== 编译 ==============================

struct Compiler {
    opts: CompileOptions,
    lits: Vec<Vec<u8>>,
    ac_slots: Vec<usize>,
    res: Vec<String>,
    re_slots: Vec<usize>,
    fields: Vec<FieldTerm>,
    field_slots: Vec<usize>,
    next_slot: usize,
}

impl Compiler {
    fn new(opts: CompileOptions) -> Self {
        Self {
            opts,
            lits: Vec::new(),
            ac_slots: Vec::new(),
            res: Vec::new(),
            re_slots: Vec::new(),
            fields: Vec::new(),
            field_slots: Vec::new(),
            next_slot: 0,
        }
    }

    fn slot(&mut self) -> Result<usize> {
        if self.next_slot >= MAX_SLOTS {
            bail!("查询条件太多，最多 {MAX_SLOTS} 个");
        }
        self.next_slot += 1;
        Ok(self.next_slot - 1)
    }

    fn encode(&self, s: &str) -> Vec<u8> {
        match self.opts.encoding {
            Some(enc) => crate::matcher::encode_pattern(s, enc),
            None => s.as_bytes().to_vec(),
        }
    }

    fn compile(mut self, ast: Ast) -> Result<Query> {
        let root = self.lower(&ast)?;

        let ac = if self.lits.is_empty() {
            None
        } else {
            Some(
                AhoCorasickBuilder::new()
                    .match_kind(MatchKind::Standard)
                    .ascii_case_insensitive(!self.opts.case_sensitive)
                    .build(&self.lits)
                    .context("构建字面量自动机失败")?,
            )
        };

        let re = if self.res.is_empty() {
            None
        } else {
            Some(
                RegexSetBuilder::new(&self.res)
                    .case_insensitive(!self.opts.case_sensitive)
                    .build()
                    .context("构建正则集合失败")?,
            )
        };

        let mut re_mask = Bits::default();
        for &s in &self.re_slots {
            re_mask.set(s);
        }

        Ok(Query {
            root,
            ac,
            ac_slots: self.ac_slots,
            re,
            re_slots: self.re_slots,
            needs_fields: !self.fields.is_empty(),
            fields: self.fields,
            field_slots: self.field_slots,
            re_mask,
            empty: false,
        })
    }

    fn lower(&mut self, ast: &Ast) -> Result<Node> {
        Ok(match ast {
            Ast::And(v) => Node::And(v.iter().map(|a| self.lower(a)).collect::<Result<_>>()?),
            Ast::Or(v) => Node::Or(v.iter().map(|a| self.lower(a)).collect::<Result<_>>()?),
            Ast::Not(a) => Node::Not(Box::new(self.lower(a)?)),
            Ast::Text(s) => {
                if s.is_empty() {
                    return Ok(Node::Always(true));
                }
                let slot = self.slot()?;
                self.lits.push(self.encode(s));
                self.ac_slots.push(slot);
                Node::Slot(slot)
            }
            Ast::Regex(r) => {
                // 先单独编译一次，语法错误要在这里报出来而不是等 RegexSet 给个笼统的错
                RegexBuilder::new(r)
                    .case_insensitive(!self.opts.case_sensitive)
                    .build()
                    .with_context(|| format!("正则编译失败：/{r}/"))?;
                let slot = self.slot()?;
                self.res.push(r.clone());
                self.re_slots.push(slot);
                Node::Slot(slot)
            }
            Ast::Field {
                name,
                op,
                value,
                is_regex,
            } => {
                let term = self.field_term(name, op, value, *is_regex)?;
                let slot = self.slot()?;
                self.fields.push(term);
                self.field_slots.push(slot);
                Node::Slot(slot)
            }
        })
    }

    fn field_term(
        &self,
        name: &str,
        op: &'static str,
        value: &str,
        is_regex: bool,
    ) -> Result<FieldTerm> {
        let cmp = |op: &str| -> Result<CmpOp> {
            Ok(match op {
                "=" => CmpOp::Eq,
                "!=" => CmpOp::Ne,
                ">" => CmpOp::Gt,
                ">=" => CmpOp::Ge,
                "<" => CmpOp::Lt,
                "<=" => CmpOp::Le,
                _ => bail!("{name} 不支持运算符 {op}"),
            })
        };

        Ok(match name.to_ascii_lowercase().as_str() {
            "level" | "lvl" | "priority" => {
                let lv = Level::parse(value)
                    .ok_or_else(|| anyhow!("认不出日志级别 {value:?}，用 V/D/I/W/E/F 或全称"))?;
                FieldTerm::Level(cmp(op)?, lv)
            }
            "pid" => FieldTerm::Pid(
                cmp(op)?,
                value.parse().with_context(|| format!("pid 不是数字：{value}"))?,
            ),
            "tid" => FieldTerm::Tid(
                cmp(op)?,
                value.parse().with_context(|| format!("tid 不是数字：{value}"))?,
            ),
            "time" | "ts" => {
                let ts = parse_time_value(value, self.opts.base_date)
                    .ok_or_else(|| anyhow!("认不出时间 {value:?}，用 `MM-DD HH:MM:SS[.mmm]` 或 `HH:MM:SS`"))?;
                FieldTerm::Time(cmp(op)?, ts)
            }
            "tag" => FieldTerm::Tag(self.str_op(op, value, is_regex, name)?, op == "!=" || op == "!~"),
            "msg" | "message" | "text" => {
                FieldTerm::Msg(self.str_op(op, value, is_regex, name)?, op == "!=" || op == "!~")
            }
            "is" => match value.to_ascii_lowercase().as_str() {
                "log" | "structured" => FieldTerm::Structured,
                _ => bail!("is: 只支持 log"),
            },
            other => bail!("未知字段 {other}，可用：level / tag / pid / tid / time / msg / is"),
        })
    }

    fn str_op(&self, op: &str, value: &str, is_regex: bool, field: &str) -> Result<StrOp> {
        if is_regex || op == "~" || op == "!~" {
            let re = RegexBuilder::new(value)
                .case_insensitive(!self.opts.case_sensitive)
                .build()
                .with_context(|| format!("{field} 的正则编译失败：{value}"))?;
            return Ok(StrOp::Regex(re));
        }
        match op {
            "=" | "!=" => Ok(StrOp::Eq(self.encode(value))),
            ":" => {
                let ac = AhoCorasickBuilder::new()
                    .match_kind(MatchKind::Standard)
                    .ascii_case_insensitive(!self.opts.case_sensitive)
                    .build([self.encode(value)])
                    .context("构建子串匹配器失败")?;
                Ok(StrOp::Contains(ac))
            }
            _ => bail!("{field} 不支持运算符 {op}"),
        }
    }
}

/// 时间值。接受 `YYYY-MM-DD HH:MM[:SS[.mmm]]` / `MM-DD HH:MM[...]` / `HH:MM[...]`。
///
/// 只给时分秒时按 `base` 那一天算——UI 通常把文件第一行的时间戳传进来。
pub fn parse_time_value(s: &str, base: Option<Ts>) -> Option<Ts> {
    let s = s.trim();
    // 带日期的直接复用行解析器的时间戳分支
    if let Some((ts, used)) = try_parse_dated(s) {
        if used == s.len() {
            return Some(ts);
        }
    }
    // 纯时分秒：拿 base 的日期补上
    let (h, m, sec, ms) = parse_hms(s)?;
    let day_ms = 86_400_000i64;
    let base_day = base.map(|b| b.0.div_euclid(day_ms)).unwrap_or(0);
    Some(Ts(
        base_day * day_ms + (h as i64 * 3600 + m as i64 * 60 + sec as i64) * 1000 + ms as i64,
    ))
}

fn try_parse_dated(s: &str) -> Option<(Ts, usize)> {
    // 借用 logline 的解析：给它拼一个最小的合法行
    let probe = format!("{s}  1  2 I T: x");
    let l = logline::parse(probe.as_bytes())?;
    Some((l.ts?, s.len()))
}

fn parse_hms(s: &str) -> Option<(u32, u32, u32, u32)> {
    let (time, frac) = match s.split_once('.') {
        Some((t, f)) => (t, f),
        None => (s, ""),
    };
    let mut it = time.split(':');
    let h: u32 = it.next()?.parse().ok()?;
    let m: u32 = it.next()?.parse().ok()?;
    let sec: u32 = it.next().map_or(Ok(0), str::parse).ok()?;
    if it.next().is_some() || h > 23 || m > 59 || sec > 60 {
        return None;
    }
    let mut ms = 0u32;
    for (i, c) in frac.chars().take(3).enumerate() {
        ms = ms * 10 + c.to_digit(10)?;
        if i == frac.len().min(3) - 1 {
            for _ in frac.len()..3 {
                ms *= 10;
            }
        }
    }
    Some((h, m, sec, ms))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINE_D: &str = "01-02 03:04:05.678  1234  5678 D AeAlgo  : Magic: 42 gain=1024";
    const LINE_W: &str = "01-02 03:04:06.000  1234  9999 W Hal3Av3 : parseMeta noise here";
    const LINE_E: &str = "01-02 03:04:07.500  4321  5678 E AeAlgo  : takepicture failed";
    const PLAIN: &str = "just a plain line with Magic: inside";

    fn q(s: &str) -> Query {
        Query::parse(s, CompileOptions::default()).expect("查询应能编译")
    }

    fn hit(query: &Query, line: &str) -> bool {
        let mut sc = QueryScratch::default();
        query.matches(line.as_bytes(), &mut sc)
    }

    #[test]
    fn empty_query_matches_all() {
        let q = Query::parse("", CompileOptions::default()).unwrap();
        assert!(q.is_empty());
        assert!(hit(&q, PLAIN));
        assert!(!q.needs_fields());
    }

    #[test]
    fn bare_word_is_substring() {
        let q = q("Magic");
        assert!(hit(&q, LINE_D));
        assert!(hit(&q, PLAIN));
        assert!(!hit(&q, LINE_W));
    }

    #[test]
    fn case_insensitive_by_default() {
        assert!(hit(&q("magic"), LINE_D));
        let cs = Query::parse(
            "magic",
            CompileOptions {
                case_sensitive: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!hit(&cs, LINE_D));
    }

    #[test]
    fn quoted_keeps_spaces_and_specials() {
        let q = q(r#""Magic: 42""#);
        assert!(hit(&q, LINE_D));
        assert!(!hit(&q, PLAIN));
    }

    #[test]
    fn implicit_and_between_adjacent_terms() {
        let q = q(r#"Magic gain"#);
        assert!(hit(&q, LINE_D));
        assert!(!hit(&q, PLAIN), "PLAIN 只有 Magic 没有 gain");
    }

    #[test]
    fn explicit_and_or_not() {
        assert!(hit(&q("Magic and gain"), LINE_D));
        assert!(hit(&q("Magic or nothing"), LINE_D));
        assert!(!hit(&q("Magic and nothing"), LINE_D));
        assert!(hit(&q("not nothing"), LINE_D));
        assert!(!hit(&q("not Magic"), LINE_D));
        assert!(hit(&q("Magic && gain"), LINE_D));
        assert!(hit(&q("nothing || Magic"), LINE_D));
        assert!(!hit(&q("!Magic"), LINE_D));
    }

    #[test]
    fn parentheses_group() {
        let query = q("(nothing or Magic) and gain");
        assert!(hit(&query, LINE_D));
        let query = q("nothing or (Magic and nothing)");
        assert!(!hit(&query, LINE_D));
    }

    #[test]
    fn or_binds_looser_than_and() {
        // a and b or c  ==  (a and b) or c
        let q = q("Magic and nothing or parseMeta");
        assert!(!hit(&q, LINE_D));
        assert!(hit(&q, LINE_W));
    }

    #[test]
    fn level_comparison() {
        let q = q("level>=W");
        assert!(!hit(&q, LINE_D));
        assert!(hit(&q, LINE_W));
        assert!(hit(&q, LINE_E));
        assert!(q.needs_fields());
    }

    #[test]
    fn level_full_name_and_equality() {
        assert!(hit(&q("level=error"), LINE_E));
        assert!(!hit(&q("level=error"), LINE_W));
        assert!(hit(&q("level!=debug"), LINE_W));
    }

    #[test]
    fn missing_field_never_matches() {
        // 没有结构的行不该被 level>=V 捞进来
        assert!(!hit(&q("level>=V"), PLAIN));
        assert!(!hit(&q("pid=1234"), PLAIN));
    }

    #[test]
    fn tag_equality_is_exact() {
        assert!(hit(&q("tag=AeAlgo"), LINE_D));
        assert!(!hit(&q("tag=Ae"), LINE_D), "= 是整体相等，不是前缀");
        assert!(hit(&q("tag:Ae"), LINE_D), ": 才是包含");
    }

    #[test]
    fn tag_negation() {
        assert!(hit(&q("tag!=Hal3Av3"), LINE_D));
        assert!(!hit(&q("tag!=AeAlgo"), LINE_D));
    }

    #[test]
    fn pid_tid() {
        assert!(hit(&q("pid=1234"), LINE_D));
        assert!(hit(&q("tid=5678"), LINE_D));
        assert!(!hit(&q("pid=1234"), LINE_E));
        assert!(hit(&q("pid>2000"), LINE_E));
    }

    #[test]
    fn message_scoped_match() {
        // tag 里有 AeAlgo，但 msg 里没有
        assert!(!hit(&q("msg:AeAlgo"), LINE_D));
        assert!(hit(&q("msg:Magic"), LINE_D));
    }

    #[test]
    fn regex_on_whole_line() {
        let q = q(r"/gain\s*=\s*\d+/");
        assert!(hit(&q, LINE_D));
        assert!(!hit(&q, LINE_W));
    }

    #[test]
    fn regex_on_field() {
        let q = q(r"msg~/Magic:\s+\d+/");
        assert!(hit(&q, LINE_D));
        assert!(!hit(&q, LINE_W));
    }

    #[test]
    fn time_range() {
        let q = q(r#"time>="01-02 03:04:06" and time<"01-02 03:04:07""#);
        assert!(!hit(&q, LINE_D));
        assert!(hit(&q, LINE_W));
        assert!(!hit(&q, LINE_E));
    }

    #[test]
    fn time_of_day_uses_base_date() {
        let base = logline::parse(LINE_D.as_bytes()).unwrap().ts.unwrap();
        let q = Query::parse(
            "time>=03:04:06",
            CompileOptions {
                base_date: Some(base),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!hit(&q, LINE_D));
        assert!(hit(&q, LINE_W));
    }

    #[test]
    fn combined_field_and_text() {
        let q = q(r#"tag=AeAlgo and level>=E and takepicture"#);
        assert!(!hit(&q, LINE_D));
        assert!(hit(&q, LINE_E));
    }

    /// 回归：leftmost 语义会让 "bcd" 被 "abc" 吃掉，必须用 overlapping。
    #[test]
    fn overlapping_literals_both_reported() {
        let q = q("abc and bcd");
        assert!(hit(&q, "xxabcdxx"));
    }

    /// 正则延迟：`level>=E or /.../` 里，非 E 行仍要靠正则定夺，
    /// E 行则应直接短路。两条路径结果都得对。
    #[test]
    fn deferred_regex_still_correct() {
        let q = q(r"level>=E or /parse\w+/");
        assert!(hit(&q, LINE_E), "level 直接定论");
        assert!(hit(&q, LINE_W), "靠正则命中");
        assert!(!hit(&q, LINE_D));
    }

    #[test]
    fn deferred_regex_and_branch() {
        let q = q(r"level>=E and /takepicture/");
        assert!(hit(&q, LINE_E));
        assert!(!hit(&q, LINE_W), "level 不够，正则该被跳过且结论为假");
    }

    #[test]
    fn is_log_filters_unstructured() {
        let q = q("is:log");
        assert!(hit(&q, LINE_D));
        assert!(!hit(&q, PLAIN));
    }

    #[test]
    fn needs_fields_is_false_for_pure_text() {
        assert!(!q("Magic or gain").needs_fields());
        assert!(!q(r"/gain=\d+/").needs_fields());
        assert!(q("level>=W").needs_fields());
    }

    #[test]
    fn errors_are_reported() {
        let bad = [
            "(unclosed",
            "tag=",
            "level>=Z",
            "pid=abc",
            "unknownfield=1",
            r#""unterminated"#,
            "/unterminated",
            "and",
        ];
        for s in bad {
            assert!(
                Query::parse(s, CompileOptions::default()).is_err(),
                "{s:?} 应该报错"
            );
        }
    }

    #[test]
    fn escaped_quote_inside_string() {
        let q = q(r#""say \"hi\"""#);
        assert!(hit(&q, r#"he said "say "hi" loudly"#));
    }

    #[test]
    fn escaped_slash_inside_regex() {
        let q = q(r"/a\/b/");
        assert!(hit(&q, "xx a/b xx"));
    }

    #[test]
    fn gb18030_literals_are_encoded() {
        let (hay, _, _) = encoding_rs::GB18030.encode("曝光表 AEtable");
        let q = Query::parse(
            "曝光表",
            CompileOptions {
                encoding: Some(Encoding::Gb18030),
                ..Default::default()
            },
        )
        .unwrap();
        let mut sc = QueryScratch::default();
        assert!(q.matches(&hay, &mut sc));
    }

    #[test]
    fn parse_time_value_forms() {
        assert!(parse_time_value("01-02 03:04:05.678", None).is_some());
        assert!(parse_time_value("2024-01-02 03:04:05", None).is_some());
        assert!(parse_time_value("03:04", None).is_some());
        assert!(parse_time_value("03:04:05.5", None).is_some());
        assert_eq!(parse_time_value("garbage", None), None);
        assert_eq!(parse_time_value("25:00", None), None);
    }

    #[test]
    fn hms_fraction_is_padded() {
        let a = parse_time_value("03:04:05.5", None).unwrap();
        let b = parse_time_value("03:04:05.500", None).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn many_literals_share_one_pass() {
        // 32 个字面量 OR 在一起，仍然只有一个 AC
        let terms: Vec<String> = (0..32).map(|i| format!("kw{i}")).collect();
        let q = q(&terms.join(" or "));
        assert!(hit(&q, "line containing kw17 somewhere"));
        assert!(!hit(&q, "line containing none of them"));
    }

    #[test]
    fn too_many_slots_is_an_error() {
        let terms: Vec<String> = (0..MAX_SLOTS + 10).map(|i| format!("kw{i}")).collect();
        assert!(Query::parse(&terms.join(" or "), CompileOptions::default()).is_err());
    }
}
