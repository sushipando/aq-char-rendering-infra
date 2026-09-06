use crate::{
    config::Config,
    contract::string,
    geometry,
    model::*,
    store::{self, Store},
    swf::Swf,
};
use anyhow::{ensure, Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SourceObject {
    pub key: String,
    pub sha256: String,
    pub size: usize,
    #[serde(default)]
    pub remote_path: String,
}
#[derive(Clone, Serialize)]
pub struct Asset {
    pub slot: String,
    pub remote_path: String,
    pub link: String,
    pub weapon_type: String,
}

pub fn normalize_path(raw: &str) -> Result<String> {
    let value = raw.trim().replace('\\', "/");
    let value = value
        .split(['?', '#'])
        .next()
        .unwrap()
        .trim()
        .trim_start_matches('/');
    let value = if value.to_lowercase().starts_with("gamefiles/") {
        &value[10..]
    } else {
        value
    };
    ensure!(
        !value.is_empty()
            && !value.contains(':')
            && !value.contains('\0')
            && value.split('/').all(|p| !matches!(p, "" | "." | ".."))
            && value.to_lowercase().ends_with(".swf"),
        "unsafe AQW asset path"
    );
    Ok(value.into())
}

fn field<'a>(fields: &'a BTreeMap<String, String>, name: &str, default: &'a str) -> &'a str {
    fields.get(name).map(String::as_str).unwrap_or(default)
}
fn chosen(
    fields: &BTreeMap<String, String>,
    title: &str,
    base: &str,
    cosmetics: bool,
) -> (String, String) {
    let prefix = if cosmetics && fields.contains_key(&format!("strCust{title}Name")) {
        format!("strCust{title}")
    } else {
        format!("str{base}")
    };
    (
        field(fields, &format!("{prefix}File"), "").into(),
        field(fields, &format!("{prefix}Link"), "").into(),
    )
}

pub fn appearance(
    fields: &BTreeMap<String, String>,
    cosmetics: bool,
) -> Result<BTreeMap<String, Asset>> {
    let gender = field(fields, "strGender", "M").to_uppercase();
    ensure!(matches!(gender.as_str(), "M" | "F"), "unsupported gender");
    let flags = field(fields, "ia1", "0").parse::<u32>().unwrap_or(0);
    let mut result = BTreeMap::new();
    for (slot, title, base) in [
        ("armor", "Armor", "Class"),
        ("weapon", "Weapon", "Weapon"),
        ("helm", "Helm", "Helm"),
        ("cape", "Cape", "Cape"),
        ("ground", "Misc", "Misc"),
        ("pet", "Pet", "Pet"),
    ] {
        let (mut file, link) = chosen(
            fields,
            title,
            base,
            cosmetics && !matches!(slot, "ground" | "pet"),
        );
        if file.is_empty()
            || file.eq_ignore_ascii_case("none")
            || (slot == "helm" && flags & 2 != 0)
            || (slot == "cape" && flags & 1 != 0)
            || (slot == "pet" && flags & 4 != 0)
        {
            continue;
        }
        if slot == "armor" {
            file = file.replace('\\', "/").trim_start_matches('/').into();
            if !file.to_lowercase().starts_with("classes/") {
                file = format!("classes/{gender}/{file}");
            }
        }
        let weapon_type = if slot == "weapon" {
            field(
                fields,
                if cosmetics && fields.contains_key("strCustWeaponName") {
                    "strCustWeaponType"
                } else {
                    "strWeaponType"
                },
                "Sword",
            )
            .to_owned()
        } else {
            String::new()
        };
        result.insert(
            slot.into(),
            Asset {
                slot: slot.into(),
                remote_path: normalize_path(&file)?,
                link,
                weapon_type: if slot == "weapon" && weapon_type.is_empty() {
                    "Sword".into()
                } else {
                    weapon_type
                },
            },
        );
    }
    if !result.contains_key("helm") {
        let file = field(fields, "strHairFile", "");
        let name = field(fields, "strHairName", "");
        if !file.is_empty()
            && !file.eq_ignore_ascii_case("none")
            && !name.is_empty()
            && !name.eq_ignore_ascii_case("blank")
        {
            result.insert(
                "hair".into(),
                Asset {
                    slot: "hair".into(),
                    remote_path: normalize_path(file)?,
                    link: format!("{name}{gender}Hair"),
                    weapon_type: String::new(),
                },
            );
        }
    }
    Ok(result)
}

pub fn parse_flashvars(text: &str) -> Result<BTreeMap<String, String>> {
    let fields: BTreeMap<_, _> =
        form_urlencoded::parse(text.trim().trim_start_matches('&').as_bytes())
            .into_owned()
            .collect();
    ensure!(
        fields.contains_key("strName"),
        "AQW returned no public character data"
    );
    Ok(fields)
}

fn page_flashvars(page: &str) -> Result<Option<String>> {
    let attrs = Regex::new(r#"(?is)([\w-]+)\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))"#)?;
    for tag in Regex::new(r"(?is)<(?:param|embed)\b[^>]*>")?.find_iter(page) {
        let attributes: BTreeMap<_, _> = attrs
            .captures_iter(tag.as_str())
            .map(|c| {
                (
                    c[1].to_lowercase(),
                    html_escape::decode_html_entities(
                        c.get(2)
                            .or_else(|| c.get(3))
                            .or_else(|| c.get(4))
                            .unwrap()
                            .as_str(),
                    )
                    .into_owned(),
                )
            })
            .collect();
        if attributes
            .get("name")
            .is_some_and(|v| v.eq_ignore_ascii_case("flashvars"))
        {
            return Ok(attributes.get("value").cloned());
        }
        if let Some(value) = attributes.get("flashvars") {
            return Ok(Some(value.clone()));
        }
    }
    Ok(None)
}

async fn fetch_fields(
    client: &reqwest::Client,
    username: &str,
) -> Result<BTreeMap<String, String>> {
    let response = client
        .get("https://account.aq.com/CharPage")
        .query(&[("id", username)])
        .send()
        .await?;
    if response.status().is_success() {
        if let Some(encoded) = page_flashvars(&response.text().await?)? {
            return parse_flashvars(&encoded);
        }
    } else {
        ensure!(
            [403, 404].contains(&response.status().as_u16()),
            "character page request failed: {}",
            response.status()
        );
    }
    let text = client
        .get("https://game.aq.com/game/api/charpage/fvars")
        .query(&[("id", username)])
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    parse_flashvars(&text)
}

async fn source(
    store: &dyn Store,
    config: &Config,
    catalog: &BTreeMap<String, SourceObject>,
    remote: &str,
    client: &reqwest::Client,
) -> Result<(SourceObject, Vec<u8>)> {
    let remote = normalize_path(remote)?;
    if let Some(record) = catalog.get(&remote.to_lowercase()) {
        let bytes = download(store, &config.source_bucket, record).await?;
        return Ok((record.clone(), bytes));
    }
    ensure!(
        config.official_fallback,
        "asset absent from immutable dataset: {remote}"
    );
    let hash = crate::sha256(remote.to_lowercase().as_bytes());
    let key = format!(
        "dynamic-assets/{}/{}/{}.swf",
        config.dataset_version,
        &hash[..2],
        hash
    );
    let bytes = if let Some(bytes) = store.get(&config.source_bucket, &key).await? {
        bytes
    } else {
        let mut url = reqwest::Url::parse("https://game.aq.com/game/gamefiles/")?;
        url.path_segments_mut()
            .map_err(|_| anyhow::anyhow!("invalid asset base URL"))?
            .pop_if_empty()
            .extend(remote.split('/'));
        let bytes = client
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?
            .to_vec();
        Swf::parse(&bytes)?;
        store
            .put(
                &config.source_bucket,
                &key,
                bytes,
                "application/x-shockwave-flash",
                true,
            )
            .await?;
        store
            .get(&config.source_bucket, &key)
            .await?
            .context("dynamic source publish missing")?
    };
    Ok((
        SourceObject {
            key,
            sha256: crate::sha256(&bytes),
            size: bytes.len(),
            remote_path: remote,
        },
        bytes,
    ))
}

async fn download(store: &dyn Store, bucket: &str, record: &SourceObject) -> Result<Vec<u8>> {
    let bytes = store
        .get(bucket, &record.key)
        .await?
        .context("missing source object")?;
    ensure!(
        bytes.len() == record.size && crate::sha256(&bytes) == record.sha256,
        "source checksum/size mismatch"
    );
    Ok(bytes)
}

// difflib.SequenceMatcher's no-junk matching-block ratio for the short ASCII
// name tokens used by the reference override resolver (no autojunk applies).
fn name_similarity(a: &str, b: &str) -> f64 {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let mut pending = vec![(0, a.len(), 0, b.len())];
    let mut matched = 0;
    while let Some((alo, ahi, blo, bhi)) = pending.pop() {
        let mut previous = vec![0; b.len() + 1];
        let (mut ai, mut bj, mut length) = (alo, blo, 0);
        for (i, byte) in a.iter().enumerate().take(ahi).skip(alo) {
            let mut next = vec![0; b.len() + 1];
            for j in blo..bhi {
                if *byte == b[j] {
                    next[j + 1] = previous[j] + 1;
                    if next[j + 1] > length {
                        length = next[j + 1];
                        ai = i + 1 - length;
                        bj = j + 1 - length;
                    }
                }
            }
            previous = next;
        }
        if length > 0 {
            matched += length;
            if alo < ai && blo < bj {
                pending.push((alo, ai, blo, bj));
            }
            if ai + length < ahi && bj + length < bhi {
                pending.push((ai + length, ahi, bj + length, bhi));
            }
        }
    }
    2.0 * matched as f64 / (a.len() + b.len()) as f64
}

fn infer_link(swf: &Swf, remote: &str, slot: &str, gender: &str) -> Result<String> {
    let names: BTreeSet<_> = swf
        .symbols
        .iter()
        .map(|(_, n)| n.clone())
        .filter(|n| !n.is_empty())
        .collect();
    let stem = remote
        .rsplit('/')
        .next()
        .unwrap_or("")
        .trim_end_matches(".swf");
    let revision = Regex::new(r"(?i)(?:[-_]?r\d+)$")?
        .replace(stem, "")
        .into_owned();
    let date = Regex::new(r"-\d{1,2}[A-Za-z]{3}\d{2,4}$")?
        .replace(stem, "")
        .into_owned();
    let variants = [stem.to_owned(), revision.clone(), date];
    if slot == "armor" {
        let suffix = format!("{gender}Chest");
        let roots: BTreeSet<_> = names
            .iter()
            .filter(|name| {
                name.to_lowercase().ends_with(&suffix.to_lowercase()) && name.len() > suffix.len()
            })
            .map(|name| name[..name.len() - suffix.len()].to_owned())
            .collect();
        if roots.len() == 1 {
            return Ok(roots.into_iter().next().unwrap());
        }
        for variant in &variants {
            if let Some(root) = roots.iter().find(|root| root.eq_ignore_ascii_case(variant)) {
                return Ok(root.clone());
            }
        }
        // Rank only complete armor candidates. Ambiguous roots fail explicitly.
        let token = |s: &str| {
            s.to_lowercase()
                .chars()
                .filter(|c| c.is_ascii_alphanumeric())
                .collect::<String>()
        };
        let stem_token = token(&revision);
        let mut ranked: Vec<_> = roots
            .into_iter()
            .map(|root| {
                let rt = token(&root);
                let contains =
                    !rt.is_empty() && (rt.contains(&stem_token) || stem_token.contains(&rt));
                let count = geometry::ARMOR_PARTS
                    .iter()
                    .filter(|(_, suffix, _)| {
                        names
                            .iter()
                            .any(|n| n.eq_ignore_ascii_case(&format!("{root}{gender}{suffix}")))
                    })
                    .count();
                ((contains, count, name_similarity(&stem_token, &rt)), root)
            })
            .collect();
        ranked.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap().then_with(|| b.1.cmp(&a.1)));
        if ranked.first().is_some_and(|r| r.0 .1 >= 8)
            && (ranked.len() == 1 || ranked[0].0 > ranked[1].0)
        {
            return Ok(ranked.remove(0).1);
        }
    }
    for variant in variants {
        if let Some(name) = names.iter().find(|n| n.eq_ignore_ascii_case(&variant)) {
            return Ok(name.clone());
        }
    }
    let docs: Vec<_> = swf.symbols.iter().filter(|(id, _)| *id == 0).collect();
    if docs.len() == 1 {
        return Ok(docs[0].1.clone());
    }
    let roots: Vec<_> = names
        .into_iter()
        .filter(|n| {
            !n.contains('.')
                && !n.contains("::")
                && !n.to_lowercase().ends_with("_backhair")
                && !n.to_lowercase().ends_with("hairback")
        })
        .collect();
    ensure!(
        roots.len() == 1,
        "cannot infer an unambiguous exported link for {remote}"
    );
    Ok(roots[0].clone())
}

fn apply_override(
    fields: &mut BTreeMap<String, String>,
    slot: &str,
    remote: &str,
    link: &str,
    name: &str,
    weapon: &str,
    custom: bool,
) {
    let title = match slot {
        "armor" => "Armor",
        "weapon" => "Weapon",
        "helm" => "Helm",
        "cape" => "Cape",
        _ => "Misc",
    };
    let prefix = if slot == "ground" {
        "strMisc".into()
    } else if custom {
        format!("strCust{title}")
    } else if slot == "armor" {
        "strClass".into()
    } else {
        format!("str{title}")
    };
    fields.insert(format!("{prefix}File"), remote.into());
    fields.insert(format!("{prefix}Link"), link.into());
    fields.insert(
        if slot == "armor" && !custom {
            "strArmorName".into()
        } else {
            format!("{prefix}Name")
        },
        name.into(),
    );
    if slot == "weapon" {
        fields.insert(format!("{prefix}Type"), weapon.into());
    }
    if matches!(slot, "helm" | "cape") {
        let bit = if slot == "helm" { 2 } else { 1 };
        let flags = field(fields, "ia1", "0").parse::<u32>().unwrap_or(0);
        fields.insert("ia1".into(), (flags & !bit).to_string());
    }
}

pub async fn resolve(store: &dyn Store, config: &Config, request: &Value) -> Result<Value> {
    let started = Instant::now();
    let job = string(request, "job_id")?;
    let manifest: Value =
        store::read(store, &config.source_bucket, &config.asset_manifest_key).await?;
    ensure!(
        manifest["schema_version"] == 1 && manifest["dataset_version"] == config.dataset_version,
        "source dataset mismatch"
    );
    let mut catalog = BTreeMap::new();
    for (remote, raw) in manifest["assets"]
        .as_object()
        .context("missing asset catalog")?
    {
        let mut record: SourceObject = serde_json::from_value(raw.clone())?;
        record.remote_path = normalize_path(remote)?;
        ensure!(
            record.sha256.len() == 64 && record.size >= 3 && !record.key.is_empty(),
            "incomplete source record"
        );
        ensure!(
            catalog
                .insert(record.remote_path.to_lowercase(), record)
                .is_none(),
            "duplicate case-insensitive source path"
        );
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(crate::config::number(
            "CHAR_RENDER_OFFICIAL_ASSET_TIMEOUT_SECONDS",
            15,
            1,
            60,
        )? as u64))
        .user_agent("AQWCharacterRenderer/1.0")
        .build()?;
    let settings = &request["render"];
    let cosmetics = !settings["base_items"]
        .as_bool()
        .context("invalid base_items")?;
    let mut fields: BTreeMap<String, String> = if request["appearance"].is_null() {
        fetch_fields(&client, string(settings, "username")?).await?
    } else {
        serde_json::from_value(request["appearance"].clone())?
    };
    if settings["show_hidden"] == true {
        let flags = field(&fields, "ia1", "0").parse::<u32>().unwrap_or(0);
        fields.insert("ia1".into(), (flags & !7).to_string());
    }
    let gender = field(&fields, "strGender", "M").to_uppercase();
    if !settings["override"].is_null() {
        let database: SourceObject = serde_json::from_value(manifest["item_database"].clone())?;
        let items: Vec<Value> =
            serde_json::from_slice(&download(store, &config.source_bucket, &database).await?)?;
        let item = items
            .iter()
            .find(|i| i["id"] == settings["override"]["item_id"])
            .context("override item is absent from dataset")?;
        let database_slot = string(item, "slot")?.to_lowercase();
        let slot = if ["weapon", "gauntlet", "handgun", "rifle", "whip"]
            .contains(&database_slot.as_str())
        {
            "weapon"
        } else {
            database_slot.as_str()
        };
        ensure!(
            ["armor", "weapon", "helm", "cape", "ground"].contains(&slot),
            "unsupported override item slot"
        );
        ensure!(
            settings["override"]["slot"].is_null() || settings["override"]["slot"] == slot,
            "override slot conflicts with item"
        );
        let raw = string(item, "file")?;
        let remote = normalize_path(&if slot == "armor" && !raw.contains('/') {
            format!("classes/{gender}/{raw}")
        } else {
            raw.into()
        })?;
        let (record, bytes) = source(store, config, &catalog, &remote, &client).await?;
        let swf = Swf::parse(&bytes)?;
        let link = infer_link(&swf, &remote, slot, &gender)?;
        let weapon = if database_slot == "gauntlet"
            || remote.to_lowercase().split('/').any(|p| p == "gauntlets")
        {
            "Gauntlet"
        } else if remote.to_lowercase().split('/').any(|p| p == "daggers") {
            "Dagger"
        } else {
            "Sword"
        };
        apply_override(
            &mut fields,
            slot,
            &remote,
            &link,
            item["name"].as_str().unwrap_or("Override"),
            weapon,
            cosmetics,
        );
        catalog.insert(remote.to_lowercase(), record);
    }
    let assets = appearance(&fields, cosmetics)?;
    ensure!(
        assets.contains_key("armor"),
        "character has no visible armor SWF"
    );
    let character: SourceObject = serde_json::from_value(manifest["character_renderer"].clone())?;
    let character_swf = Swf::parse(&download(store, &config.source_bucket, &character).await?)?;
    let mut source_swfs = BTreeMap::new();
    let mut records = BTreeMap::new();
    for (slot, asset) in &assets {
        let (record, bytes) = source(store, config, &catalog, &asset.remote_path, &client).await?;
        source_swfs.insert(slot.clone(), Swf::parse(&bytes)?);
        records.insert(slot.clone(), record);
    }
    let mut requests: Vec<(String, SymbolRequest)> = Vec::new();
    let mut aliases = BTreeMap::new();
    let mut warnings = Vec::new();
    for (logical, suffix, required) in geometry::ARMOR_PARTS {
        let name = format!("{}{gender}{suffix}", assets["armor"].link);
        let found = source_swfs["armor"].symbol(&name);
        let (slot, swf, (id, name)) = if let Some(found) = found {
            ("armor", &source_swfs["armor"], found)
        } else if *logical == "head" {
            let fallback = format!("mcHead{gender}");
            warnings.push(format!("Armor has no {name}; used {fallback}"));
            (
                "character",
                &character_swf,
                character_swf
                    .symbol(&fallback)
                    .context("missing fallback head")?,
            )
        } else {
            ensure!(!required, "armor does not export required class {name}");
            continue;
        };
        let key = format!("armor_{logical}");
        requests.push((
            slot.into(),
            SymbolRequest {
                key: key.clone(),
                class_name: name,
                character_id: id,
                frame: swf.timeline(id)?.0,
                root_timeline_frames: 1,
            },
        ));
        aliases.insert((*logical).to_string(), key);
    }
    for slot in ["weapon", "cape", "helm", "ground", "pet", "hair"] {
        let Some(asset) = assets.get(slot) else {
            continue;
        };
        let swf = &source_swfs[slot];
        let (id, name) = swf
            .symbol(&asset.link)
            .with_context(|| format!("missing exported class {}", asset.link))?;
        let (frame, span) = swf.timeline(id)?;
        requests.push((
            slot.into(),
            SymbolRequest {
                key: slot.into(),
                class_name: name,
                character_id: id,
                frame,
                root_timeline_frames: if slot == "weapon" { span } else { 1 },
            },
        ));
        aliases.insert(slot.into(), slot.into());
        let back = if slot == "helm" {
            Some(format!("{}_backhair", asset.link))
        } else if slot == "hair" {
            Some(format!(
                "{}HairBack",
                asset.link.strip_suffix("Hair").unwrap_or(&asset.link)
            ))
        } else {
            None
        };
        if let Some((id, name)) = back.and_then(|n| swf.symbol(&n)) {
            requests.push((
                slot.into(),
                SymbolRequest {
                    key: "backhair".into(),
                    class_name: name,
                    character_id: id,
                    frame: swf.timeline(id)?.0,
                    root_timeline_frames: 1,
                },
            ));
            aliases.insert("backhair".into(), "backhair".into());
        }
    }
    records.insert("character".into(), character.clone());
    let mut unique: BTreeMap<(String, u16, usize, usize), String> = BTreeMap::new();
    let mut remapped = BTreeMap::new();
    let mut grouped: BTreeMap<String, (SourceObject, Vec<SymbolRequest>)> = BTreeMap::new();
    for (slot, request) in requests {
        let record = &records[&slot];
        let identity = (
            record.key.clone(),
            request.character_id,
            request.frame,
            request.root_timeline_frames,
        );
        if let Some(previous) = unique.get(&identity) {
            remapped.insert(request.key, previous.clone());
        } else {
            unique.insert(identity, request.key.clone());
            grouped
                .entry(record.key.clone())
                .or_insert_with(|| (record.clone(), Vec::new()))
                .1
                .push(request);
        }
    }
    for key in aliases.values_mut() {
        if let Some(replacement) = remapped.get(key) {
            *key = replacement.clone();
        }
    }
    let mut sources = Vec::new();
    for (index, (_, (record, mut requests))) in grouped.into_iter().enumerate() {
        requests.sort_by(|a, b| a.key.cmp(&b.key));
        sources.push(json!({"idx":index,"key":record.key,"sha256":record.sha256,"remote_path":record.remote_path,"requests":requests}));
    }
    let weapon_type = assets
        .get("weapon")
        .map(|a| a.weapon_type.clone())
        .unwrap_or_else(|| "Sword".into());
    let hash = crate::digest(
        &json!({"schema_version":1,"renderer_version":config.renderer_version,"character_renderer_sha256":character.sha256,"ffdec_version":FFDEC_VERSION,"export_policy":EXPORT_POLICY,"finalize_policy":FINALIZE_POLICY,"libwebp_version":"1.5.0","asset_dataset_version":config.dataset_version,"appearance":{"gender":gender,"visibility":fields.get("ia1"),"colors":fields.iter().filter(|(k,_)|k.starts_with("intColor")).collect::<BTreeMap<_,_>>(),"assets":assets,"sources":sources,"override":settings["override"]},"settings":settings,"bounds_policy":BOUNDS_POLICY}),
    )?;
    let hash = crate::digest(&(&hash, aqw_component_raster::region::POLICY))?;
    let quality = settings["webp_quality"]
        .as_f64()
        .context("invalid quality")?
        .to_string()
        .replace('.', "_");
    let final_key = format!(
        "renders/{}/q{quality}/{}/{}/{}.webp",
        config.renderer_version,
        settings["output_size"],
        &hash[..2],
        hash
    );
    if config.render_cache && request["cache"]["render"] == true {
        if let Some(mut result) =
            store::cached::<Value>(store, &config.work_bucket, &format!("{final_key}.json")).await?
        {
            if store.exists(&config.work_bucket, &final_key).await? {
                result["cache_hit"] = true.into();
                return Ok(
                    json!({"schema_version":1,"job_id":job,"cache_hit":true,"render_hash":hash,"final_key":final_key,"result":result}),
                );
            }
        }
    }
    let max = settings["max_frames"]
        .as_u64()
        .context("invalid max_frames")? as usize;
    let complete = settings["complete_loop"] == true;
    let precomputed = if complete && request["cache"]["animation"] == true {
        precomputed_loop(store, &config.source_bucket, &sources, max).await?
    } else {
        None
    };
    let count = if complete {
        precomputed
            .as_ref()
            .and_then(|v| v["frame_count"].as_u64())
            .map(|n| n as usize)
            .unwrap_or(max)
    } else {
        1
    };
    let export_count = if complete { count + 8.min(count) } else { 1 };
    let input_key = format!("jobs/{job}/prepare/input.json");
    store::write(store,&config.work_bucket,&input_key,&json!({"schema_version":1,"job_id":job,"render_hash":hash,"final_key":final_key,"export_frame_count":export_count,"precomputed_loop":precomputed,"fields":fields,"aliases":aliases,"weapon_type":weapon_type,"settings":settings,"bounds_mode":request["bounds_mode"],"component_raster_mode":request["component_raster_mode"],"cache":request["cache"],"warnings":warnings,"character_renderer":character,"frame_rate":character_swf.frame_rate,"sources":sources}),false).await?;
    crate::log(
        "prepare_resolve_profile",
        json!({"job_id":job,"cache_hit":false,"cache":request["cache"],"source_count":sources.len(),"export_frame_count":export_count,"duration_ms":started.elapsed().as_secs_f64()*1000.0}),
    );
    Ok(
        json!({"schema_version":1,"job_id":job,"cache_hit":false,"input_key":input_key,"render_hash":hash,"final_key":final_key,"sources":sources}),
    )
}

async fn precomputed_loop(
    store: &dyn Store,
    source_bucket: &str,
    sources: &[Value],
    max: usize,
) -> Result<Option<Value>> {
    let mut item = 1;
    let mut blink = None;
    let mut static_keys = Vec::new();
    let mut ground = BTreeMap::new();
    for source in sources {
        let key = format!(
            "animation-metadata/2/{FFDEC_VERSION}/{}.json",
            string(source, "sha256")?
        );
        let Some(meta) = store::cached::<Value>(store, source_bucket, &key).await? else {
            return Ok(None);
        };
        // Legacy metadata measured timelines before nested state resolution. Its
        // periods can truncate a corrected export even when all SVG caches miss.
        if meta["export_policy"] != EXPORT_POLICY { return Ok(None); }
        for request in source["requests"]
            .as_array()
            .context("missing symbol requests")?
        {
            let key = string(request, "key")?;
            let symbol = &meta["symbols"][string(request, "class_name")?.to_lowercase()];
            let Some(period) = symbol["period"]
                .as_u64()
                .filter(|n| *n > 0)
                .map(|n| n as usize)
            else {
                return Ok(None);
            };
            let flip = symbol["mirror_flip_frame"].as_u64().unwrap_or(0) as usize;
            if matches!(key, "ground" | "pet") && (symbol["random_pose_as3"] == true || flip > 0) {
                if flip >= 2 {
                    ground.insert(key.to_string(), flip);
                } else {
                    static_keys.push(key.to_string());
                }
                continue;
            }
            if key == "armor_head" {
                blink = Some(period);
            } else {
                let Some(next) = geometry::lcm(item, period) else {
                    return Ok(None);
                };
                item = next;
            }
        }
    }
    let count = blink
        .and_then(|b| b.div_ceil(item).checked_mul(item))
        .unwrap_or(max)
        .min(max);
    Ok(Some(
        json!({"detected_item_loop":item,"detected_blink_frames":blink,"frame_count":count,"source_count":sources.len(),"static_keys":static_keys,"ground_animate":ground}),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn legacy_loop_metadata_cannot_truncate_normalized_timelines() {
        let root = tempfile::tempdir().unwrap();
        let store = crate::store::FsStore(root.path().into());
        let sources = vec![json!({"sha256":"fixture","requests":[{"key":"pet","class_name":"Pet"}]})];
        let key = format!("animation-metadata/2/{FFDEC_VERSION}/fixture.json");
        for policy in [None,Some("rust-effective-svg-v1"),Some(EXPORT_POLICY)] {
            let mut meta = json!({"symbols":{"pet":{"period":37}}});
            if let Some(policy) = policy { meta["export_policy"] = policy.into(); }
            store::write(&store,"source",&key,&meta,false).await.unwrap();
            assert_eq!(precomputed_loop(&store,"source",&sources,120).await.unwrap().is_some(),policy == Some(EXPORT_POLICY));
        }
    }
    #[test]
    fn armor_ranking_matches_sequence_matcher() {
        for (a, b, expected) in [
            ("", "", 1.0),
            ("lovekitty", "lovekitty", 1.0),
            ("lovekitty", "kitty", 10.0 / 14.0),
            ("abcd", "bcde", 0.75),
            ("tide", "diet", 0.25),
            ("diet", "tide", 0.5),
            ("abc", "", 0.0),
        ] {
            assert!(
                (name_similarity(a, b) - expected).abs() < 1e-12,
                "{a} vs {b}"
            );
        }
    }
    #[test]
    fn paths_cannot_escape_asset_origin() {
        assert_eq!(
            normalize_path("/gamefiles/capes/Test.swf?x=1").unwrap(),
            "capes/Test.swf"
        );
        for path in ["../x.swf", "https://evil/x.swf", "foo/../x.swf", "x.png"] {
            assert!(normalize_path(path).is_err());
        }
    }
    #[test]
    fn hidden_helm_selects_hair_and_cosmetic_presence_wins() {
        let fields = BTreeMap::from([
            ("strGender".into(), "F".into()),
            ("strHelmFile".into(), "helm.swf".into()),
            ("strHairFile".into(), "hair.swf".into()),
            ("strHairName".into(), "Test".into()),
            ("ia1".into(), "2".into()),
        ]);
        assert_eq!(appearance(&fields, true).unwrap()["hair"].link, "TestFHair");
    }
}
