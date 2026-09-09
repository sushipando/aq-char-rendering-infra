//! Bounded evaluation of the deterministic ActionScript used by timeline scripts.
//! No network, timers, game services, eval, or native code. Unknown reachable
//! behavior is an error; a catch cannot hide an evaluator limitation.
use crate::script::{self, Action, Command, Target, Token};
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RuntimeClass {
    pub(crate) name: String,
    pub(crate) fields: Vec<Vec<Token>>,
    pub(crate) methods: BTreeMap<String, Method>,
    pub(crate) frames: BTreeMap<usize, String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Method {
    pub parameters: Vec<Vec<Token>>,
    pub body: Vec<Token>,
}
impl RuntimeClass {
    pub fn has_click_listener(&self) -> bool {
        self.methods.values().any(|m|m.body.iter().any(|t|matches!(t.word(),Some("CLICK"|"MOUSE_DOWN"|"MOUSE_UP"))||matches!(t,Token::String(s) if matches!(s.as_str(),"click"|"mouseDown"|"mouseUp"))))
    }
    pub(crate) fn new(name: &str) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Value {
    Undefined,
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Object(String),
    Array(Vec<Value>),
    Host,
    Builtin(String),
}
impl Value {
    fn truth(&self) -> bool {
        match self {
            Self::Undefined | Self::Null => false,
            Self::Bool(v) => *v,
            Self::Number(v) => *v != 0.0 && !v.is_nan(),
            Self::String(v) => !v.is_empty(),
            _ => true,
        }
    }
    fn number(&self) -> Result<f64> {
        Ok(match self {
            Self::Number(n) => *n,
            Self::Bool(b) => u8::from(*b) as f64,
            Self::Null => 0.,
            Self::Undefined => f64::NAN,
            Self::String(s) => s.parse().unwrap_or(f64::NAN),
            _ => bail!("non-numeric timeline expression"),
        })
    }
    fn string(&self) -> String {
        match self {
            Self::Undefined => "undefined".into(),
            Self::Null => "null".into(),
            Self::Bool(v) => v.to_string(),
            Self::Number(v) => v.to_string(),
            Self::String(v) | Self::Object(v) | Self::Builtin(v) => v.clone(),
            Self::Array(v) => v.iter().map(Value::string).collect::<Vec<_>>().join(","),
            Self::Host => "@host".into(),
        }
    }
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub values: BTreeMap<String, Value>,
    // Event -> (receiver, handler). Registration order is observable.
    pub listeners: Vec<(String, String, String)>,
    pub initialized: bool,
    pub faults: Vec<String>,
}
#[derive(Clone, Debug, Default)]
pub struct ContextData {
    /// Name of this instance, then immediate parent, etc. Never inferred from class names.
    pub names: Vec<String>,
    pub children: Vec<String>,
    pub frame: usize,
    pub total_frames: usize,
    pub label: Option<String>,
    pub foreign: BTreeMap<String, Value>,
    pub labels: Option<std::collections::BTreeSet<String>>,
    pub old_labels: bool,
}
#[derive(Clone, Debug)]
enum Expr {
    Literal(Value),
    Name(String),
    Member(Box<Expr>, Box<Expr>),
    Call(Box<Expr>, Vec<Expr>),
    Unary(String, Box<Expr>),
    Binary(String, Box<Expr>, Box<Expr>),
    Conditional(Box<Expr>, Box<Expr>, Box<Expr>),
    Assign(Box<Expr>, Box<Expr>),
    New(String, Vec<Expr>),
}
#[derive(Clone, Debug)]
enum Stmt {
    Expression(Expr),
    Declare(String, String, Option<Expr>),
    If(Expr, Vec<Stmt>, Vec<Stmt>),
    Return(Option<Expr>),
    While(Expr, Vec<Stmt>),
    Try(Vec<Stmt>, Vec<Stmt>),
    Break,
}
struct Parser<'a> {
    tokens: &'a [Token],
    at: usize,
}
impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at)
    }
    fn punct(&mut self, c: char) -> bool {
        if self.peek() == Some(&Token::Punct(c)) {
            self.at += 1;
            true
        } else {
            false
        }
    }
    fn word(&mut self, w: &str) -> bool {
        if self.peek().and_then(Token::word) == Some(w) {
            self.at += 1;
            true
        } else {
            false
        }
    }
    fn need(&mut self, c: char) -> Result<()> {
        ensure!(self.punct(c), "expected {c:?} at token {}", self.at);
        Ok(())
    }
    fn name(&mut self) -> Result<String> {
        let s = self
            .peek()
            .and_then(Token::word)
            .context("expected identifier")?
            .to_owned();
        self.at += 1;
        Ok(s)
    }
    fn block(&mut self) -> Result<Vec<Stmt>> {
        if !self.punct('{') {
            return Ok(vec![self.statement()?]);
        }
        let mut out = Vec::new();
        while !self.punct('}') {
            ensure!(self.at < self.tokens.len(), "unclosed statement block");
            out.push(self.statement()?);
        }
        Ok(out)
    }
    fn statements(&mut self) -> Result<Vec<Stmt>> {
        let mut out = Vec::new();
        while self.at < self.tokens.len() {
            if self.punct(';') {
                continue;
            }
            out.push(self.statement()?);
        }
        Ok(out)
    }
    fn statement(&mut self) -> Result<Stmt> {
        if self.word("if") {
            self.need('(')?;
            let c = self.expr(0)?;
            self.need(')')?;
            let yes = self.block()?;
            let no = if self.word("else") {
                self.block()?
            } else {
                vec![]
            };
            return Ok(Stmt::If(c, yes, no));
        }
        if self.word("return") {
            let e = if self.peek().is_none() || matches!(self.peek(), Some(Token::Punct(';' | '}')))
            {
                None
            } else {
                Some(self.expr(0)?)
            };
            self.punct(';');
            return Ok(Stmt::Return(e));
        }
        if self.word("while") {
            self.need('(')?;
            let c = self.expr(0)?;
            self.need(')')?;
            return Ok(Stmt::While(c, self.block()?));
        }
        if self.word("try") {
            let body = self.block()?;
            ensure!(self.word("catch"), "try needs catch");
            self.need('(')?;
            self.name()?;
            self.need(':')?;
            if !self.punct('*') {
                self.name()?;
            }
            self.need(')')?;
            return Ok(Stmt::Try(body, self.block()?));
        }
        if self.word("break") {
            self.punct(';');
            return Ok(Stmt::Break);
        }
        if self.word("var") || self.word("const") {
            let n = self.name()?;
            let ty = if self.punct(':') {
                if self.punct('*') {
                    "*".into()
                } else {
                    self.name()?
                }
            } else {
                "*".into()
            };
            let e = if self.punct('=') {
                Some(self.expr(0)?)
            } else {
                None
            };
            self.punct(';');
            return Ok(Stmt::Declare(n, ty, e));
        }
        let e = self.expr(0)?;
        self.punct(';');
        Ok(Stmt::Expression(e))
    }
    fn arguments(&mut self) -> Result<Vec<Expr>> {
        let mut out = Vec::new();
        if self.punct(')') {
            return Ok(out);
        }
        loop {
            out.push(self.expr(0)?);
            if self.punct(')') {
                break;
            }
            self.need(',')?;
        }
        Ok(out)
    }
    fn operator(&self) -> Option<(String, u8, usize)> {
        let rest = &self.tokens[self.at..];
        for (s, p) in [
            ("===", 4),
            ("!==", 4),
            ("==", 4),
            ("!=", 4),
            ("<=", 5),
            (">=", 5),
            ("&&", 3),
            ("||", 2),
            ("+", 6),
            ("-", 6),
            ("*", 7),
            ("/", 7),
            ("%", 7),
            ("<", 5),
            (">", 5),
            ("=", 1),
        ] {
            if rest.len() >= s.len()
                && rest
                    .iter()
                    .zip(s.chars())
                    .all(|(t, c)| *t == Token::Punct(c))
            {
                return Some((s.into(), p, s.len()));
            }
        }
        for s in ["as", "is"] {
            if rest.first().and_then(Token::word) == Some(s) {
                return Some((s.into(), 5, 1));
            }
        }
        None
    }
    fn expr(&mut self, min: u8) -> Result<Expr> {
        let mut lhs = if self.punct('!') {
            Expr::Unary("!".into(), Box::new(self.expr(8)?))
        } else if self.punct('-') {
            Expr::Unary("-".into(), Box::new(self.expr(8)?))
        } else if self.punct('(') {
            let e = self.expr(0)?;
            self.need(')')?;
            e
        } else if self.word("new") {
            let n = self.name()?;
            let args = if self.punct('(') {
                self.arguments()?
            } else {
                vec![]
            };
            Expr::New(n, args)
        } else {
            let t = self.peek().context("missing expression")?.clone();
            self.at += 1;
            match t {
                Token::String(s) => Expr::Literal(Value::String(s)),
                Token::Word(s) => match s.as_str() {
                    "true" => Expr::Literal(Value::Bool(true)),
                    "false" => Expr::Literal(Value::Bool(false)),
                    "null" => Expr::Literal(Value::Null),
                    "undefined" => Expr::Literal(Value::Undefined),
                    _ => {
                        if let Ok(n) = s.parse::<f64>() {
                            Expr::Literal(Value::Number(n))
                        } else {
                            Expr::Name(s)
                        }
                    }
                },
                _ => bail!("unsupported expression token {t:?}"),
            }
        };
        loop {
            if self.punct('.') {
                let n = self.name()?;
                lhs = Expr::Member(Box::new(lhs), Box::new(Expr::Literal(Value::String(n))));
                continue;
            }
            if self.punct('[') {
                let n = self.expr(0)?;
                self.need(']')?;
                lhs = Expr::Member(Box::new(lhs), Box::new(n));
                continue;
            }
            if self.punct('(') {
                lhs = Expr::Call(Box::new(lhs), self.arguments()?);
                continue;
            }
            if self.tokens.get(self.at..self.at + 2)
                == Some(&[Token::Punct('+'), Token::Punct('+')])
            {
                self.at += 2;
                lhs = Expr::Assign(
                    Box::new(lhs.clone()),
                    Box::new(Expr::Binary(
                        "+".into(),
                        Box::new(lhs),
                        Box::new(Expr::Literal(Value::Number(1.))),
                    )),
                );
                continue;
            }
            if min == 0 && self.punct('?') {
                let yes = self.expr(0)?;
                self.need(':')?;
                let no = self.expr(0)?;
                lhs = Expr::Conditional(Box::new(lhs), Box::new(yes), Box::new(no));
                continue;
            }
            let Some((op, p, n)) = self.operator() else {
                break;
            };
            if p < min {
                break;
            }
            self.at += n;
            let rhs = self.expr(if op == "=" { p } else { p + 1 })?;
            lhs = if op == "=" {
                Expr::Assign(Box::new(lhs), Box::new(rhs))
            } else {
                Expr::Binary(op, Box::new(lhs), Box::new(rhs))
            };
        }
        Ok(lhs)
    }
}
#[derive(PartialEq)]
enum Flow {
    Continue,
    Return,
    Break,
}
pub struct Evaluator<'a> {
    class: &'a RuntimeClass,
    state: &'a mut State,
    context: &'a ContextData,
    locals: BTreeMap<String, Value>,
    pub commands: Vec<Command>,
    budget: usize,
    depth: usize,
}
impl<'a> Evaluator<'a> {
    pub fn new(class: &'a RuntimeClass, state: &'a mut State, context: &'a ContextData) -> Self {
        Self {
            class,
            state,
            context,
            locals: BTreeMap::new(),
            commands: vec![],
            budget: 100_000,
            depth: 0,
        }
    }
    fn work(&mut self) -> Result<()> {
        self.budget = self
            .budget
            .checked_sub(1)
            .context("script evaluation work limit exceeded")?;
        Ok(())
    }
    fn object_member(&mut self, path: &str, key: &str) -> Result<Value> {
        if let Some(receiver) = path.strip_prefix("@event:") {
            if matches!(key, "target" | "currentTarget") {
                return Ok(Value::Object(receiver.into()));
            }
        }
        if let Some(layer) = path.strip_prefix("@layer:") {
            if key == "visible" {
                return Ok(self
                    .state
                    .values
                    .get(&format!("@layer:{layer}.visible"))
                    .cloned()
                    .unwrap_or(Value::Bool(true)));
            }
        }
        if !path.is_empty() && path.split('.').all(|p| p == "parent") {
            let level = path.split('.').count();
            let holder = self.context.names.get(level).map(String::as_str);
            let layer = match (holder, key) {
                (Some("head"), "hair") => Some("hair"),
                (Some("head"), "helm") => Some("helm"),
                (Some("mcChar"), "head") => Some("head"),
                (Some("mcChar"), "weapon") => Some("weapon"),
                (Some("mcChar"), "weaponOff") => Some("weapon_off"),
                (Some("mcChar"), "backhair") => Some("backhair"),
                _ => None,
            };
            if let Some(layer) = layer {
                return Ok(Value::Object(format!("@layer:{layer}")));
            }
        }
        if path.is_empty() && self.class.methods.contains_key(&format!("get:{key}")) {
            return self.method(&format!("get:{key}"), vec![]);
        }
        if key == "parent" {
            return Ok(Value::Object(if path.is_empty() {
                "parent".into()
            } else {
                format!("{path}.parent")
            }));
        }
        let full = if path.is_empty() {
            key.into()
        } else {
            format!("{path}.{key}")
        };
        if let Some(v) = self.context.foreign.get(&full) {
            ensure!(
                self.commands.is_empty(),
                "reading an instance clock after a timeline call needs immediate evaluation"
            );
            return Ok(v.clone());
        }
        if let Some(v) = self.state.values.get(&full) {
            return Ok(v.clone());
        }
        if path.is_empty() {
            match key {
                "currentFrame" => return Ok(Value::Number(self.context.frame as f64)),
                "totalFrames" => return Ok(Value::Number(self.context.total_frames as f64)),
                "currentLabel" | "currentFrameLabel" => {
                    return Ok(self
                        .context
                        .label
                        .clone()
                        .map(Value::String)
                        .unwrap_or(Value::Null))
                }
                "numChildren" => return Ok(Value::Number(self.context.children.len() as f64)),
                "stage" | "root" => return Ok(Value::Host),
                _ => (),
            }
            if self.context.children.iter().any(|c| c == key) {
                return Ok(Value::Object(key.into()));
            }
            if self.class.methods.contains_key(key) {
                return Ok(Value::Builtin(key.into()));
            }
        }
        if key == "name" && (path.is_empty() || path.split('.').all(|p| p == "parent")) {
            let n = if path.is_empty() {
                0
            } else {
                path.split('.').count()
            };
            return Ok(Value::String(
                self.context
                    .names
                    .get(n)
                    .cloned()
                    .with_context(|| format!("missing ancestor context at level {n}"))?,
            ));
        }
        if matches!(key, "pAV" | "world" | "rootClass") {
            return Ok(Value::Host);
        }
        if key == "isMyAvatar" {
            return Ok(Value::Bool(false));
        }
        Ok(Value::Undefined)
    }
    fn name(&mut self, n: &str) -> Result<Value> {
        if let Some(v) = self.locals.get(n) {
            return Ok(v.clone());
        }
        if let Some(value) = self.state.values.get(n) {
            return Ok(value.clone());
        }
        match n {
            "this" => Ok(Value::Object("".into())),
            "parent" => Ok(Value::Object("parent".into())),
            "stage" | "root" => Ok(Value::Host),
            "Math" | "MovieClip" | "Boolean" | "int" | "uint" | "Number" | "String" | "Event"
            | "MouseEvent" | "Error" | "Dictionary" | "Object" => Ok(Value::Builtin(n.into())),
            _ => {
                let value = self.object_member("", n)?;
                ensure!(
                    value != Value::Undefined || self.state.values.contains_key(n),
                    "unknown identifier {n}"
                );
                Ok(value)
            }
        }
    }
    fn member(&mut self, obj: Value, key: &str) -> Result<Value> {
        match obj {
            Value::Object(path) => self.object_member(&path, key),
            Value::Host => Ok(Value::Host),
            Value::Builtin(p) => Ok(Value::Builtin(format!("{p}.{key}"))),
            Value::Array(v) => Ok(if key == "length" {
                Value::Number(v.len() as f64)
            } else {
                key.parse::<usize>()
                    .ok()
                    .and_then(|i| v.get(i).cloned())
                    .unwrap_or(Value::Undefined)
            }),
            Value::String(s) if key == "length" => Ok(Value::Number(s.len() as f64)),
            Value::Null | Value::Undefined => bail!("null reference accessing {key}"),
            _ => bail!("unsupported property {key}"),
        }
    }
    fn eval(&mut self, e: &Expr) -> Result<Value> {
        self.work()?;
        Ok(match e {
            Expr::Literal(v) => v.clone(),
            Expr::Name(n) => self.name(n)?,
            Expr::Member(o, k) => {
                let o = self.eval(o)?;
                let k = self.eval(k)?.string();
                self.member(o, &k)?
            }
            Expr::Unary(op, e) => {
                let v = self.eval(e)?;
                if op == "!" {
                    Value::Bool(!v.truth())
                } else {
                    Value::Number(-v.number()?)
                }
            }
            Expr::Binary(op, a, b) => {
                let a = self.eval(a)?;
                if op == "&&" && !a.truth() {
                    return Ok(a);
                }
                if op == "||" && a.truth() {
                    return Ok(a);
                }
                let b = self.eval(b)?;
                match op.as_str() {
                    "&&" | "||" => b,
                    "==" | "===" | "!=" | "!==" => {
                        let equal = a == b
                            || (matches!(op.as_str(), "==" | "!=")
                                && matches!(
                                    (&a, &b),
                                    (Value::Null, Value::Undefined)
                                        | (Value::Undefined, Value::Null)
                                ));
                        Value::Bool(equal ^ op.starts_with('!'))
                    }
                    "<" => Value::Bool(a.number()? < b.number()?),
                    "<=" => Value::Bool(a.number()? <= b.number()?),
                    ">" => Value::Bool(a.number()? > b.number()?),
                    ">=" => Value::Bool(a.number()? >= b.number()?),
                    "+" if matches!(a, Value::String(_)) || matches!(b, Value::String(_)) => {
                        Value::String(format!("{}{}", a.string(), b.string()))
                    }
                    "+" => Value::Number(a.number()? + b.number()?),
                    "-" => Value::Number(a.number()? - b.number()?),
                    "*" => Value::Number(a.number()? * b.number()?),
                    "/" => Value::Number(a.number()? / b.number()?),
                    "%" => Value::Number(a.number()? % b.number()?),
                    "as" => a,
                    "is" => Value::Bool(matches!(a, Value::Object(_) | Value::Host)),
                    _ => bail!("unsupported operator {op}"),
                }
            }
            Expr::Conditional(c, a, b) => {
                if self.eval(c)?.truth() {
                    self.eval(a)?
                } else {
                    self.eval(b)?
                }
            }
            Expr::Assign(lhs, rhs) => {
                let v = self.eval(rhs)?;
                self.assign(lhs, v.clone())?;
                v
            }
            Expr::Call(c, args) => {
                let args = args
                    .iter()
                    .map(|a| self.eval(a))
                    .collect::<Result<Vec<_>>>()?;
                match c.as_ref() {
                    Expr::Name(n) => self.call(Value::Object("".into()), n, args)?,
                    Expr::Member(o, k) => {
                        let o = self.eval(o)?;
                        let k = self.eval(k)?.string();
                        self.call(o, &k, args)?
                    }
                    _ => bail!("computed callback invocation is unsupported"),
                }
            }
            Expr::New(n, args) => {
                let values = args
                    .iter()
                    .map(|a| self.eval(a))
                    .collect::<Result<Vec<_>>>()?;
                if n == "Array" {
                    return Ok(Value::Array(
                        if let [Value::Number(n)] = values.as_slice() {
                            ensure!(
                                n.is_finite() && *n >= 0. && n.fract() == 0. && *n <= 65535.,
                                "invalid array length"
                            );
                            vec![Value::Undefined; *n as usize]
                        } else {
                            values
                        },
                    ));
                }
                ensure!(
                    matches!(n.as_str(), "Dictionary" | "Object"),
                    "unsupported constructed class {n}"
                );
                Value::Object(format!("@dictionary:{}", self.state.values.len()))
            }
        })
    }
    fn assign(&mut self, lhs: &Expr, v: Value) -> Result<()> {
        let (path, key) = match lhs {
            Expr::Name(n) if self.locals.contains_key(n) => {
                self.locals.insert(n.clone(), v);
                return Ok(());
            }
            Expr::Name(n) => ("".into(), n.clone()),
            Expr::Member(o, k) => {
                let obj = self.eval(o)?;
                let key = self.eval(k)?.string();
                match obj {
                    Value::Object(p) => (p, key),
                    Value::Host => return Ok(()),
                    _ => bail!("null reference in assignment"),
                }
            }
            _ => bail!("invalid assignment"),
        };
        if path.is_empty() && self.class.methods.contains_key(&format!("set:{key}")) {
            self.method(&format!("set:{key}"), vec![v])?;
            return Ok(());
        }
        ensure!(
            !matches!(
                key.as_str(),
                "x" | "y"
                    | "z"
                    | "scaleX"
                    | "scaleY"
                    | "rotation"
                    | "alpha"
                    | "filters"
                    | "transform"
                    | "blendMode"
                    | "mask"
            ),
            "runtime visual property {key} needs display-list evaluation"
        );
        let full = if path.is_empty() {
            key
        } else {
            format!("{path}.{key}")
        };
        self.state.values.insert(full, v);
        Ok(())
    }
    fn call(&mut self, obj: Value, name: &str, args: Vec<Value>) -> Result<Value> {
        self.work()?;
        let arg = |i: usize| args.get(i).cloned().unwrap_or(Value::Undefined);
        if obj == Value::Host {
            ensure!(
                !matches!(
                    name,
                    "copyAvatarMC" | "attachMovie" | "addChild" | "addChildAt"
                ),
                "null reference calling unavailable game-host visual service {name}"
            );
            return Ok(if name == "getChildAt" {
                Value::Host
            } else {
                Value::Undefined
            });
        } // Game UI/services are absent from the render host.
        if let Value::Builtin(ref p) = obj {
            if p == "Math" {
                let n = arg(0).number()?;
                return Ok(Value::Number(match name {
                    "ceil" => n.ceil(),
                    "floor" => n.floor(),
                    "round" => (n + 0.5).floor(),
                    "abs" => n.abs(),
                    "min" => n.min(arg(1).number()?),
                    "max" => n.max(arg(1).number()?),
                    "random" => 0.5,
                    _ => bail!("unsupported Math.{name}"),
                }));
            }
        }
        if let Value::String(ref s) = obj {
            return Ok(match name {
                "indexOf" => {
                    Value::Number(s.find(&arg(0).string()).map(|n| n as f64).unwrap_or(-1.))
                }
                "toLowerCase" => Value::String(s.to_lowercase()),
                "toUpperCase" => Value::String(s.to_uppercase()),
                _ => bail!("unsupported string method {name}"),
            });
        }
        let Value::Object(path) = obj else {
            bail!("null reference calling {name}");
        };
        if path.is_empty() {
            if self.class.methods.contains_key(name) {
                return self.method(name, args);
            }
            match name {
                "MovieClip" | "Object" => return Ok(arg(0)),
                "Boolean" => return Ok(Value::Bool(arg(0).truth())),
                "int" | "uint" => return Ok(Value::Number(arg(0).number()?.trunc())),
                "Number" => return Ok(Value::Number(arg(0).number()?)),
                "String" => return Ok(Value::String(arg(0).string())),
                "super" | "trace" => return Ok(Value::Undefined),
                "addFrameScript" => {
                    ensure!(
                        !args.is_empty() && args.len() % 2 == 0,
                        "invalid frame registration"
                    );
                    for pair in args.chunks(2) {
                        let n = pair[0].number()?;
                        ensure!(
                            n.is_finite() && n >= 0. && n < 65535.,
                            "invalid callback frame"
                        );
                        let callback = if self.class.methods.contains_key(&pair[1].string()) {
                            pair[1].clone()
                        } else {
                            ensure!(
                                !matches!(
                                    pair[1],
                                    Value::Object(_) | Value::Host | Value::Builtin(_)
                                ),
                                "noncallable frame object"
                            );
                            Value::Null
                        };
                        self.state
                            .values
                            .insert(format!("@frame:{}", n as usize + 1), callback);
                    }
                    return Ok(Value::Undefined);
                }
                _ => (),
            }
            if self.class.methods.contains_key(name) {
                return self.method(name, args);
            }
        }
        match name {
            "stop" | "play" | "gotoAndPlay" | "gotoAndStop" | "nextFrame" | "prevFrame" => {
                let action = match name {
                    "stop" => Action::Stop,
                    "play" => Action::Play,
                    "nextFrame" | "prevFrame" => {
                        ensure!(path.is_empty(), "relative child frame needs instance clock");
                        Action::Goto {
                            target: Target::Frame(if name == "nextFrame" {
                                (self.context.frame + 1).min(self.context.total_frames)
                            } else {
                                self.context.frame.saturating_sub(1).max(1)
                            }),
                            play: false,
                        }
                    }
                    _ => {
                        ensure!(
                            args.len() == 1
                                || args.len() == 2
                                    && matches!(arg(1), Value::Null | Value::Undefined),
                            "nondefault scene is unsupported"
                        );
                        let target = match arg(0) {
                            Value::String(s) => {
                                ensure!(
                                    !path.is_empty()
                                        || self.context.old_labels
                                        || s.parse::<usize>().is_ok()
                                        || self
                                            .context
                                            .labels
                                            .as_ref()
                                            .is_none_or(|labels| labels.contains(&s)),
                                    "unknown frame label {s:?}"
                                );
                                Target::Label(s)
                            }
                            v => {
                                let n = v.number()?;
                                ensure!(n.is_finite(), "non-finite frame target");
                                Target::Frame((n as usize).max(1))
                            }
                        };
                        Action::Goto {
                            target,
                            play: name == "gotoAndPlay",
                        }
                    }
                };
                self.commands.push(Command {
                    child: (!path.is_empty()).then_some(path),
                    action,
                });
            }
            "addEventListener" | "removeEventListener" => {
                let event = arg(0).string();
                let handler = arg(1).string();
                ensure!(
                    self.class.methods.contains_key(&handler),
                    "missing event callback {handler}"
                );
                ensure!(
                    event.starts_with("MouseEvent.")
                        || matches!(
                            event.as_str(),
                            "MouseEvent.CLICK"
                                | "click"
                                | "Event.FRAME_CONSTRUCTED"
                                | "frameConstructed"
                                | "Event.ENTER_FRAME"
                                | "enterFrame"
                                | "Event.EXIT_FRAME"
                                | "exitFrame"
                                | "Event.ADDED"
                                | "added"
                                | "Event.ADDED_TO_STAGE"
                                | "addedToStage"
                                | "Event.REMOVED_FROM_STAGE"
                                | "removedFromStage"
                        ),
                    "unsupported automatic event {event}"
                );
                let listener = (event, path, handler);
                let same = |(event, path, handler): &(String, String, String)| {
                    event_key(event) == event_key(&listener.0)
                        && *path == listener.1
                        && *handler == listener.2
                };
                if name == "removeEventListener" {
                    self.state.listeners.retain(|v| !same(v));
                } else if !self.state.listeners.iter().any(same) {
                    self.state.listeners.push(listener);
                }
            }
            "addAnimationListener" | "removeAnimationListener" => (), // Idle render host emits no combat/movement events.
            "hasEventListener" => {
                return Ok(Value::Bool(self.state.listeners.iter().any(
                    |(event, receiver, _)| {
                        event_key(event) == event_key(&arg(0).string()) && *receiver == path
                    },
                )))
            }
            "hasOwnProperty" => {
                return Ok(Value::Bool(self.state.values.contains_key(
                    &if path.is_empty() {
                        arg(0).string()
                    } else {
                        format!("{path}.{}", arg(0).string())
                    },
                )))
            }
            "getChildAt" => {
                ensure!(
                    path.is_empty(),
                    "ancestor child lookup needs instance graph"
                );
                let n = arg(0).number()? as usize;
                return Ok(Value::Object(
                    self.context
                        .children
                        .get(n)
                        .cloned()
                        .context("child index out of bounds")?,
                ));
            }
            _ => bail!("unsupported reachable method {path}.{name}"),
        }
        Ok(Value::Undefined)
    }
    fn execute(&mut self, body: &[Stmt]) -> Result<(Flow, Value)> {
        for s in body {
            self.work()?;
            match s {
                Stmt::Expression(e) => {
                    self.eval(e)?;
                }
                Stmt::Declare(n, ty, e) => {
                    let v = if let Some(e) = e {
                        self.eval(e)?
                    } else {
                        default_value(ty)
                    };
                    self.locals.insert(n.clone(), v);
                }
                Stmt::If(c, a, b) => {
                    let branch = if self.eval(c)?.truth() { a } else { b };
                    let r = self.execute(branch)?;
                    if r.0 != Flow::Continue {
                        return Ok(r);
                    }
                }
                Stmt::Return(e) => {
                    return Ok((
                        Flow::Return,
                        if let Some(e) = e {
                            self.eval(e)?
                        } else {
                            Value::Undefined
                        },
                    ))
                }
                Stmt::Break => return Ok((Flow::Break, Value::Undefined)),
                Stmt::While(c, b) => {
                    while self.eval(c)?.truth() {
                        let r = self.execute(b)?;
                        if r.0 == Flow::Return {
                            return Ok(r);
                        }
                        if r.0 == Flow::Break {
                            break;
                        }
                    }
                }
                Stmt::Try(body, catch) => match self.execute(body) {
                    Ok(r) if r.0 != Flow::Continue => return Ok(r),
                    Ok(_) => (),
                    Err(e)
                        if e.chain().any(|cause| {
                            cause.to_string().starts_with("null reference")
                                || cause.to_string().starts_with("unknown frame label")
                        }) =>
                    {
                        let r = self.execute(catch)?;
                        if r.0 != Flow::Continue {
                            return Ok(r);
                        }
                    }
                    Err(e) => return Err(e),
                },
            }
        }
        Ok((Flow::Continue, Value::Undefined))
    }
    fn callback(&mut self, name: &str, args: Vec<Value>) -> Result<()> {
        if let Err(error) = self.method(name, args) {
            if error.chain().any(|e| {
                e.to_string().starts_with("null reference")
                    || e.to_string().starts_with("unknown frame label")
            }) {
                let warning = format!("{error:#}");
                if !self.state.faults.contains(&warning) {
                    self.state.faults.push(warning);
                }
            } else {
                return Err(error);
            }
        }
        Ok(())
    }
    pub fn held_events(&mut self) -> Result<()> {
        self.event("Event.FRAME_CONSTRUCTED")?;
        self.event("Event.EXIT_FRAME")
    }
    pub fn method(&mut self, name: &str, args: Vec<Value>) -> Result<Value> {
        ensure!(self.depth < 32, "script call depth exceeded");
        let m = self
            .class
            .methods
            .get(name)
            .with_context(|| format!("missing method {name}"))?
            .clone();
        let old = std::mem::take(&mut self.locals);
        self.depth += 1;
        let result: Result<Value> = (|| {
            for (i, p) in m.parameters.iter().enumerate() {
                let n = p
                    .first()
                    .and_then(Token::word)
                    .context("invalid parameter")?;
                let v = if let Some(v) = args.get(i) {
                    v.clone()
                } else if let Some(at) = p.iter().position(|t| *t == Token::Punct('=')) {
                    let e = Parser {
                        tokens: &p[at + 1..],
                        at: 0,
                    }
                    .expr(0)?;
                    self.eval(&e)?
                } else {
                    Value::Undefined
                };
                self.locals.insert(n.into(), v);
            }
            let body = Parser {
                tokens: &m.body,
                at: 0,
            }
            .statements()?;
            Ok(self.execute(&body)?.1)
        })();
        self.depth -= 1;
        self.locals = old;
        result.with_context(|| format!("in {}.{name}", self.class.name))
    }
    pub fn initialize(&mut self) -> Result<()> {
        if self.state.initialized {
            return Ok(());
        }
        self.state.initialized = true;
        for field in &self.class.fields.clone() {
            let mut tokens = script::lex("var")?;
            tokens.extend(field.clone());
            tokens.push(Token::Punct(';'));
            let statements = Parser {
                tokens: &tokens,
                at: 0,
            }
            .statements()?;
            for s in statements {
                if let Stmt::Declare(n, ty, e) = s {
                    // Placed children are bound before the constructor; typed declarations do not erase them.
                    if self.context.children.contains(&n) {
                        continue;
                    }
                    let v = if let Some(e) = e {
                        self.eval(&e)?
                    } else {
                        default_value(&ty)
                    };
                    self.state.values.insert(n, v);
                }
            }
        }
        if self.class.methods.contains_key(&self.class.name) {
            self.method(&self.class.name, vec![])?;
        }
        Ok(())
    }
    pub fn event(&mut self, event: &str) -> Result<()> {
        let callbacks = self.state.listeners.clone();
        for (kind, receiver, handler) in callbacks {
            if event_key(&kind) == event_key(event) {
                self.callback(&handler, vec![Value::Object(format!("@event:{receiver}"))])?;
            }
        }
        Ok(())
    }
    pub fn animation_click(&mut self) -> Result<bool> {
        let mut receivers = Vec::new();
        for (event, receiver, _) in &self.state.listeners {
            if matches!(event_key(event).as_str(), "click" | "mousedown" | "mouseup")
                && !receivers.contains(receiver)
            {
                receivers.push(receiver.clone());
            }
        }
        for receiver in receivers {
            let original = self.state.clone();
            let start = self.commands.len();
            for event in ["mousedown", "mouseup", "click"] {
                for (kind, path, handler) in self.state.listeners.clone() {
                    if path == receiver && event_key(&kind) == event {
                        self.callback(&handler, vec![Value::Object(format!("@event:{receiver}"))])?;
                    }
                }
            }
            if self.commands.len() > start {
                return Ok(true);
            }
            *self.state = original;
        }
        Ok(false)
    }
    pub fn frame(&mut self) -> Result<()> {
        for field in &self.class.fields {
            if field
                .get(2)
                .and_then(Token::word)
                .is_some_and(|t| matches!(t, "MovieClip" | "SimpleButton"))
            {
                if let Some(name) = field.first().and_then(Token::word) {
                    self.state.values.insert(
                        name.into(),
                        if self.context.children.iter().any(|n| n == name) {
                            Value::Object(name.into())
                        } else {
                            Value::Null
                        },
                    );
                }
            }
        }
        self.event("Event.FRAME_CONSTRUCTED")?;
        match self
            .state
            .values
            .get(&format!("@frame:{}", self.context.frame))
            .cloned()
        {
            Some(Value::Null | Value::Undefined) => (),
            Some(v) => {
                self.callback(&v.string(), vec![])?;
            }
            None => {
                if let Some(name) = self.class.frames.get(&self.context.frame) {
                    self.callback(name, vec![])?;
                }
            }
        }
        self.event("Event.EXIT_FRAME")?;
        Ok(())
    }
}
fn event_key(event: &str) -> String {
    event
        .rsplit('.')
        .next()
        .unwrap_or(event)
        .replace('_', "")
        .to_lowercase()
}
fn default_value(ty: &str) -> Value {
    match ty {
        "Boolean" => Value::Bool(false),
        "int" | "uint" => Value::Number(0.),
        "Number" => Value::Number(f64::NAN),
        "*" => Value::Undefined,
        _ => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn run(source: &str, names: &[&str], click: bool) -> Result<(Vec<Command>, State)> {
        let class = crate::script::parse(source)?.unwrap().1.runtime.unwrap();
        let mut state = State::default();
        let context = ContextData {
            names: names.iter().map(|s| s.to_string()).collect(),
            children: vec!["button".into()],
            frame: 1,
            total_frames: 20,
            label: None,
            foreign: BTreeMap::new(),
            ..Default::default()
        };
        let mut eval = Evaluator::new(&class, &mut state, &context);
        eval.initialize()?;
        eval.frame()?;
        if click {
            eval.event("MouseEvent.CLICK")?;
        }
        Ok((eval.commands, state))
    }

    #[test]
    fn repeated_listener_registration_keeps_original_order() {
        let source="class C {function C(){addEventListener(MouseEvent.CLICK,a);addEventListener(MouseEvent.CLICK,b);addEventListener(MouseEvent.CLICK,a);}function a(e:Object){gotoAndStop(2);}function b(e:Object){gotoAndStop(3);}}";
        let (commands, _) = run(source, &["asset"], true).unwrap();
        assert_eq!(
            commands
                .iter()
                .map(|c| c.action.clone())
                .collect::<Vec<_>>(),
            vec![
                Action::Goto {
                    target: Target::Frame(2),
                    play: false
                },
                Action::Goto {
                    target: Target::Frame(3),
                    play: false
                }
            ]
        );
    }
    #[test]
    fn parent_labels_are_resolved_on_the_parent() {
        let runtime=crate::script::parse("class C {function C(){addFrameScript(0,frame1);}function frame1(){parent.gotoAndPlay(\"Idle\");}}").unwrap().unwrap().1.runtime.unwrap();
        let context = ContextData {
            frame: 1,
            total_frames: 1,
            labels: Some(Default::default()),
            ..Default::default()
        };
        let mut state = State::default();
        let mut e = Evaluator::new(&runtime, &mut state, &context);
        e.initialize().unwrap();
        e.frame().unwrap();
        assert_eq!(
            e.commands,
            vec![Command {
                child: Some("parent".into()),
                action: Action::Goto {
                    target: Target::Label("Idle".into()),
                    play: true
                }
            }]
        );
        assert!(state.faults.is_empty());
    }
    #[test]
    fn invalid_label_aborts_callback_before_later_visual_writes() {
        let runtime=crate::script::parse("class C {function C(){addFrameScript(0,frame1);}function frame1(){gotoAndPlay(\"missing\");visible=false;}}").unwrap().unwrap().1.runtime.unwrap();
        let context = ContextData {
            frame: 1,
            total_frames: 1,
            labels: Some(Default::default()),
            ..Default::default()
        };
        let mut state = State::default();
        let mut e = Evaluator::new(&runtime, &mut state, &context);
        e.initialize().unwrap();
        e.frame().unwrap();
        assert!(e.commands.is_empty());
        assert!(!state.values.contains_key("visible"));
        assert_eq!(state.faults.len(), 1);
    }
    #[test]
    fn absent_quest_service_and_arrays_have_real_values() {
        let (_,state)=run("class C {public var messages:*;function C(){addFrameScript(0,frame1);}function frame1(){messages=new Array(\"hello\",\"bye\");if(MovieClip(stage.getChildAt(0)).world.isQuestInProgress(12)){gotoAndPlay(5);}else{stop();}}}",&["asset","pet"],false).unwrap();
        assert!(matches!(state.values["messages"],Value::Array(ref values) if values.len()==2));
    }
    #[test]
    fn missing_child_aborts_authored_callback_but_unknown_operation_fails() {
        let (commands,state)=run("class C {public var absent:MovieClip;function C(){addFrameScript(0,frame1);}function frame1(){stop();absent.visible=false;play();}}",&["asset"],false).unwrap();
        assert_eq!(commands.len(), 1);
        assert_eq!(state.faults.len(), 1);
        assert!(run("class C {function C(){addFrameScript(0,frame1);}function frame1(){try{mystery();}catch(e:Error){stop();}}}",&["asset"],false).is_err());
    }
    #[test]
    fn parameters_conditionals_and_early_return_use_instance_context() {
        let s = r#"class C {public var started:Boolean; function C(){addFrameScript(0,frame1);} function choose(frame:int){if(started){return;}started=true;if(MovieClip(parent).name=="backshoulder"){gotoAndStop(frame+2);}else{stop();}}function frame1(){choose(3);}}"#;
        assert_eq!(
            run(s, &["child", "backshoulder"], false).unwrap().0[0].action,
            Action::Goto {
                target: Target::Frame(5),
                play: false
            }
        );
        assert_eq!(
            run(s, &["child", "frontshoulder"], false).unwrap().0[0].action,
            Action::Stop
        );
    }
    #[test]
    fn click_is_registered_then_dispatched_once_and_can_control_parent() {
        let s="class C {function C(){addFrameScript(0,frame1);} function frame1(){button.addEventListener(MouseEvent.CLICK,onClick,false,0,true);stop();}function onClick(e:MouseEvent){MovieClip(parent).play();}}";
        assert_eq!(run(s, &["c", "p"], false).unwrap().0.len(), 1);
        let commands = run(s, &["c", "p"], true).unwrap().0;
        assert_eq!(
            commands[1],
            Command {
                child: Some("parent".into()),
                action: Action::Play
            }
        );
    }
    #[test]
    fn automatic_frame_callbacks_execute_and_catches_do_not_hide_limits() {
        let s="class C {public var target:int=1;function C(){addEventListener(Event.FRAME_CONSTRUCTED,configured);addFrameScript(0,frame1);}function configured(e:Object){target=currentFrame+2;}function frame1(){gotoAndStop(target);}}";
        assert_eq!(
            run(s, &["c"], false).unwrap().0[0].action,
            Action::Goto {
                target: Target::Frame(3),
                play: false
            }
        );
        assert!(run(
            &s.replace(
                "target=currentFrame+2;",
                "try{unknownMethod();}catch(e:Error){}"
            ),
            &["c"],
            false
        )
        .is_err());
    }
    #[test]
    fn bounded_loops_and_short_circuit() {
        let s="class C {function C(){addFrameScript(0,f);}function f(){var n:int=0;while(n<3){n++;}if(false&&null.invalid()){play();}gotoAndStop(n);}}";
        assert_eq!(
            run(s, &["c"], false).unwrap().0[0].action,
            Action::Goto {
                target: Target::Frame(3),
                play: false
            }
        );
        assert!(run(&s.replace("n<3", "true"), &["c"], false).is_err());
    }
}
