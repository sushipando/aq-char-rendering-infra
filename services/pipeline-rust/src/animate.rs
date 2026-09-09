//! Verified static subset of Adobe Animate advanced-layer output.
//! No VM: prove generated callbacks cannot change the authored visible result.
use crate::{
    script::{AnimateLayer, Class},
    swf::{self, Swf},
};
use anyhow::{ensure, Context, Result};
use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
};

fn simplify_frame_setters(
    runtime: &crate::script_eval::RuntimeClass,
) -> Option<(crate::script_eval::RuntimeClass, usize)> {
    use crate::script::{close, lex, Token};
    let mut simple = runtime.clone();
    let mut limit = usize::MAX;
    let mut setters = Vec::new();
    for (name, method) in &runtime.methods {
        if !name.starts_with("__setProp_") || name == "__setProp_handler" {
            continue;
        }
        let body = &method.body;
        let end = close(body, 1, '(', ')').ok()?;
        let stop = close(body, end + 1, '{', '}').ok()?;
        if stop + 1 != body.len() {
            return None;
        }
        let child = body.get(4)?.word()?;
        let param = method.parameters.first()?.first()?.word()?;
        let cap = body.windows(4).find_map(|w| {
            (w[0].word() == Some(param) && w[1] == Token::Punct('<') && w[2] == Token::Punct('='))
                .then(|| w[3].word()?.parse::<usize>().ok())
                .flatten()
        })?;
        let guard=format!("if(this.{child}!=null&&{param}>=1&&{param}<={cap}&&(this.__setPropDict[this.{child}]==undefined||!(int(this.__setPropDict[this.{child}])>=1&&int(this.__setPropDict[this.{child}])<={cap})))");
        if body[..=end] != lex(&guard).ok()? {
            return None;
        }
        let assignment = lex(&format!("this.__setPropDict[this.{child}]={param};")).ok()?;
        if !body[end + 2..stop].starts_with(&assignment) {
            return None;
        }
        let simplified = simple.methods.get_mut(name)?;
        simplified.parameters.clear();
        simplified.body = body[end + 2 + assignment.len()..stop].to_vec();
        limit = limit.min(cap);
        setters.push(name.clone());
    }
    if setters.is_empty() {
        return None;
    }
    let handler = runtime.methods.get("__setProp_handler")?;
    let prefix=lex("var _loc2_:int=currentFrame;if(this.__lastFrameProp==_loc2_){return;}this.__lastFrameProp=_loc2_;").ok()?;
    if !handler.body.starts_with(&prefix) {
        return None;
    }
    let calls = &handler.body[prefix.len()..];
    let mut seen = Vec::new();
    for stmt in calls
        .split(|t| *t == Token::Punct(';'))
        .filter(|s| !s.is_empty())
    {
        let name = stmt.get(2)?.word()?;
        if !setters.contains(&name.to_string())
            || stmt != lex(&format!("this.{name}(_loc2_)")).ok()?
        {
            return None;
        }
        seen.push(name.to_string());
    }
    if seen.len() != setters.len() {
        return None;
    }
    let registration =
        lex("addEventListener(Event.FRAME_CONSTRUCTED,this.__setProp_handler,false,0,true);")
            .ok()?;
    let ctor = simple.methods.get_mut(&runtime.name)?;
    let at = ctor
        .body
        .windows(registration.len())
        .position(|w| w == registration)?;
    let mut calls = Vec::new();
    for name in seen {
        calls.extend(lex(&format!("this.{name}();")).ok()?);
    }
    ctor.body.splice(at..at + registration.len(), calls);
    simple.methods.remove("__setProp_handler");
    simple.fields.retain(|f| {
        !matches!(
            f.first().and_then(Token::word),
            Some("__setPropDict" | "__lastFrameProp")
        )
    });
    Some((simple, limit))
}
fn residual_frame(runtime: &crate::script_eval::RuntimeClass) -> Vec<crate::script::Token> {
    let mut body = runtime
        .methods
        .get("frame1")
        .map(|m| m.body.clone())
        .unwrap_or_default();
    for pattern in [
        "this.___applyLayerZdepthAndEffects___();",
        "addEventListener(Event.ADDED,this.___onAdded___);",
        "this.parentFirstFrame=0;",
    ] {
        let tokens = crate::script::lex(pattern).unwrap();
        while let Some(at) = body.windows(tokens.len()).position(|w| w == tokens) {
            body.drain(at..at + tokens.len());
        }
    }
    body
}
fn sanitized(class: &Class) -> Class {
    let generated = matches!(
        class.animate_layer,
        Some(
            AnimateLayer::Properties
                | AnimateLayer::Controller { .. }
                | AnimateLayer::Pairs { .. }
                | AnimateLayer::Graphic { .. }
        )
    );
    if !generated {
        return class.clone();
    }
    let mut result = Class {
        synchronized_children: bindings(class.animate_layer.as_ref()),
        graphic_children: match &class.animate_layer {
            Some(AnimateLayer::Graphic { children }) => children.clone(),
            Some(AnimateLayer::Pairs { graphics, .. }) => graphics.clone(),
            _ => vec![],
        },
        ..Class::default()
    };
    if !matches!(class.animate_layer, Some(AnimateLayer::Properties)) {
        if let Some(runtime) = &class.runtime {
            let residual = residual_frame(runtime);
            if !residual.is_empty() {
                let mut retained = crate::script_eval::RuntimeClass::new(&runtime.name);
                retained.fields = runtime.fields.clone();
                retained.frames.insert(1, "frame1".into());
                retained.methods.insert(
                    runtime.name.clone(),
                    crate::script_eval::Method {
                        parameters: vec![],
                        body: crate::script::lex("addFrameScript(0,this.frame1);").unwrap(),
                    },
                );
                retained.methods.insert(
                    "frame1".into(),
                    crate::script_eval::Method {
                        parameters: vec![],
                        body: residual,
                    },
                );
                result.runtime = Some(retained);
                result.unsupported = Some("custom frame callback beside generated layers".into());
            }
        }
    }
    result
}
/// Recognize generated controllers by complete method semantics, independent of
/// the number/names of layers. SWF placement validation remains a separate proof.
pub(crate) fn classify_generated(
    runtime: &crate::script_eval::RuntimeClass,
) -> Option<AnimateLayer> {
    use crate::script::Token;
    use crate::script_eval::{ContextData, Evaluator, State, Value};
    if runtime.methods.contains_key("__setProp_handler") {
        let (simple, limit) = simplify_frame_setters(runtime)?;
        let kind = classify_generated(&simple)?;
        return Some(match kind {
            AnimateLayer::Pairs {
                pairs,
                graphics,
                masks,
                ..
            } => AnimateLayer::Pairs {
                pairs,
                graphics,
                masks,
                frame_limit: Some(limit),
            },
            _ => return None,
        });
    }
    let setters: Vec<_> = runtime
        .methods
        .keys()
        .filter(|n| n.starts_with("__setProp_"))
        .cloned()
        .collect();
    if setters.is_empty() {
        return None;
    }
    for field in &runtime.fields {
        if let Some(at) = field.iter().position(|t| *t == Token::Punct('=')) {
            let value = &field[at + 1..];
            if !matches!(
                value,
                [Token::Word(_)] | [Token::String(_)] | [Token::Punct('-'), Token::Word(_)]
            ) {
                return None;
            }
        }
    }
    let controller = runtime
        .methods
        .contains_key("___applyLayerZdepthAndEffects___");
    let children: Vec<_> = runtime
        .fields
        .iter()
        .filter_map(|f| f.first().and_then(Token::word).map(str::to_owned))
        .collect();
    let context = ContextData {
        children,
        frame: 1,
        total_frames: 1,
        ..Default::default()
    };
    let mut state = State::default();
    for setter in &setters {
        let mut eval = Evaluator::new(runtime, &mut state, &context);
        eval.method(setter, vec![]).ok()?;
        if !eval.commands.is_empty() || !state.listeners.is_empty() {
            return None;
        }
    }
    if controller {
        static TEMPLATE: std::sync::OnceLock<crate::script_eval::RuntimeClass> =
            std::sync::OnceLock::new();
        let template = TEMPLATE.get_or_init(|| {
            crate::script::parse(include_str!("../assets/animate/controller-flat-layer.as"))
                .unwrap()
                .unwrap()
                .1
                .runtime
                .unwrap()
        });
        for (name, method) in &runtime.methods {
            if name == &runtime.name || setters.contains(name) || name == "frame1" {
                continue;
            }
            if name == "syncFrame" {
                if method.body
                    != crate::script::lex(include_str!("../assets/animate/sync-frame.as")).ok()?
                {
                    return None;
                }
            } else if template
                .methods
                .get(name)
                .is_none_or(|m| m.body != method.body)
            {
                return None;
            }
        }
        // Only generated startup calls and the one frame callback may execute.
        for (name, method) in &runtime.methods {
            if name != &runtime.name {
                continue;
            }
            for stmt in method
                .body
                .split(|t| *t == Token::Punct(';'))
                .filter(|s| !s.is_empty())
            {
                let spelling = stmt
                    .iter()
                    .map(|t| match t {
                        Token::Word(s) | Token::Identifier(s) => s.clone(),
                        Token::String(s) => format!("\"{s}\""),
                        Token::Punct(c) => c.to_string(),
                    })
                    .collect::<String>();
                if matches!(
                    spelling.as_str(),
                    "super()"
                        | "addFrameScript(0,this.frame1)"
                        | "this.___applyLayerZdepthAndEffects___()"
                        | "addEventListener(Event.ADDED,this.___onAdded___)"
                        | "this.parentFirstFrame=0"
                ) {
                    continue;
                }
                if setters.iter().any(|n| spelling == format!("this.{n}()")) {
                    continue;
                }
                return None;
            }
        }
        let mut pairs = Vec::new();
        let mut masks = BTreeMap::new();
        for (key, value) in &state.values {
            if let Some(prop) = key.strip_suffix(".isAttachedToMask") {
                if *value == Value::Bool(true) {
                    let Value::String(mask) = state.values.get(&format!("{prop}.maskLayerName"))?
                    else {
                        return None;
                    };
                    if mask.is_empty()
                        || state.values.get(&format!("{mask}.containerType"))
                            != Some(&Value::Number(1.))
                    {
                        return None;
                    }
                    masks.insert(prop.strip_suffix("_prop_")?.to_string(), mask.clone());
                }
            }
        }
        for (key, value) in &state.values {
            let (child, field) = key.rsplit_once('.')?;
            let valid = match field {
                "componentInspectorSetting" => *value == Value::Bool(false),
                "containerType" => matches!(value,Value::Number(n) if *n==1.||*n==2.),
                "isAttachedToCamera" => *value == Value::Bool(false),
                "isAttachedToMask" => matches!(value, Value::Bool(_)),
                "layerDepth" => *value == Value::Number(0.),
                "layerIndex" => matches!(value,Value::Number(n) if *n>=0.&&n.fract()==0.),
                "maskLayerName" => {
                    matches!(value,Value::String(v) if v.is_empty()||masks.values().any(|m|m==v))
                }
                "firstFrame" => *value == Value::Number(1.),
                "lastFrame" | "loopMode" => *value == Value::Number(0.),
                _ => false,
            };
            if !valid {
                return None;
            }
            if field == "containerType" && *value == Value::Number(1.) {
                let object = child.strip_suffix("_prop_")?;
                if state.values.get(&format!("{object}.containerType")) != Some(&Value::Number(2.))
                {
                    if masks.values().any(|m| m == child) {
                        continue;
                    }
                    return None;
                }
                pairs.push((object.into(), child.into()));
            }
        }
        if pairs.is_empty() {
            return None;
        }
        Some(AnimateLayer::Pairs {
            pairs,
            graphics: state
                .values
                .keys()
                .filter_map(|k| k.strip_suffix(".loopMode").map(str::to_owned))
                .collect(),
            frame_limit: None,
            masks,
        })
    } else {
        // Default Graphic looping is neutral only after the SWF proof establishes
        // the controlled clip has one frame. Other ranges/modes need synchronization.
        if runtime.methods.len() != setters.len() + 1 {
            return None;
        }
        let ctor = runtime.methods.get(&runtime.name)?;
        for stmt in ctor
            .body
            .split(|t| *t == Token::Punct(';'))
            .filter(|s| !s.is_empty())
        {
            if stmt == crate::script::lex("super()").ok()? {
                continue;
            }
            if !setters
                .iter()
                .any(|n| crate::script::lex(&format!("this.{n}()")).ok().as_deref() == Some(stmt))
            {
                return None;
            }
        }
        for (key, value) in &state.values {
            let (_, field) = key.rsplit_once('.')?;
            if !match field {
                "componentInspectorSetting" => *value == Value::Bool(false),
                "firstFrame" => *value == Value::Number(1.),
                "lastFrame" | "loopMode" => *value == Value::Number(0.),
                _ => false,
            } {
                return None;
            }
        }
        Some(AnimateLayer::Graphic {
            children: state
                .values
                .keys()
                .filter_map(|k| k.strip_suffix(".loopMode").map(str::to_owned))
                .collect(),
        })
    }
}

/// Bake generated flat-layer effects and carry explicit clock bindings into
/// timeline resolution. Exact runtime recognition excludes camera/depth/masks.
pub(crate) fn prepare(
    source: &[u8],
    swf: &Swf,
    scripts: &BTreeMap<String, Class>,
) -> Result<(Vec<u8>, BTreeMap<String, Class>)> {
    if !scripts.values().any(|c|matches!(&c.animate_layer,Some(AnimateLayer::Pairs{frame_limit,masks,..}) if frame_limit.is_some()||!masks.is_empty())) {if let Ok(validated)=validated_scripts(source,swf,scripts){return Ok((source.to_vec(),validated.into_owned()));}}
    ensure!(
        scripts.values().all(|c| c.animate_layer.is_some()
            || c.runtime
                .as_ref()
                .is_none_or(|r| !r.methods.keys().any(|n| n == "executeFrame"
                    || n == "syncFrame"
                    || n == "___applyLayerZdepthAndEffects___"))),
        "unrecognized generated layer runtime: {}",
        scripts
            .iter()
            .filter(|(_, c)| c.animate_layer.is_none()
                && c.runtime.as_ref().is_some_and(|r| r
                    .methods
                    .keys()
                    .any(|n| n == "executeFrame"
                        || n == "syncFrame"
                        || n == "___applyLayerZdepthAndEffects___")))
            .map(|(n, _)| n.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    let kinds: BTreeMap<_, _> = swf
        .symbols
        .iter()
        .filter_map(|(id, n)| {
            scripts
                .get(&n.to_lowercase())
                .and_then(|c| c.animate_layer.as_ref())
                .map(|k| (*id, k))
        })
        .collect();
    let mut replacements = BTreeMap::new();
    let body = swf::decompress(source)?;
    let offset = (5 + 4 * (body[0] as usize >> 3)).div_ceil(8) + 4;
    // Stage-owned cameras/properties must be wrapped into an explicit controller.
    for (code, data) in swf::tags(&body, offset)? {
        if let Some((_, id, name, _)) = swf::instance(code, data)? {
            ensure!(
                name.as_deref() != Some("___camera___instance")
                    && !id
                        .is_some_and(|id| matches!(kinds.get(&id), Some(AnimateLayer::Properties))),
                "unbound stage layer/camera"
            );
        }
    }
    for (id, payload) in &swf.sprites {
        let kind = kinds.get(id).copied();
        let tags = swf::tags(payload, 4)?;
        ensure!(
            tags.iter().filter(|(c, _)| *c == 1).count() == swf::u16_at(payload, 2)? as usize,
            "sprite frame count mismatch"
        );
        if matches!(kind, Some(AnimateLayer::Properties)) {
            ensure!(
                tags.iter()
                    .all(|(code, data)| matches!(code, 0 | 1) && data.is_empty()),
                "property clip contains artwork/actions"
            );
            continue;
        }
        let pairs = pairs(kind);
        if pairs.is_empty() {
            for (code, data) in tags {
                if let Some((_, child, name, _)) = swf::instance(code, data)? {
                    ensure!(
                        name.as_deref() != Some("___camera___instance")
                            && !child.is_some_and(|id| matches!(
                                kinds.get(&id),
                                Some(AnimateLayer::Properties)
                            )),
                        "unbound layer property/camera"
                    );
                }
            }
            continue;
        }
        let masks = if let Some(AnimateLayer::Pairs { masks, .. }) = kind {
            masks.clone()
        } else {
            BTreeMap::new()
        };
        let mut frames = crate::display::snapshots(payload)?;
        let mut configured = BTreeSet::new();
        for (index, frame) in frames.iter_mut().enumerate() {
            let mut names = BTreeMap::new();
            for (depth, p) in frame.iter() {
                if let Some(name) = p.name() {
                    ensure!(
                        name != "___camera___instance" && names.insert(name, *depth).is_none(),
                        "camera/mask/duplicate layer name"
                    );
                }
            }
            for (object, prop) in &pairs {
                let (Some(&target), Some(&property)) = (names.get(object), names.get(prop)) else {
                    ensure!(
                        !names.contains_key(object) && !names.contains_key(prop),
                        "incomplete layer target/property pair"
                    );
                    continue;
                };
                if let Some(AnimateLayer::Pairs {
                    frame_limit: Some(limit),
                    ..
                }) = kind
                {
                    for depth in [target, property] {
                        let placement = &frame[&depth];
                        let identity = (placement.id, placement.generation);
                        if configured.insert(identity) {
                            ensure!(
                                index < *limit,
                                "generated object first appears outside its setter range"
                            );
                        }
                    }
                }
                ensure!(
                    target != property
                        && matches!(
                            kinds.get(&frame[&property].id),
                            Some(AnimateLayer::Properties)
                        ),
                    "invalid layer binding"
                );
                let property = frame[&property].clone();
                frame.get_mut(&target).unwrap().apply_flat_layer(&property);
                if let Some(mask_prop) = masks.get(object) {
                    let mask_depth = names
                        .get(mask_prop)
                        .context("missing generated mask properties")?;
                    ensure!(
                        frame[mask_depth].identity_color()?,
                        "generated mask color needs composition"
                    );
                }
            }
            for p in frame
                .values()
                .filter(|p| matches!(kinds.get(&p.id), Some(AnimateLayer::Properties)))
            {
                ensure!(
                    p.name()
                        .is_some_and(|name| pairs.iter().any(|(_, prop)| *prop == name)
                            || masks.values().any(|m| *m == name)),
                    "unbound property placement"
                );
            }
        }
        replacements.insert(*id, crate::display::write_frames(*id, &frames)?);
    }
    let mut mask_targets = BTreeSet::new();
    for (id, kind) in &kinds {
        if let AnimateLayer::Pairs { masks, .. } = kind {
            for frame in crate::display::snapshots(&swf.sprites[id])? {
                for object in masks.keys() {
                    if let Some(target) =
                        frame.values().find(|p| p.name().as_deref() == Some(object))
                    {
                        mask_targets.insert(target.id);
                    }
                }
            }
        }
    }
    for id in &mask_targets {
        let payload = replacements.get(id).unwrap_or(&swf.sprites[id]);
        let mut frames = crate::display::snapshots(payload)?;
        for frame in &mut frames {
            for (depth, p) in frame {
                if p.name().as_deref() == Some("_mask_obj_instance") {
                    ensure!(
                        p.masks_following(*depth)?,
                        "generated mask has no authored clipping range"
                    );
                    p.reset_matrix();
                }
            }
        }
        replacements.insert(*id, crate::display::write_frames(*id, &frames)?);
    }
    let mut scripts: BTreeMap<String, Class> = scripts
        .iter()
        .map(|(name, c)| (name.clone(), sanitized(c)))
        .collect();
    let mut symbols = swf.symbols.clone();
    for id in mask_targets {
        let name = swf
            .symbols
            .iter()
            .find(|(n, _)| *n == id)
            .map(|(_, name)| name.to_lowercase())
            .unwrap_or_else(|| {
                let name = format!("AqwGeneratedMaskOwner{id}");
                symbols.push((id, name.clone()));
                name.to_lowercase()
            });
        scripts
            .entry(name)
            .or_default()
            .synchronized_children
            .push("_mask_obj_instance".into());
    }
    Ok((
        swf::replace_dictionary(source, &replacements, &symbols)?,
        scripts,
    ))
}

pub fn validated_scripts<'a>(
    source: &[u8],
    swf: &Swf,
    scripts: &'a BTreeMap<String, Class>,
) -> Result<Cow<'a, BTreeMap<String, Class>>> {
    if !scripts.values().any(|c| {
        matches!(
            c.animate_layer,
            Some(
                AnimateLayer::Properties
                    | AnimateLayer::Controller { .. }
                    | AnimateLayer::Pairs { .. }
            )
        )
    }) {
        return Ok(Cow::Borrowed(scripts));
    }
    ensure!(
        scripts.values().all(|c| c.animate_layer.is_some()
            || c.runtime
                .as_ref()
                .is_none_or(|r| !r.methods.keys().any(|n| n == "executeFrame"
                    || n == "syncFrame"
                    || n == "___applyLayerZdepthAndEffects___"))),
        "Animate layers: unrecognized generated runtime"
    );
    let body = swf::decompress(source)?;
    let offset = (5 + 4 * (body[0] as usize >> 3)).div_ceil(8) + 4;
    ensure!(
        swf::u16_at(&body, offset - 2)? == 1,
        "Animate layers: animated stage requires frame synchronization"
    );
    let kinds: BTreeMap<_, _> = swf
        .symbols
        .iter()
        .filter_map(|(id, name)| {
            scripts
                .get(&name.to_lowercase())
                .and_then(|c| c.animate_layer.as_ref().map(|kind| (*id, kind)))
        })
        .collect();
    // Validate the stage too: document-level placement can introduce a camera,
    // a duplicate instance, or a property clip outside a recognized controller.
    validate_container(&swf::tags(&body, offset)?, None, &kinds)?;
    for (id, payload) in &swf.sprites {
        // A malformed declared frame count is not an animation compatibility case.
        ensure!(
            swf::tags(payload, 4)?
                .iter()
                .filter(|(code, _)| *code == 1)
                .count()
                == swf::u16_at(payload, 2)? as usize,
            "sprite frame count mismatch"
        );
        let generated = matches!(
            kinds.get(id),
            Some(
                AnimateLayer::Properties
                    | AnimateLayer::Controller { .. }
                    | AnimateLayer::Pairs { .. }
                    | AnimateLayer::Graphic { .. }
            )
        );
        if !generated {
            for (code, data) in swf::tags(payload, 4)? {
                if let Some((_, Some(child), _, _)) = swf::instance(code, data)? {
                    ensure!(
                        !matches!(kinds.get(&child), Some(AnimateLayer::Properties)),
                        "property clip under unrecognized controller"
                    );
                }
            }
            continue;
        }
        let tags = swf::tags(payload, 4)?;
        if matches!(kinds.get(id), Some(AnimateLayer::Properties)) {
            ensure!(
                tags.iter()
                    .all(|(code, data)| (*code == 0 || *code == 1) && data.is_empty()),
                "Animate layers: property clip {id} contains artwork or actions"
            );
        }
        if matches!(kinds.get(id), Some(AnimateLayer::Graphic { .. })) {
            ensure!(
                swf::u16_at(payload, 2)? == 1,
                "graphic wrapper requires frame synchronization"
            );
            for &(code, data) in &tags {
                if let Some((_, Some(child), _, _)) = swf::instance(code, data)? {
                    if let Some(payload) = swf.sprites.get(&child) {
                        ensure!(
                            swf::u16_at(payload, 2)? == 1,
                            "graphic child requires frame synchronization"
                        );
                    }
                }
            }
        }
        if swf::u16_at(payload, 2)? == 1 {
            validate_container(&tags, kinds.get(id).copied(), &kinds)
                .with_context(|| format!("Animate layer sprite {id}"))?;
        } else {
            let frames = crate::display::snapshots(payload)?;
            let expected: BTreeSet<_> = bindings(kinds.get(id).copied()).into_iter().collect();
            for frame in frames {
                let named: BTreeMap<_, _> = frame
                    .values()
                    .filter_map(|p| p.name().map(|n| (n, p)))
                    .collect();
                ensure!(
                    named.len() == frame.len(),
                    "unnamed animated layer placement"
                );
                for (object, prop) in pairs(kinds.get(id).copied()) {
                    let a = named
                        .get(&object)
                        .context("missing animated layer target")?;
                    let b = named
                        .get(&prop)
                        .context("missing animated layer properties")?;
                    ensure!(
                        a.identity_matrix() && b.identity_matrix() && a.effects() == b.effects(),
                        "animated layer transform/effects differ"
                    );
                }
                let actual: BTreeSet<_> = named
                    .iter()
                    .filter(|(_, p)| matches!(kinds.get(&p.id), Some(AnimateLayer::Properties)))
                    .map(|(n, _)| n.to_string())
                    .collect();
                ensure!(
                    actual.iter().all(|n| expected.contains(n)),
                    "unbound animated properties"
                );
            }
        }
    }
    Ok(Cow::Owned(
        scripts
            .iter()
            .map(|(name, class)| (name.clone(), sanitized(class)))
            .collect(),
    ))
}

fn pairs(kind: Option<&AnimateLayer>) -> Vec<(String, String)> {
    match kind {
        Some(AnimateLayer::Controller { pair: Some(pair) }) => vec![pair.clone()],
        Some(AnimateLayer::Pairs { pairs, .. }) => pairs.clone(),
        _ => vec![],
    }
}
fn bindings(kind: Option<&AnimateLayer>) -> Vec<String> {
    if let Some(AnimateLayer::Graphic { children }) = kind {
        return children.clone();
    }
    let mut names: Vec<_> = pairs(kind).into_iter().flat_map(|(a, b)| [a, b]).collect();
    if let Some(AnimateLayer::Pairs { graphics, .. }) = kind {
        names.extend(graphics.iter().cloned());
    }
    names
}

fn validate_container(
    tags: &[(u16, &[u8])],
    kind: Option<&AnimateLayer>,
    kinds: &BTreeMap<u16, &AnimateLayer>,
) -> Result<()> {
    let mut names = BTreeMap::new();
    let mut depths = BTreeSet::new();
    let mut properties = BTreeSet::new();
    let mut shown = false;
    for &(code, data) in tags {
        ensure!(
            !matches!(code, 5 | 12 | 28 | 59),
            "Animate layers: removal/action is unsupported"
        );
        if code == 1 {
            ensure!(!shown, "Animate layers: multiple frames");
            shown = true;
        }
        if let Some((depth, id, name, moving)) = swf::instance(code, data)? {
            ensure!(
                !shown && !moving && depths.insert(depth),
                "Animate layers: changing/duplicate placement depth"
            );
            let id = id.context("Animate layers: missing placed character")?;
            if let Some(name) = name {
                ensure!(
                    name != "___camera___instance" && name != "_mask_obj_instance",
                    "Animate layers: camera/mask requires runtime support"
                );
                ensure!(
                    names.insert(name.clone(), (id, code, data)).is_none(),
                    "Animate layers: duplicate instance name"
                );
                if matches!(kinds.get(&id), Some(AnimateLayer::Properties)) {
                    properties.insert(name);
                }
            } else {
                ensure!(
                    !matches!(kinds.get(&id), Some(AnimateLayer::Properties)),
                    "Animate layers: unnamed property clip"
                );
            }
        }
    }
    if let Some(AnimateLayer::Pairs { pairs, .. }) = kind {
        ensure!(
            properties == pairs.iter().map(|(_, p)| p.clone()).collect(),
            "Animate layer bindings mismatch"
        );
        for (object, prop) in pairs {
            let &(target, code, data) = names.get(object).context("missing target layer")?;
            let &(_, pcode, pdata) = names.get(prop).context("missing property layer")?;
            ensure!(
                !matches!(kinds.get(&target), Some(AnimateLayer::Properties)),
                "property is its own target"
            );
            ensure!(
                swf::flat_layer_style(code, data)? == swf::flat_layer_style(pcode, pdata)?,
                "layer effects differ"
            );
        }
        return Ok(());
    }
    match kind {
        Some(AnimateLayer::Controller {
            pair: Some((object, prop)),
        }) => {
            ensure!(
                properties == BTreeSet::from([prop.clone()]),
                "Animate layers: property binding mismatch"
            );
            let &(target, code, data) = names
                .get(object)
                .context("Animate layers: missing target layer")?;
            ensure!(
                !matches!(kinds.get(&target), Some(AnimateLayer::Properties)),
                "Animate layers: property cannot be its own target"
            );
            let &(_, pcode, pdata) = names
                .get(prop)
                .context("Animate layers: missing property layer")?;
            ensure!(
                swf::flat_layer_style(code, data)? == swf::flat_layer_style(pcode, pdata)?,
                "Animate layers: property and target effects differ"
            );
        }
        _ => ensure!(
            properties.is_empty(),
            "Animate layers: property clip without recognized controller"
        ),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::ScriptMetadata;
    fn fixture(controller: &str, runtime: &str) -> (Vec<u8>, Swf, BTreeMap<String, Class>) {
        let mut metadata = ScriptMetadata::default();
        metadata.inspect(controller).unwrap();
        metadata.inspect(runtime).unwrap();
        let mut body = vec![0, 0, 24, 1, 0];
        for id in 1u16..=3 {
            let mut payload = Vec::new();
            payload.extend(id.to_le_bytes());
            payload.extend(1u16.to_le_bytes());
            if id == 1 {
                for (depth, child, name) in [(1u16, 3u16, "scrolling"), (2, 2, "scrolling_prop_")] {
                    let mut data = vec![0x26];
                    data.extend(depth.to_le_bytes());
                    data.extend(child.to_le_bytes());
                    data.push(0);
                    data.extend(name.as_bytes());
                    data.push(0);
                    swf::write_tag(&mut payload, 26, &data);
                }
            }
            swf::write_tag(&mut payload, 1, &[]);
            swf::write_tag(&mut payload, 0, &[]);
            swf::write_tag(&mut body, 39, &payload);
        }
        let mut symbols = vec![2, 0, 1, 0];
        symbols.extend(b"BoAEnergy_fla.Symbol11_3\0");
        symbols.extend([2, 0]);
        symbols.extend(b"privatePkg.___LayerProp___\0");
        swf::write_tag(&mut body, 76, &symbols);
        swf::write_tag(&mut body, 1, &[]);
        swf::write_tag(&mut body, 0, &[]);
        let mut bytes = b"FWS\x0a".to_vec();
        bytes.extend(((body.len() + 8) as u32).to_le_bytes());
        bytes.extend(body);
        let parsed = Swf::parse(&bytes).unwrap();
        (bytes, parsed, metadata.timelines)
    }
    const CONTROLLER: &str = include_str!("../assets/animate/controller-flat-layer.as");
    const RUNTIME: &str = include_str!("../assets/animate/layer-runtime.as");

    #[test]
    fn exact_generated_code_requires_neutral_static_placements() {
        let (bytes, swf, scripts) = fixture(CONTROLLER, RUNTIME);
        let accepted = validated_scripts(&bytes, &swf, &scripts).unwrap();
        assert!(accepted
            .values()
            .all(|c| c.unsupported.is_none() && c.constructor.unsupported.is_none()));
        let mut animated = swf.sprites.clone();
        animated.get_mut(&3).unwrap()[2] = 2;
        let changed = swf::replace_sprites(&bytes, &animated).unwrap();
        assert!(validated_scripts(&changed, &Swf::parse(&changed).unwrap(), &scripts).is_err());
        let mut wrong_matrix = swf.sprites.clone();
        // First PlaceObject2 header (2 bytes), flags/depth/id (5 bytes), matrix.
        wrong_matrix.get_mut(&1).unwrap()[11] = 2;
        let changed = swf::replace_sprites(&bytes, &wrong_matrix).unwrap();
        assert!(validated_scripts(&changed, &Swf::parse(&changed).unwrap(), &scripts).is_err());
        let mut artwork = swf.sprites.clone();
        artwork.insert(2, swf.sprites[&1].clone());
        artwork.get_mut(&2).unwrap()[0] = 2;
        let changed = swf::replace_sprites(&bytes, &artwork).unwrap();
        assert!(validated_scripts(&changed, &Swf::parse(&changed).unwrap(), &scripts).is_err());
    }

    #[test]
    fn mismatched_effects_camera_and_duplicate_names_are_rejected() {
        let controller = AnimateLayer::Controller {
            pair: Some(("scrolling".into(), "scrolling_prop_".into())),
        };
        let property = AnimateLayer::Properties;
        let kinds = BTreeMap::from([(2, &property)]);
        let placement = |depth: u16, id: u16, name: &str, blend: u8| {
            let mut data = vec![0x26, 2];
            data.extend(depth.to_le_bytes());
            data.extend(id.to_le_bytes());
            data.push(0);
            data.extend(name.as_bytes());
            data.push(0);
            data.push(blend);
            data
        };
        let target = placement(1, 3, "scrolling", 1);
        let prop = placement(2, 2, "scrolling_prop_", 1);
        assert!(
            validate_container(&[(70, &target), (70, &prop)], Some(&controller), &kinds).is_ok()
        );
        let different = placement(2, 2, "scrolling_prop_", 2);
        assert!(validate_container(
            &[(70, &target), (70, &different)],
            Some(&controller),
            &kinds
        )
        .is_err());
        let duplicate = placement(3, 3, "scrolling", 1);
        assert!(validate_container(
            &[(70, &target), (70, &prop), (70, &duplicate)],
            Some(&controller),
            &kinds
        )
        .is_err());
        let camera = placement(3, 3, "___camera___instance", 1);
        assert!(validate_container(
            &[(70, &target), (70, &prop), (70, &camera)],
            Some(&controller),
            &kinds
        )
        .is_err());
    }

    #[test]
    fn runtime_changes_and_nonzero_layer_depth_are_not_ignored() {
        for (controller, runtime) in [
            (
                CONTROLLER.replace("layerDepth = 0", "layerDepth = 50"),
                RUNTIME.to_owned(),
            ),
            (
                CONTROLLER.to_owned(),
                RUNTIME.replace("visible = false", "visible = true"),
            ),
        ] {
            let (bytes, swf, scripts) = fixture(&controller, &runtime);
            // A recognized companion forces strict validation of the entire family.
            assert!(validated_scripts(&bytes, &swf, &scripts).is_err());
        }
    }

    #[test]
    fn generated_residual_callback_keeps_escaped_class_names() {
        let changed = CONTROLLER
            .replace("Symbol11_3", "§class with spaces§")
            .replace(
                "this.___applyLayerZdepthAndEffects___();",
                "stop(); this.___applyLayerZdepthAndEffects___();",
            );
        let (_, class) = crate::script::parse(&changed).unwrap().unwrap();
        let preserved = sanitized(&class);
        assert_eq!(preserved.runtime.unwrap().name, "class with spaces");
    }
    #[test]
    fn custom_stop_beside_generated_scaffolding_is_preserved() {
        let changed = CONTROLLER.replace(
            "this.___applyLayerZdepthAndEffects___();",
            "stop(); this.___applyLayerZdepthAndEffects___();",
        );
        let (bytes, swf, scripts) = fixture(&changed, RUNTIME);
        let validated = validated_scripts(&bytes, &swf, &scripts).unwrap();
        let runtime = validated.values().find_map(|c| c.runtime.as_ref()).unwrap();
        let mut state = crate::script_eval::State::default();
        let context = crate::script_eval::ContextData {
            frame: 1,
            total_frames: 1,
            ..Default::default()
        };
        let mut eval = crate::script_eval::Evaluator::new(runtime, &mut state, &context);
        eval.initialize().unwrap();
        eval.frame().unwrap();
        assert_eq!(eval.commands[0].action, crate::script::Action::Stop);
    }
    #[test]
    fn generated_class_and_layer_names_are_not_asset_specific() {
        let source = CONTROLLER
            .replace("Symbol11_3", "RenamedController")
            .replace("scrolling", "energy");
        let mut metadata = ScriptMetadata::default();
        metadata.inspect(&source).unwrap();
        assert_eq!(
            metadata.timelines["boaenergy_fla.renamedcontroller"].animate_layer,
            Some(AnimateLayer::Controller {
                pair: Some(("energy".into(), "energy_prop_".into()))
            })
        );
    }
}
