//! Give independently placed MovieClips independent export definitions.
//! Reusing a DefineSprite is an authoring optimization, not shared runtime state.
use crate::{
    model::SymbolRequest,
    script::Class,
    swf::{self, Swf},
};
use anyhow::{ensure, Context, Result};
use std::collections::{BTreeMap, BTreeSet};

pub struct Isolated {
    pub bytes: Vec<u8>,
    pub swf: Swf,
    pub requests: Vec<SymbolRequest>,
    pub scripts: BTreeMap<String, Class>,
    pub names: BTreeMap<u16, Vec<String>>,
    pub aliases: BTreeMap<String, String>,
}
struct Builder<'a> {
    swf: &'a Swf,
    scripts: BTreeMap<String, Class>,
    used: BTreeSet<u16>,
    claimed: BTreeSet<u16>,
    active: BTreeSet<u16>,
    sprites: BTreeMap<u16, Vec<u8>>,
    symbols: Vec<(u16, String)>,
    names: BTreeMap<u16, Vec<String>>,
    aliases: BTreeMap<String, String>,
    budget: usize,
}
impl Builder<'_> {
    fn visit(&mut self, id: u16, names: Vec<String>) -> Result<u16> {
        let Some(payload) = self.swf.sprites.get(&id) else {
            return Ok(id);
        };
        ensure!(
            self.active.len() < 64 && self.active.insert(id),
            "cyclic/deep sprite hierarchy"
        );
        self.budget = self
            .budget
            .checked_sub(payload.len())
            .context("instance expansion exceeds 128 MiB")?;
        let copy = if self.claimed.insert(id) {
            id
        } else {
            let n = (1..=u16::MAX)
                .rev()
                .find(|n| !self.used.contains(n))
                .context("no free SWF character IDs")?;
            self.used.insert(n);
            n
        };
        self.names.insert(copy, names.clone());
        if copy != id {
            if let Some((_, old)) = self.swf.symbols.iter().find(|(n, _)| *n == id) {
                let name = format!("{old}__render_instance_{copy}");
                self.symbols.push((copy, name.clone()));
                self.aliases.insert(name.to_lowercase(), old.to_lowercase());
                if let Some(class) = self.scripts.get(&old.to_lowercase()).cloned() {
                    self.scripts.insert(name.to_lowercase(), class);
                }
            }
        }
        let mut out = copy.to_le_bytes().to_vec();
        out.extend_from_slice(&payload[2..4]);
        for (code, data) in swf::tags(payload, 4)? {
            let mut data = data.to_vec();
            if let Some((depth, Some(child), name, _moving)) = swf::instance(code, &data)? {
                let child_copy = {
                    let mut child_names = vec![name.unwrap_or_else(|| format!("instance{depth}"))];
                    child_names.extend(names.clone());
                    self.visit(child, child_names)?
                };
                swf::set_placed_id(code, &mut data, child_copy)?;
            }
            swf::write_tag(&mut out, code, &data);
        }
        self.sprites.insert(copy, out);
        self.active.remove(&id);
        Ok(copy)
    }
}
pub fn isolate(
    source: &[u8],
    swf: &Swf,
    scripts: &BTreeMap<String, Class>,
    requests: &[SymbolRequest],
) -> Result<Isolated> {
    let body = swf::decompress(source)?;
    let offset = (5 + 4 * (body[0] as usize >> 3)).div_ceil(8) + 4;
    // A conservative superset also reserves IDs of shapes, buttons, images, fonts, etc.
    let used = swf::tags(&body, offset)?
        .iter()
        .filter_map(|(_, d)| swf::u16_at(d, 0).ok())
        .collect();
    let mut b = Builder {
        swf,
        scripts: scripts.clone(),
        used,
        claimed: BTreeSet::new(),
        active: BTreeSet::new(),
        sprites: BTreeMap::new(),
        symbols: swf.symbols.clone(),
        names: BTreeMap::new(),
        aliases: BTreeMap::new(),
        budget: 128 * 1024 * 1024,
    };
    let mut requests = requests.to_vec();
    for r in &mut requests {
        let mut names = vec!["asset".into()];
        names.extend(if r.ancestor_names.is_empty() {
            vec![r.key.clone(), "mcChar".into(), "stage".into()]
        } else {
            r.ancestor_names.clone()
        });
        r.character_id = b.visit(r.character_id, names)?;
        if let Some((_, name)) = b.symbols.iter().find(|(id, _)| *id == r.character_id) {
            r.class_name = name.clone();
        }
    }
    let bytes = swf::replace_dictionary(source, &b.sprites, &b.symbols)?;
    let swf = Swf::parse(&bytes)?;
    Ok(Isolated {
        bytes,
        swf,
        requests,
        scripts: b.scripts,
        names: b.names,
        aliases: b.aliases,
    })
}
