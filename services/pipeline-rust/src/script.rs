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
    /// Decoded AVM1 callbacks are queued after the current action block finishes.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub avm1_frames: BTreeMap<usize, Vec<crate::avm1::Control>>,
    #[serde(default)]
    pub animate_layer: Option<AnimateLayer>,
    #[serde(default)]
    pub hidden_in_hand: Option<String>,
    #[serde(default)]
    pub runtime: Option<crate::script_eval::RuntimeClass>,
    #[serde(default)]
    pub synchronized_children: Vec<String>,
    #[serde(default)]
    pub graphic_children: Vec<String>,
    pub frames: BTreeMap<usize, Program>,
    pub constructor: Program,
    pub unsupported: Option<String>,
}

/// Recognition is only a candidate; animate::validated_scripts proves the SWF
/// placements are equivalent before allowing any generated callbacks to be skipped.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AnimateLayer {
    Plain,
    Properties,
    Controller {
        pair: Option<(String, String)>,
    },
    Pairs {
        pairs: Vec<(String, String)>,
        #[serde(default)]
        graphics: Vec<String>,
        #[serde(default)]
        frame_limit: Option<usize>,
        #[serde(default)]
        masks: BTreeMap<String, String>,
    },
    Graphic {
        children: Vec<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Token {
    Word(String),
    Identifier(String),
    String(String),
    Punct(char),
}
impl Token {
    pub(crate) fn word(&self) -> Option<&str> {
        if let Self::Word(s) | Self::Identifier(s) = self {
            Some(s)
        } else {
            None
        }
    }
}

pub(crate) fn lex(text: &str) -> Result<Vec<Token>> {
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
            '§' => {
                // FFDec quotes identifiers that cannot be written as ordinary
                // ActionScript names. The delimiters are not part of the SWF name.
                ensure!(chars.peek() != Some(&'§'), "unsupported FFDec pseudoinstruction");
                let mut name = String::new();
                let mut closed = false;
                while let Some(c) = chars.next() {
                    if c == '§' { closed = true; break; }
                    if c == '\\' {
                        let escaped = chars.next().context("truncated identifier escape")?;
                        name.push(match escaped {
                            '\\' => '\\', '§' => '§',
                            'n' => '\n', 'r' => '\r', 't' => '\t',
                            _ => bail!("unsupported FFDec identifier escape"),
                        });
                    } else { name.push(c); }
                }
                ensure!(closed && !name.is_empty(), "unterminated or empty FFDec identifier");
                tokens.push(Token::Identifier(name));
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

/// Background export keeps authored colors. AVM1 content cannot call the AS3
/// host's MovieClip API; these guarded host hooks have no exported color effect.
/// Match complete decompiled programs, never a substring or arbitrary try body.
pub(crate) fn inert_background_avm1(text: &str) -> Result<bool> {
    let tokens = lex(text)?;
    for pattern in [
        r#"try { MovieClip(this.stage.getChildAt(0)).mcSetColor(this,"Trim","None"); } catch(e:Error) {}"#,
        "var isProp = true; mouseEnabled = false; mouseChildren = false;",
    ] {
        if tokens == lex(pattern)? { return Ok(true); }
    }
    // Linkage is descriptive metadata here, not a lookup or executable expression.
    // Require the entire setup program; only this one literal value may vary.
    let linkage = lex(r#"var isProp = true; var strLinkage = ""; mouseEnabled = false; mouseChildren = false;"#)?;
    Ok(tokens.len() == linkage.len() && tokens.iter().zip(&linkage).all(|(actual, expected)| {
        if matches!(expected, Token::String(_)) { matches!(actual, Token::String(_)) }
        else { actual == expected }
    }))
}

pub(crate) fn close(tokens: &[Token], start: usize, left: char, right: char) -> Result<usize> {
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
            Token::String(_) | Token::Identifier(_) => return false,
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

// Registering a callback does not execute it. Recognize only AQW's literal
// listener-registration block with an empty catch; leave all other try/control
// flow intact so reachable conditional timeline changes still fail closed.
fn listener_registration(stmt: &[Token]) -> bool {
    let Some(at) = stmt.iter().position(|t| t.word() == Some("addAnimationListener")) else { return false; };
    let receiver = &stmt[..at];
    let mut spelling = String::new();
    for token in receiver {
        match token {
            Token::Word(w) => spelling.push_str(w),
            Token::Punct(c) => spelling.push(*c),
            _ => return false,
        }
    }
    if spelling != "MovieClip(parent.parent.parent)." { return false; }
    if stmt.get(at + 1) != Some(&Token::Punct('(')) || stmt.last() != Some(&Token::Punct(')')) { return false; }
    let args = &stmt[at + 2..stmt.len() - 1];
    let valid = matches!(args, [Token::String(_), Token::Punct(','), Token::Word(this), Token::Punct('.'), Token::Word(_)] if this == "this");
    let valid_flag = matches!(args, [Token::String(_), Token::Punct(','), Token::Word(this), Token::Punct('.'), Token::Word(_), Token::Punct(','), Token::Word(flag)] if this == "this" && matches!(flag.as_str(), "true" | "false"));
    valid || valid_flag
}

// Literal click-listener registration stores a callback; it does not call it.
// Do not exempt frame/timer events, computed receivers, or callback expressions.
fn click_registration(stmt: &[Token]) -> bool {
    let Ok((path, args)) = call(stmt) else { return false; };
    if !matches!(path.as_slice(), ["this", _, "addEventListener"] | [_, "addEventListener"]) {
        return false;
    }
    let parts: Vec<_> = args.split(|t| *t == Token::Punct(',')).collect();
    if !matches!(parts.len(), 2 | 5) { return false; }
    if parts[0] != lex("MouseEvent.CLICK").unwrap() { return false; }
    let handler = parts[1];
    if !matches!(handler, [Token::Word(_)] | [Token::Word(_), Token::Punct('.'), Token::Word(_)])
        || (handler.len() == 3 && handler[0].word() != Some("this")) { return false; }
    parts.len() == 2 || (parts[2] == lex("false").unwrap()
        && parts[3] == lex("0").unwrap()
        && (parts[4] == lex("true").unwrap() || parts[4] == lex("false").unwrap()))
}

fn without_listener_setup(body: &[Token]) -> Result<Vec<Token>> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < body.len() {
        if body[at].word() == Some("try") && body.get(at + 1) == Some(&Token::Punct('{')) {
            let end = close(body, at + 1, '{', '}')?;
            let statements: Vec<_> = body[at + 2..end].split(|t| *t == Token::Punct(';')).filter(|s| !s.is_empty()).collect();
            if !statements.is_empty() && statements.iter().all(|s| listener_registration(s))
                && body.get(end + 1).and_then(Token::word) == Some("catch")
                && body.get(end + 2) == Some(&Token::Punct('(')) {
                let args_end = close(body, end + 2, '(', ')')?;
                let args = &body[end + 3..args_end];
                let simple_catch = matches!(args, [Token::Word(_), Token::Punct(':'), Token::Punct('*')])
                    || matches!(args, [Token::Word(_), Token::Punct(':'), Token::Word(kind)] if kind == "Error");
                if simple_catch && body.get(args_end + 1) == Some(&Token::Punct('{'))
                    && body.get(args_end + 2) == Some(&Token::Punct('}')) {
                    at = args_end + 3;
                    continue;
                }
            }
        }
        out.push(body[at].clone());
        at += 1;
    }
    Ok(out)
}

// Export policy: bank UI initialization has no rendered effect. Match the entire
// callback AND helper, not arbitrary initialization conditionals or try blocks.
// Registering onBankClick does not execute it; the final stop remains effective.
const BANK_IDLE: &str = "if(!this.petInit){this.petInit=true;this.initPet();}this.stop();";
const BANK_INIT: &str = r#"
    try {
        this.rootClass = stage.getChildAt(0) as MovieClip;
        MovieClip(parent).mouseEnabled = MovieClip(parent).mouseChildren = true;
        MovieClip(parent).buttonMode = true;
        this.btnBank.addEventListener(MouseEvent.CLICK,this.onBankClick,false,0,true);
        this.avatar = MovieClip(parent).pAV;
    } catch(e:Error) {}
"#;

// FFDec emits both explicit and implicit self references, including a mixture
// within one method. Only qualifiers at known self-reference positions may vary;
// never strip `this` globally (other.this or a different receiver is not self).
fn bank_pattern_matches(actual: &[Token], pattern: &str) -> bool {
    let expected = lex(pattern).expect("literal bank pattern");
    let (mut a, mut e) = (0, 0);
    while e < expected.len() {
        if expected[e].word() == Some("this")
            && expected.get(e + 1) == Some(&Token::Punct('.')) {
            e += 2;
            if actual.get(a).and_then(Token::word) == Some("this")
                && actual.get(a + 1) == Some(&Token::Punct('.')) {
                a += 2;
            }
        }
        if actual.get(a) != expected.get(e) { return false; }
        a += 1;
        e += 1;
    }
    a == actual.len()
}

const CAPE_BANK_IDLE: &str = "if(!this.capeInit){this.capeInit=true;this.initCape();}this.stop();this.capeInit=false;this.rootClass=MovieClip(stage.getChildAt(0));";

fn bank_setup_matches(
    body: &[Token],
    methods: &BTreeMap<String, Vec<Token>>,
    helper_name: &str,
    idle: &str,
    helper_pattern: &str,
    allow_empty: bool,
) -> bool {
    let Some(helper) = methods.get(helper_name) else {
        return false;
    };
    if !bank_pattern_matches(body, idle)
        || !(allow_empty && helper.is_empty() || bank_pattern_matches(helper, helper_pattern))
    {
        return false;
    }
    // Property accessors or locally shadowed host built-ins can execute code.
    !body
        .iter()
        .chain(helper.iter())
        .filter_map(Token::word)
        .any(|name| {
            methods.contains_key(&format!("get:{name}"))
                || methods.contains_key(&format!("set:{name}"))
        })
        && !["MovieClip", "stage", "stop"]
            .iter()
            .any(|name| methods.contains_key(*name))
}

fn bank_idle_setup(body: &[Token], methods: &BTreeMap<String, Vec<Token>>) -> bool {
    bank_setup_matches(body, methods, "initPet", BANK_IDLE, BANK_INIT, true)
}

fn bank_cape_idle_setup(body: &[Token], methods: &BTreeMap<String, Vec<Token>>) -> bool {
    if !methods.contains_key("initCape") {
        return false;
    }
    let helper = BANK_INIT.replace(
        "MovieClip(parent).pAV",
        "MovieClip(parent.parent.parent).pAV",
    );
    bank_setup_matches(body, methods, "initCape", CAPE_BANK_IDLE, &helper, false)
}

fn bank_child_idle_setup(
    body: &[Token],
    methods: &BTreeMap<String, Vec<Token>>,
) -> Option<Command> {
    if !methods.contains_key("initPet") {
        return None;
    }
    let end = body.iter().position(|t| *t == Token::Punct(';'))?;
    if !bank_idle_setup(&body[end + 1..], methods) {
        return None;
    }
    let (path, args) = call(&body[..end]).ok()?;
    let (child, control) = match path.as_slice() {
        ["this", child, control] | [child, control] => (*child, *control),
        _ => return None,
    };
    if !matches!(control, "gotoAndPlay" | "gotoAndStop")
        || methods.contains_key(&format!("get:{child}"))
        || methods.contains_key(&format!("set:{child}"))
    {
        return None;
    }
    let [Token::String(label)] = args else {
        return None;
    };
    if !label.eq_ignore_ascii_case("idle") {
        return None;
    }
    Some(Command {
        child: Some(child.into()),
        action: Action::Goto {
            target: Target::Label(label.clone()),
            play: control == "gotoAndPlay",
        },
    })
}

// There is no game attack dispatcher in the rendered idle view. Registering
// with that optional host does not trigger an attack; preserve the final stop.
fn idle_attack_registration(body: &[Token], methods: &BTreeMap<String, Vec<Token>>) -> bool {
    const SETUP: &str = r#"
        this._stage = stage ? MovieClip(stage.getChildAt(0)) : null;
        if(Boolean(this._stage) && Boolean(this._stage.registerAttackFrame)) {
            this._stage.registerAttackFrame(this);
        }
        this.stop();
    "#;
    body.iter().any(|t| t.word() == Some("registerAttackFrame"))
        && bank_pattern_matches(body, SETUP)
        && ![
            "_stage",
            "stage",
            "MovieClip",
            "Boolean",
            "registerAttackFrame",
            "stop",
        ]
        .iter()
        .any(|name| {
            methods.contains_key(*name)
                || methods.contains_key(&format!("get:{name}"))
                || methods.contains_key(&format!("set:{name}"))
        })
}

// Export policy: choose phase 1 for a once-only randomized looping effect.
// Compare the complete class, including an undefined flag and the sole frame-1
// registration. This rules out flag resets, extra controls and custom helpers.
fn deterministic_loop_phase(
    tokens: &[Token],
    class_at: usize,
    class_name: &str,
    methods: &BTreeMap<String, Vec<Token>>,
) -> Option<String> {
    if methods.len() != 2 {
        return None;
    }
    let callback = methods.keys().find(|name| name.as_str() != class_name)?;
    let field=tokens.windows(6).find_map(|w| matches!(w,
        [Token::Word(public),Token::Word(var),Token::Word(_),Token::Punct(':'),Token::Punct('*'),Token::Punct(';')]
        if public=="public" && var=="var").then(||w[2].word()).flatten())?;
    // Template insertion is only for ordinary identifier spellings. Quoted or
    // punctuated names remain supported by the normal parser, not this policy.
    if [class_name, callback.as_str(), field].iter().any(|name| {
        !name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '$')
    }) {
        return None;
    }
    if [class_name, callback.as_str(), field].iter().any(|name| {
        matches!(
            *name,
            "Math" | "totalFrames" | "gotoAndPlay" | "undefined" | "play" | "addFrameScript"
        )
    }) {
        return None;
    }
    let open = (class_at + 2..tokens.len()).find(|i| tokens[*i] == Token::Punct('{'))?;
    let end = close(tokens, open, '{', '}').ok()?;
    let pattern = format!(
        r#"{class_name} extends MovieClip {{
        public var {field}:*;
        public function {class_name}(){{super(); addFrameScript(0,this.{callback});}}
        internal function {callback}():*{{
            if(this.{field} == undefined){{this.{field}=true; this.gotoAndPlay(Math.ceil(Math.random()*this.totalFrames));}}
        }}
    }}"#
    );
    bank_pattern_matches(&tokens[class_at + 1..=end], &pattern).then(|| callback.clone())
}

fn compile(
    body: &[Token],
    methods: &BTreeMap<String, Vec<Token>>,
    active: &mut BTreeSet<String>,
) -> Result<Vec<Command>> {
    ensure!(active.len() <= 32, "frame helper recursion limit exceeded");
    if idle_attack_registration(body, methods) {
        return Ok(vec![Command {
            child: None,
            action: Action::Stop,
        }]);
    }
    if let Some(child) = bank_child_idle_setup(body, methods) {
        return Ok(vec![
            child,
            Command {
                child: None,
                action: Action::Stop,
            },
        ]);
    }
    if bank_idle_setup(body, methods) || bank_cape_idle_setup(body, methods) {
        return Ok(vec![Command {
            child: None,
            action: Action::Stop,
        }]);
    }
    ensure!(
        !body.iter().any(|t| t
            .word()
            .is_some_and(|w| matches!(w, "visible" | "copyAvatarMC"))),
        "visual script needs instance evaluation"
    );
    let cleaned = without_listener_setup(body)?;
    let body = cleaned.as_slice();
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
        if click_registration(stmt) {
            continue;
        }
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

/// Recognize only the complete first-frame hand-holder visibility idiom.
fn hidden_in_hand(body: &[Token]) -> Option<String> {
    for hand in ["fronthand", "backhand"] {
        for target in ["visible", "this.visible"] {
            let expected = lex(&format!("if(MovieClip(parent.parent).name == \"{hand}\") {{{target} = false;}}" )).ok()?;
            if body == expected { return Some(hand.into()); }
        }
    }
    None
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

fn layer_shape(tokens: &[Token]) -> Option<(Vec<Token>, Option<(String, String)>)> {
    let at = tokens.iter().position(|t| *t == Token::Word("class".into()))?;
    let name = tokens.get(at+1)?.word()?;
    let mut names = BTreeMap::from([(name.to_owned(), "CanonicalClass".to_owned())]);
    let vars: Vec<_> = tokens.windows(5).filter_map(|w|
        (w[0].word()==Some("public") && w[1].word()==Some("var") && w[3]==Token::Punct(':') && w[4].word()==Some("MovieClip"))
        .then(||w[2].word().map(str::to_owned)).flatten()).collect();
    let pair = if vars.len() == 2 {
        let prop = vars.iter().find(|n|n.ends_with("_prop_"))?;
        let object = prop.strip_suffix("_prop_")?;
        if !vars.iter().any(|n|n==object) {return None;}
        names.insert(object.into(), "layer".into());
        names.insert(prop.clone(), "layer_prop_".into());
        let setters: Vec<_> = tokens.windows(2).filter_map(|w|
            (w[0].word()==Some("function")).then(||w[1].word()).flatten())
            .filter(|n|n.starts_with("__setProp_")).collect();
        if setters.len()!=2 {return None;}
        // Generated setters occur in object/property declaration order. Their
        // complete bodies are checked by the canonical template comparison.
        names.insert(setters[0].into(), "initObject".into());
        names.insert(setters[1].into(), "initProperties".into());
        Some((object.into(),prop.clone()))
    } else if vars.is_empty() { None } else {return None;};
    let open = (at+2..tokens.len()).find(|i|tokens[*i]==Token::Punct('{'))?;
    let end = close(tokens,open,'{','}').ok()?;
    let canonical = tokens[at+1..=end].iter().map(|t| match t {
        Token::Word(w) | Token::Identifier(w) if names.contains_key(w) => Token::Word(names[w].clone()),
        t => t.clone(),
    }).collect();
    Some((canonical,pair))
}

fn recognize_animate_layer(tokens: &[Token]) -> Option<AnimateLayer> {
    let (actual, pair) = layer_shape(tokens)?;
    static LEGACY: std::sync::OnceLock<Vec<Token>> = std::sync::OnceLock::new();
    let legacy = LEGACY.get_or_init(|| {
        layer_shape(&lex(include_str!("../assets/animate/layer-runtime-legacy.as")).unwrap())
            .unwrap()
            .0
    });
    if &actual == legacy {
        return Some(AnimateLayer::Properties);
    }
    static SHAPES: std::sync::OnceLock<Vec<Vec<Token>>> = std::sync::OnceLock::new();
    let shapes = SHAPES.get_or_init(|| {
        [
            include_str!("../assets/animate/layer-runtime.as"),
            include_str!("../assets/animate/controller.as"),
            include_str!("../assets/animate/controller-flat-layer.as"),
            "class C extends MovieClip {public function C(){super();}}",
        ]
        .iter()
        .map(|s| {
            layer_shape(&lex(s).expect("Animate template syntax"))
                .expect("Animate template shape")
                .0
        })
        .collect()
    });
    for (index, expected) in shapes.iter().enumerate() {
        if &actual == expected {
            return Some(match index {
                0 => AnimateLayer::Properties,
                1 => AnimateLayer::Controller { pair: None },
                2 => AnimateLayer::Controller { pair },
                _ => AnimateLayer::Plain,
            });
        }
    }
    None
}

pub fn parse(text: &str) -> Result<Option<(String, Class)>> {
    let mut tokens = lex(text)?;
    let Some(class_at) = tokens
        .iter()
        .position(|t| *t == Token::Word("class".into()))
    else {
        return Ok(None);
    };
    let package_at = tokens
        .iter()
        .position(|t| *t == Token::Word("package".into()));
    // Once declaration keywords are located, quoted and unquoted references
    // must resolve to the same underlying identifier throughout compilation.
    for token in &mut tokens {
        if let Token::Identifier(name) = token {
            *token = Token::Word(std::mem::take(name));
        }
    }
    let class_name = tokens
        .get(class_at + 1)
        .and_then(Token::word)
        .context("missing class name")?;
    let mut package = String::new();
    if let Some(p) = package_at {
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
    let mut runtime = crate::script_eval::RuntimeClass::new(class_name);
    let mut at = class_at;
    while at < tokens.len() {
        if tokens[at].word() != Some("function") {
            if matches!(tokens[at].word(), Some("var" | "const")) {
                if let Some(end) = tokens[at..].iter().position(|t| *t == Token::Punct(';')) {
                    runtime.fields.push(tokens[at + 1..at + end].to_vec());
                    at += end + 1;
                    continue;
                }
            }
            at += 1;
            continue;
        }
        let Some(method) = tokens.get(at + 1).and_then(Token::word) else {
            bail!("missing function name");
        };
        let accessor =
            matches!(method, "get" | "set") && tokens.get(at + 2).and_then(Token::word).is_some();
        let method_key = if accessor {
            format!("{method}:{}", tokens[at + 2].word().unwrap())
        } else {
            method.to_owned()
        };
        let open = (at + 2..tokens.len())
            .find(|i| tokens[*i] == Token::Punct('{'))
            .context("missing function body")?;
        let end = close(&tokens, open, '{', '}')?;
        let args_open = (at + 2..open)
            .find(|i| tokens[*i] == Token::Punct('('))
            .context("missing method arguments")?;
        let args_end = close(&tokens, args_open, '(', ')')?;
        runtime.methods.insert(
            method_key.clone(),
            crate::script_eval::Method {
                parameters: tokens[args_open + 1..args_end]
                    .split(|t| *t == Token::Punct(','))
                    .filter(|p| !p.is_empty())
                    .map(|p| p.to_vec())
                    .collect(),
                body: tokens[open + 1..end].to_vec(),
            },
        );
        ensure!(
            methods
                .insert(method_key.clone(), tokens[open + 1..end].to_vec())
                .is_none(),
            "duplicate function {method_key}"
        );
        if accessor {
            // Property evaluation is not a helper call. Reject a reachable use
            // rather than silently discarding possible getter/setter effects.
            methods
                .entry(tokens[at + 2].word().unwrap().to_owned())
                .or_insert_with(|| vec![Token::Word("throw".into()), Token::Word("stop".into())]);
        }
        at = end + 1;
    }
    if let Some(callback) = deterministic_loop_phase(&tokens, class_at, class_name, &methods) {
        methods.insert(callback, lex("play();")?);
    }
    let mut class = Class {
        animate_layer: recognize_animate_layer(&tokens),
        ..Class::default()
    };
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
                runtime.frames.insert(frame, method.clone());
                if frame == 1 {
                    class.hidden_in_hand = hidden_in_hand(body);
                }
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
    if class.frames.len() != 1 || class.unsupported.is_some() {
        class.hidden_in_hand = None;
    }
    if class.animate_layer.is_none() {
        class.animate_layer = crate::animate::classify_generated(&runtime);
    }
    class.runtime = Some(runtime);
    Ok(Some((name, class)))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn once_only_random_phase_keeps_looping_and_rejects_modified_classes() {
        let source="class Flame extends MovieClip {public var started:*; public function Flame(){super(); addFrameScript(0,this.frame1);} internal function frame1():*{if(this.started==undefined){this.started=true; gotoAndPlay(Math.ceil(Math.random()*totalFrames));}}}";
        let (_, class) = parse(source).unwrap().unwrap();
        assert!(class.unsupported.is_none());
        assert_eq!(
            class.frames[&1].commands,
            vec![Command {
                child: None,
                action: Action::Play
            }]
        );
        for changed in [
            source.replace("started:*;", "started:*=false;"),
            source.replace("started=true", "started=false"),
            source.replace("gotoAndPlay", "gotoAndStop"),
            source.replace("Math.ceil", "Math.floor"),
            source.replace("started", "Math"),
            source.replace("this.started=true;", "this.started=true; stop();"),
        ] {
            let class = parse(&changed).unwrap().unwrap().1;
            assert!(class.frames[&1].unsupported.is_some(), "{changed}");
        }
    }

    #[test]
    fn attack_host_registration_preserves_idle_stop_only() {
        let setup="this._stage=stage?MovieClip(stage.getChildAt(0)):null;if(Boolean(this._stage)&&Boolean(this._stage.registerAttackFrame)){this._stage.registerAttackFrame(this);}stop();";
        let wrap = |body: &str| {
            format!("class W {{function W(){{addFrameScript(0,this.frame1);}} function frame1(){{{body}}}}}")
        };
        let class = parse(&wrap(setup)).unwrap().unwrap().1;
        assert_eq!(
            class.frames[&1].commands,
            vec![Command {
                child: None,
                action: Action::Stop
            }]
        );
        for changed in [
            setup.replace("stop();", "play();"),
            setup.replace("registerAttackFrame", "doAnything"),
            format!("{setup} gotoAndPlay(3);"),
        ] {
            assert!(parse(&wrap(&changed)).unwrap().unwrap().1.frames[&1]
                .unsupported
                .is_some());
        }
    }

    #[test]
    fn bank_cape_setup_preserves_stop_and_rejects_side_effects() {
        let helper = BANK_INIT.replace(
            "MovieClip(parent).pAV",
            "MovieClip(parent.parent.parent).pAV",
        );
        let source = |body: &str, init: &str| {
            format!("class Cape {{function Cape(){{addFrameScript(0,this.frame1);}}function frame1(){{{body}}}function initCape(){{{init}}}}}")
        };
        for body in [
            CAPE_BANK_IDLE.to_owned(),
            CAPE_BANK_IDLE.replace("this.", ""),
        ] {
            let class = parse(&source(&body, &helper)).unwrap().unwrap().1;
            assert_eq!(
                class.frames[&1].commands,
                vec![Command {
                    child: None,
                    action: Action::Stop
                }]
            );
        }
        for (body, init) in [
            (format!("{CAPE_BANK_IDLE} visible=false;"), helper.clone()),
            (
                CAPE_BANK_IDLE.to_owned(),
                helper.replace("buttonMode = true", "visible = false"),
            ),
            (
                CAPE_BANK_IDLE.to_owned(),
                helper.replace("catch(e:Error) {}", "catch(e:Error) {gotoAndPlay(2);}"),
            ),
        ] {
            assert!(parse(&source(&body, &init)).unwrap().unwrap().1.frames[&1]
                .unsupported
                .is_some());
        }
    }

    #[test]
    fn bank_child_idle_control_is_preserved_not_discarded() {
        let source = |prefix: &str, extra: &str| {
            format!("class Pet {{function Pet(){{addFrameScript(0,this.frame1);}}function frame1(){{{prefix}{BANK_IDLE}}}function initPet(){{{BANK_INIT}}}{extra}}}")
        };
        let class = parse(&source("this.CCPet.gotoAndPlay(\"Idle\");", ""))
            .unwrap()
            .unwrap()
            .1;
        assert_eq!(
            class.frames[&1].commands,
            vec![
                Command {
                    child: Some("CCPet".into()),
                    action: Action::Goto {
                        target: Target::Label("Idle".into()),
                        play: true
                    }
                },
                Command {
                    child: None,
                    action: Action::Stop
                }
            ]
        );
        for (prefix, extra) in [
            ("this.CCPet.gotoAndPlay(target);", ""),
            ("this.CCPet.gotoAndPlay(\"Walk\");", ""),
            (
                "this.CCPet.gotoAndPlay(\"Idle\");this.other.gotoAndPlay(2);",
                "",
            ),
            (
                "this.CCPet.gotoAndPlay(\"Idle\");",
                "function get CCPet(){gotoAndPlay(2);}",
            ),
        ] {
            assert!(parse(&source(prefix, extra)).unwrap().unwrap().1.frames[&1]
                .unsupported
                .is_some());
        }
    }

    #[test]
    fn empty_pet_initializer_cannot_hide_render_changes() {
        let source="class Pet {function Pet(){addFrameScript(0,this.frame1);} function initPet(){} function frame1(){if(!petInit){petInit=true;initPet();}stop();}}";
        assert_eq!(
            parse(source).unwrap().unwrap().1.frames[&1].commands,
            vec![Command {
                child: None,
                action: Action::Stop
            }]
        );
        assert!(parse(&source.replace(
            "function initPet(){}",
            "function initPet(){gotoAndPlay(2);}"
        ))
        .unwrap()
        .unwrap()
        .1
        .frames[&1]
            .unsupported
            .is_some());
    }

    #[test]
    fn click_callback_is_not_executed_but_stop_is_preserved() {
        let source = |registration: &str| format!("class Button {{function Button(){{addFrameScript(0,this.frame1);}} function frame1(){{{registration}; stop();}} function onClick(){{MovieClip(parent).play();}}}}");
        let class = parse(&source("this.btnButton.addEventListener(MouseEvent.CLICK,this.onClick,false,0,true)")).unwrap().unwrap().1;
        assert!(class.frames[&1].unsupported.is_none());
        assert_eq!(class.frames[&1].commands, vec![Command{child:None, action:Action::Stop}]);
        for registration in [
            "this.btnButton.addEventListener(Event.ENTER_FRAME,this.onClick,false,0,true)",
            "this.btnButton.addEventListener(MouseEvent.CLICK,this.onClick(),false,0,true)",
            "this.onClick()",
        ] {
            let class = parse(&source(registration)).unwrap().unwrap().1;
            assert!(class.frames[&1].unsupported.is_some(), "{registration}");
        }
    }

    #[test]
    fn ffdec_identifiers_keep_constructor_and_callback_identity() {
        for name in ["013BlackSkullsScythe", "has space", "s-r/rg", "日本語"] {
            let source = format!("package {{ public class §{name}§ extends MovieClip {{ function §{name}§() {{super(); addFrameScript(0,this.§idle-callback§);}} function §idle-callback§() {{stop();}} }} }}");
            let (parsed, class) = parse(&source).unwrap().unwrap();
            assert_eq!(parsed, name.to_lowercase());
            assert!(class.unsupported.is_none(), "{:?}", class.unsupported);
            assert!(class.constructor.unsupported.is_none());
            assert_eq!(class.frames[&1].commands[0].action, Action::Stop);
        }
        let (name, _) = parse("package §class§ {class §package§ extends MovieClip {function §package§(){super();}}}").unwrap().unwrap();
        assert_eq!(name, "class.package");
        for source in ["class §broken", "class §§pop()", r"class §bad\q§"] {
            assert!(parse(source).is_err(), "{source}");
        }
    }

    #[test]
    fn background_avm1_requires_entire_known_program() {
        let color = r#"try { MovieClip(this.stage.getChildAt(0)).mcSetColor(this,"Trim","None"); } catch(e:Error) {}"#;
        assert!(inert_background_avm1(color).unwrap());
        assert!(inert_background_avm1("var isProp=true; mouseEnabled=false; mouseChildren=false;").unwrap());
        for text in [format!("{color} stop();"), color.replace("mcSetColor", "gotoAndStop"), color.replace("{}", "{play();}"), "_visible=false;".into(), "gotoAndStop(2);".into()] {
            assert!(!inert_background_avm1(&text).unwrap(), "{text}");
        }
    }
    #[test]
    fn background_linkage_accepts_only_literal_metadata() {
        let source = r#"var isProp=true; var strLinkage="BushZ"; mouseEnabled=false; mouseChildren=false;"#;
        for name in ["BushZ", "Another_prop-17", "", "日本語"] {
            assert!(inert_background_avm1(&source.replace("BushZ", name)).unwrap());
        }
        for changed in [
            source.replace("\"BushZ\"", "getLinkage()"),
            source.replace("\"BushZ\"", "other.name"),
            source.replace("\"BushZ\"", "\"Bush\"+\"Z\""),
            source.replace("strLinkage", "_visible"),
            source.replace("mouseEnabled=false", "mouseEnabled=true"),
            format!("{source} stop();"),
        ] {
            assert!(!inert_background_avm1(&changed).unwrap(), "{changed}");
        }
    }
    #[test]
    fn hand_visibility_requires_exact_registered_first_frame_rule() {
        let source = r#"class Clip {function Clip(){addFrameScript(0,this.frame1);}
            function frame1(){if(MovieClip(parent.parent).name == "fronthand"){visible=false;}}}"#;
        assert_eq!(parse(source).unwrap().unwrap().1.hidden_in_hand.as_deref(), Some("fronthand"));
        for invalid in [source.replace("parent.parent", "parent"), source.replace("false", "true"), source.replace("fronthand", "weapon"), source.replace("addFrameScript(0", "addFrameScript(1"), source.replace("visible=false;", "visible=false;play();")] {
            assert!(parse(&invalid).unwrap().unwrap().1.hidden_in_hand.is_none());
        }
    }

    #[test]
    fn multiple_accessors_are_distinct_and_do_not_execute() {
        let source = r#"class Avatar {
            function Avatar(){addFrameScript(0,this.frame1);}
            function get helmName():String{return this.a ? this.b : this.c;}
            function get armorName():String{return this.d;}
            function set helmName(value:String):void{this.b=value;}
            function frame1(){stop();}
        }"#;
        let class = parse(source).unwrap().unwrap().1;
        assert!(class.unsupported.is_none());
        assert_eq!(class.frames[&1].commands[0].action, Action::Stop);
        let accessed = source.replace("function frame1(){stop();}", "function frame1(){var x = this.helmName; stop();}");
        assert!(parse(&accessed).unwrap().unwrap().1.frames[&1].unsupported.is_some());
        assert!(parse("class C {function get name(){return 1;} function get name(){return 2;}}").is_err());
    }

    #[test]
    fn listener_registration_try_block_does_not_call_conditional_handlers() {
        let source = r#"class Armor {
            function Armor(){addFrameScript(1,this.frame2);}
            function onIdle(){if(!this.idleing){this.gotoAndPlay("Idleing");}}
            function frame2(){this.walking=false;this.idleing=false;
                try {MovieClip(parent.parent.parent).addAnimationListener("Idle",this.onIdle,false);
                     MovieClip(parent.parent.parent).addAnimationListener("Walk",this.onIdle,true);}
                catch(e:*) {} stop();}
        }"#;
        let class = parse(source).unwrap().unwrap().1;
        assert!(class.frames[&2].unsupported.is_none());
        assert_eq!(class.frames[&2].commands, vec![Command{child:None,action:Action::Stop}]);
        for invalid in [
            source.replace("catch(e:*) {}", "catch(e:*) {gotoAndPlay(9);}"),
            source.replace("this.onIdle,false", "this.onIdle(),false"),
            source.replace("try {MovieClip", "try {stop(); MovieClip"),
            source.replace("stop();}", "if(x) stop();}"),
            source.replace("parent.parent.parent", "getParent()"),
        ] {
            assert!(parse(&invalid).unwrap().unwrap().1.frames[&2].unsupported.is_some());
        }
    }

    #[test]
    fn bank_idle_preserves_stop_and_rejects_rendering_side_effects() {
        let fixture = |body: &str, helper: &str, extra: &str| {
            parse(&format!("class C {{function C(){{addFrameScript(7,this.idle,27,this.walk);}} function idle(){{{body}}} function initPet(){{{helper}}} function walk(){{if(this.onMove){{gotoAndPlay(\"Walk\");}}}} {extra}}}"))
                .unwrap().unwrap().1
        };
        for (body, helper) in [
            (
                BANK_IDLE.replace("this.", ""),
                BANK_INIT.replace("this.", ""),
            ),
            (
                BANK_IDLE.replace("this.petInit", "petInit"),
                BANK_INIT.replace("this.avatar", "avatar"),
            ),
        ] {
            let c = fixture(&body, &helper, "");
            assert!(c.frames[&8].unsupported.is_none());
            assert_eq!(
                c.frames[&8].commands,
                vec![Command {
                    child: None,
                    action: Action::Stop
                }]
            );
        }
        let c = fixture(BANK_IDLE, BANK_INIT, "");
        assert!(c.frames[&8].unsupported.is_none());
        assert_eq!(
            c.frames[&8].commands,
            vec![Command {
                child: None,
                action: Action::Stop
            }]
        );
        assert!(c.frames[&28].unsupported.is_some());
        for helper in [
            BANK_INIT.replace("buttonMode = true", "visible = false"),
            BANK_INIT.replace("this.avatar = MovieClip(parent).pAV;", "gotoAndPlay(29);"),
            BANK_INIT.replace("catch(e:Error) {}", "catch(e:Error) { return; }"),
        ] {
            assert!(fixture(BANK_IDLE, &helper, "").frames[&8]
                .unsupported
                .is_some());
        }
        for body in [
            BANK_IDLE.replace("!this.petInit", "this.petInit"),
            BANK_IDLE.replace("this.petInit", "other.petInit"),
            BANK_IDLE.replace("this.initPet", "other.this.initPet"),
            BANK_IDLE.replace("this.initPet();", "this.initPet();this.visible=false;"),
            "if(!this.petInit){this.petInit=true;this.initPet();stop();}".into(),
        ] {
            assert!(fixture(&body, BANK_INIT, "").frames[&8]
                .unsupported
                .is_some());
        }
        for name in [
            "petInit",
            "rootClass",
            "avatar",
            "btnBank",
            "onBankClick",
            "stage",
            "parent",
        ] {
            for kind in ["get", "set"] {
                let extra = format!("function {kind} {name}(){{gotoAndPlay(29);}}");
                assert!(
                    fixture(BANK_IDLE, BANK_INIT, &extra).frames[&8]
                        .unsupported
                        .is_some(),
                    "{kind} {name}"
                );
            }
        }
    }
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
