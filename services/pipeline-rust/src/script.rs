//! Deliberately small, bounded parser for FFDec's decompiled frame callbacks.
//! This is not an ActionScript VM. Unsupported *reachable* control flow fails closed.
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Target {
    Frame(usize),
    Label(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Action {
    Stop,
    Play,
    /// Deliberate export policy: retain the first variation of a recognized
    /// uniform random-pose idiom; do not evaluate arbitrary expressions.
    FirstRandomPose,
    Goto {
        target: Target,
        play: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Command {
    pub child: Option<String>,
    pub action: Action,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Program {
    pub commands: Vec<Command>,
    pub unsupported: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Class {
    pub frames: BTreeMap<usize, Program>,
    pub constructor: Program,
    pub unsupported: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Token {
    Word(String),
    String(String),
    Punct(char),
}
impl Token {
    fn word(&self) -> Option<&str> {
        if let Self::Word(s) = self {
            Some(s)
        } else {
            None
        }
    }
}

fn lex(text: &str) -> Result<Vec<Token>> {
    ensure!(
        text.len() <= 4 * 1024 * 1024,
        "ActionScript file exceeds parser limit"
    );
    let mut chars = text.chars().peekable();
    let mut tokens = Vec::new();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => (),
            '/' if chars.peek() == Some(&'/') => {
                chars.next();
                for c in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut closed = false;
                while let Some(c) = chars.next() {
                    if c == '*' && chars.peek() == Some(&'/') {
                        chars.next();
                        closed = true;
                        break;
                    }
                }
                ensure!(closed, "unterminated ActionScript comment");
            }
            '\'' | '"' => {
                let mut s = String::new();
                let mut closed = false;
                while let Some(next) = chars.next() {
                    if next == c {
                        closed = true;
                        break;
                    }
                    if next == '\\' {
                        let escaped = chars.next().context("truncated string escape")?;
                        s.push(match escaped {
                            'n' => '\n',
                            'r' => '\r',
                            't' => '\t',
                            '\\' => '\\',
                            '\'' => '\'',
                            '"' => '"',
                            _ => bail!("unsupported ActionScript string escape"),
                        });
                    } else {
                        s.push(next);
                    }
                }
                ensure!(closed, "unterminated ActionScript string");
                tokens.push(Token::String(s));
            }
            c if c.is_alphanumeric() || c == '_' || c == '$' => {
                let mut s = c.to_string();
                while chars
                    .peek()
                    .is_some_and(|c| c.is_alphanumeric() || *c == '_' || *c == '$')
                {
                    s.push(chars.next().unwrap());
                }
                tokens.push(Token::Word(s));
            }
            c => tokens.push(Token::Punct(c)),
        }
        ensure!(tokens.len() <= 500_000, "ActionScript token limit exceeded");
    }
    Ok(tokens)
}

fn close(tokens: &[Token], start: usize, left: char, right: char) -> Result<usize> {
    ensure!(
        tokens.get(start) == Some(&Token::Punct(left)),
        "missing delimiter"
    );
    let mut depth = 0usize;
    for (i, t) in tokens.iter().enumerate().skip(start) {
        if *t == Token::Punct(left) {
            depth += 1;
        }
        if *t == Token::Punct(right) {
            depth = depth.checked_sub(1).context("unbalanced delimiters")?;
            if depth == 0 {
                return Ok(i);
            }
        }
        ensure!(depth <= 128, "ActionScript nesting limit exceeded");
    }
    bail!("unclosed ActionScript delimiter")
}

fn control(name: &str) -> bool {
    matches!(
        name,
        "stop"
            | "play"
            | "gotoAndStop"
            | "gotoAndPlay"
            | "nextFrame"
            | "prevFrame"
            | "nextScene"
            | "prevScene"
    )
}

fn uniform_random_pose(args: &[Token]) -> bool {
    let mut spelling = String::new();
    for t in args {
        match t {
            Token::Word(s) => spelling.push_str(s),
            Token::Punct(c) => spelling.push(*c),
            Token::String(_) => return false,
        }
    }
    let spelling = spelling.replace("this.totalFrames", "totalFrames");
    matches!(
        spelling.as_str(),
        "Math.round(Math.random()*(totalFrames-1)+1)"
            | "Math.floor(Math.random()*totalFrames)+1"
            | "int(Math.random()*totalFrames)+1"
    )
}

fn call(statement: &[Token]) -> Result<(Vec<&str>, &[Token])> {
    let open = statement
        .iter()
        .position(|t| *t == Token::Punct('('))
        .context("not a simple call")?;
    ensure!(
        close(statement, open, '(', ')')? == statement.len() - 1,
        "not a standalone call"
    );
    let mut path = Vec::new();
    for (i, t) in statement[..open].iter().enumerate() {
        if i % 2 == 0 {
            path.push(t.word().context("dynamic receiver")?);
        } else {
            ensure!(*t == Token::Punct('.'), "dynamic receiver");
        }
    }
    ensure!(open % 2 == 1, "invalid call receiver");
    Ok((path, &statement[open + 1..statement.len() - 1]))
}

fn compile(
    body: &[Token],
    methods: &BTreeMap<String, Vec<Token>>,
    active: &mut BTreeSet<String>,
) -> Result<Vec<Command>> {
    ensure!(active.len() <= 32, "frame helper recursion limit exceeded");
    let relevant = body.iter().any(|t| {
        t.word()
            .is_some_and(|w| control(w) || methods.contains_key(w))
    });
    if !relevant {
        return Ok(Vec::new());
    }
    // A callback containing control flow is not equivalent to an unconditional stop.
    ensure!(
        !body
            .iter()
            .any(|t| matches!(t, Token::Punct('{' | '}' | '?'))
                || t.word().is_some_and(|w| matches!(
                    w,
                    "if" | "else"
                        | "for"
                        | "while"
                        | "do"
                        | "switch"
                        | "return"
                        | "throw"
                        | "try"
                        | "catch"
                        | "with"
                ))),
        "conditional/early-exit frame control is unsupported"
    );
    let mut commands = Vec::new();
    for stmt in body
        .split(|t| *t == Token::Punct(';'))
        .filter(|s| !s.is_empty())
    {
        let relevant = stmt.iter().any(|t| {
            t.word()
                .is_some_and(|w| control(w) || methods.contains_key(w))
        });
        if !relevant {
            continue;
        }
        let (path, args) = call(stmt)?;
        let name = *path.last().context("missing method")?;
        let receiver = &path[..path.len() - 1];
        if !control(name) {
            ensure!(
                receiver.is_empty() || receiver == ["this"],
                "indirect frame helper is unsupported"
            );
            ensure!(args.is_empty(), "parameterized frame helper is unsupported");
            ensure!(active.insert(name.into()), "recursive frame helper {name}");
            commands.extend(compile(
                methods.get(name).context("unknown frame helper")?,
                methods,
                active,
            )?);
            active.remove(name);
            continue;
        }
        let child = match receiver {
            [] | ["this"] => None,
            [name] => Some((*name).to_owned()),
            ["this", name] => Some((*name).to_owned()),
            _ => bail!("indirect timeline receiver is unsupported"),
        };
        let action = match name {
            "stop" | "play" => {
                ensure!(args.is_empty(), "unexpected timeline arguments");
                if name == "stop" {
                    Action::Stop
                } else {
                    Action::Play
                }
            }
            "gotoAndStop" | "gotoAndPlay" => {
                if name == "gotoAndStop" && uniform_random_pose(args) {
                    commands.push(Command {
                        child,
                        action: Action::FirstRandomPose,
                    });
                    continue;
                }
                let target = match args {
                    [Token::String(s)] => Target::Label(s.clone()),
                    [Token::Word(n)] => {
                        Target::Frame(n.parse().context("nonliteral timeline target")?)
                    }
                    _ => bail!("dynamic target or scene argument is unsupported"),
                };
                Action::Goto {
                    target,
                    play: name == "gotoAndPlay",
                }
            }
            _ => bail!("unsupported timeline method {name}"),
        };
        commands.push(Command { child, action });
        ensure!(commands.len() <= 4096, "frame command limit exceeded");
    }
    Ok(commands)
}

fn program(body: &[Token], methods: &BTreeMap<String, Vec<Token>>) -> Program {
    match compile(body, methods, &mut BTreeSet::new()) {
        Ok(commands) => Program {
            commands,
            unsupported: None,
        },
        Err(e) => Program {
            commands: Vec::new(),
            unsupported: Some(e.to_string()),
        },
    }
}

pub fn parse(text: &str) -> Result<Option<(String, Class)>> {
    let tokens = lex(text)?;
    let Some(class_at) = tokens.iter().position(|t| t.word() == Some("class")) else {
        return Ok(None);
    };
    let class_name = tokens
        .get(class_at + 1)
        .and_then(Token::word)
        .context("missing class name")?;
    let mut package = String::new();
    if let Some(p) = tokens.iter().position(|t| t.word() == Some("package")) {
        for t in &tokens[p + 1..] {
            match t {
                Token::Word(s) => package.push_str(s),
                Token::Punct('.') => package.push('.'),
                _ => break,
            }
        }
    }
    let name = if package.is_empty() {
        class_name.to_owned()
    } else {
        format!("{package}.{class_name}")
    }
    .to_lowercase();
    let mut methods = BTreeMap::new();
    let mut at = class_at;
    while at < tokens.len() {
        if tokens[at].word() != Some("function") {
            at += 1;
            continue;
        }
        let Some(method) = tokens.get(at + 1).and_then(Token::word) else {
            bail!("missing function name");
        };
        let open = (at + 2..tokens.len())
            .find(|i| tokens[*i] == Token::Punct('{'))
            .context("missing function body")?;
        let end = close(&tokens, open, '{', '}')?;
        ensure!(
            methods
                .insert(method.into(), tokens[open + 1..end].to_vec())
                .is_none(),
            "duplicate function {method}"
        );
        at = end + 1;
    }
    let mut class = Class::default();
    let registrations = (|| -> Result<()> {
        let constructor = methods.get(class_name).map(Vec::as_slice).unwrap_or(&[]);
        let mut registrations = 0;
        for stmt in constructor
            .split(|t| *t == Token::Punct(';'))
            .filter(|s| s.iter().any(|t| t.word() == Some("addFrameScript")))
        {
            let (path, args) = call(stmt)
                .context("frame registration must be unconditional in the constructor")?;
            ensure!(
                path == ["addFrameScript"] || path == ["this", "addFrameScript"],
                "unsupported frame registration receiver"
            );
            registrations += 1;
            let args: Vec<_> = args.split(|t| *t == Token::Punct(',')).collect();
            ensure!(args.len() % 2 == 0, "invalid addFrameScript pairs");
            for pair in args.chunks(2) {
                let [Token::Word(index)] = pair[0] else {
                    bail!("dynamic addFrameScript index");
                };
                let frame = index
                    .parse::<usize>()?
                    .checked_add(1)
                    .context("frame overflow")?;
                ensure!(
                    frame <= u16::MAX as usize,
                    "frame callback exceeds SWF limit"
                );
                let method = match pair[1] {
                    [Token::Word(method)] => method,
                    [Token::Word(this), Token::Punct('.'), Token::Word(method)]
                        if this == "this" =>
                    {
                        method
                    }
                    _ => bail!("dynamic frame callback"),
                };
                let body = methods
                    .get(method)
                    .context("missing registered frame callback")?;
                ensure!(
                    class
                        .frames
                        .insert(frame, program(body, &methods))
                        .is_none(),
                    "duplicate frame registration"
                );
            }
        }
        ensure!(
            registrations
                == tokens
                    .iter()
                    .filter(|t| t.word() == Some("addFrameScript"))
                    .count(),
            "frame registration outside a direct constructor call is unsupported"
        );
        Ok(())
    })();
    if let Err(e) = registrations {
        class.unsupported = Some(e.to_string());
    }
    if let Some(body) = methods.get(class_name) {
        // Registrations refer to methods but do not execute them in the constructor.
        let filtered: Vec<_> = body
            .split(|t| *t == Token::Punct(';'))
            .filter(|s| !s.iter().any(|t| t.word() == Some("addFrameScript")))
            .flat_map(|s| s.iter().cloned().chain([Token::Punct(';')]))
            .collect();
        class.constructor = program(&filtered, &methods);
    }
    Ok(Some((name, class)))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn registered_callbacks_only_and_comments_are_not_code() {
        let (name, c) = parse(
            r#"package pet.fla { class Pet {
          function Pet() { addFrameScript(0,this.a,15,this.b,23,this.walk); }
          function a() { /* stop(); */ gotoAndPlay("Idle"); }
          function b() { var s = "if(foo) stop();"; this.hold(); }
          function hold() { this.stop(); }
          function walk() { if(this.moving) gotoAndPlay("Walk"); }
          function unrelated() { stop(); }
        }}"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(name, "pet.fla.pet");
        assert!(c.constructor.commands.is_empty());
        assert_eq!(c.frames[&16].commands[0].action, Action::Stop);
        assert!(c.frames[&24].unsupported.is_some());
        assert_eq!(c.frames.len(), 3);
    }
    #[test]
    fn dynamic_targets_indirect_controls_and_recursive_helpers_fail_closed() {
        for body in [
            "gotoAndStop(Math.random()*totalFrames);",
            "if(x) stop();",
            "this.a.b.stop();",
            "this.h();",
            "var f = stop;",
        ] {
            let text = format!("class C {{ function C() {{addFrameScript(0,this.f);}} function f() {{{body}}} function h() {{this.h();}} }}");
            assert!(
                parse(&text).unwrap().unwrap().1.frames[&1]
                    .unsupported
                    .is_some(),
                "{body}"
            );
        }
    }
    #[test]
    fn only_recognized_uniform_random_pose_idioms_use_first_variant_policy() {
        let class = parse("class C {function C(){addFrameScript(0,this.f);} function f(){gotoAndStop(Math.round(Math.random()*(this.totalFrames-1)+1));}} ").unwrap().unwrap().1;
        assert_eq!(class.frames[&1].commands[0].action, Action::FirstRandomPose);
        assert!(!uniform_random_pose(
            &lex("Math.round(Math.random()*(other.totalFrames-1)+1)").unwrap()
        ));
    }
    #[test]
    fn conditional_and_dynamic_frame_registration_are_not_treated_as_certain() {
        for constructor in [
            "if(x) addFrameScript(0,this.f);",
            "addFrameScript(frame,this.f);",
            "addFrameScript(0,callback);",
        ] {
            let class = parse(&format!(
                "class C {{function C(){{{constructor}}} function f(){{stop();}}}} "
            ))
            .unwrap()
            .unwrap()
            .1;
            assert!(class.unsupported.is_some(), "{constructor}");
        }
    }
}
