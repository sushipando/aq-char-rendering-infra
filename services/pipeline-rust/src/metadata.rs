//! Public render metadata only: no request destinations, tokens, or AWS details.
use anyhow::{ensure, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub const POLICY: &str = "aqw-xmp-v2";

/// Match actual asset selection, including overrides, base-items and visibility.
/// Resolve has already applied show_hidden and item overrides to these fields.
pub fn display(fields: &Value, settings: &Value) -> Result<Value> {
    let fields: BTreeMap<String, String> = serde_json::from_value(if fields.is_null() {
        json!({})
    } else {
        fields.clone()
    })?;
    let get = |key: &str| fields.get(key).map(String::as_str).unwrap_or("");
    let first = |keys: &[&str]| {
        keys.iter()
            .map(|key| get(key))
            .find(|v| !v.is_empty())
            .unwrap_or("")
            .to_owned()
    };
    let cosmetics = settings["base_items"] != true;
    let assets = crate::resolve::appearance(&fields, cosmetics)?;
    let mut items = Vec::new();
    let class = get("strClassName");
    if !class.is_empty() {
        items.push(json!({"slot":"Class","name":class}));
    }
    for (slot, label, title, base) in [
        ("armor", "Armor", "Armor", "Armor"),
        ("helm", "Helm", "Helm", "Helm"),
        ("cape", "Cape", "Cape", "Cape"),
        ("weapon", "Weapon", "Weapon", "Weapon"),
        ("pet", "Pet", "Pet", "Pet"),
        ("ground", "Rune", "Misc", "Misc"),
        ("hair", "Hair", "Hair", "Hair"),
    ] {
        if !assets.contains_key(slot) {
            continue;
        }
        let custom = format!("strCust{title}Name");
        let base = format!("str{base}Name");
        let mut name = if cosmetics
            && !matches!(slot, "pet" | "ground" | "hair")
            && fields.contains_key(&custom)
        {
            get(&custom).to_owned()
        } else {
            get(&base).to_owned()
        };
        if name.is_empty() && slot == "armor" {
            name = class.to_owned();
        }
        if name.is_empty() && slot == "ground" {
            name = first(&["strGroundName"]);
        }
        // Keep nameless assets identifiable rather than silently omitting them.
        if name.is_empty() {
            name = assets[slot].remote_path.clone();
        }
        items.push(json!({"slot":label,"name":name}));
    }
    let mut colors = BTreeMap::new();
    for (label, key) in [
        ("hair", "intColorHair"),
        ("skin", "intColorSkin"),
        ("eye", "intColorEye"),
        ("base", "intColorBase"),
        ("trim", "intColorTrim"),
        ("accessory", "intColorAccessory"),
    ] {
        if let Ok(n) = get(key).parse::<u32>() {
            if n <= 0xffffff {
                colors.insert(label, format!("#{n:06X}"));
            }
        }
    }
    Ok(
        json!({"name":first(&["strName"]),"class":class,"level":first(&["level","intLevel","strLevel"]),
        "guild":first(&["guild","strGuildName","strGuild"]),"shown_items":items,"colors":colors}),
    )
}

fn escape(s: &str) -> String {
    s.chars()
        .filter(|c| matches!(*c, '\t' | '\n' | '\r') || *c >= '\u{20}')
        .collect::<String>()
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

pub fn packet(prepared: &Value) -> Result<Vec<u8>> {
    let data = display(&prepared["fields"], &prepared["settings"])?;
    let mut out = String::from("<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\"><rdf:Description rdf:about=\"\" xmlns:aqw=\"http://aqw.char/info/1.0/\">");
    for tag in ["name", "class", "level", "guild"] {
        out.push_str(&format!(
            "<aqw:{tag}>{}</aqw:{tag}>",
            escape(data[tag].as_str().unwrap_or(""))
        ));
    }
    for (tag, key) in [("jobId", "job_id"), ("renderHash", "render_hash")] {
        out.push_str(&format!(
            "<aqw:{tag}>{}</aqw:{tag}>",
            escape(prepared[key].as_str().unwrap_or(""))
        ));
    }
    out.push_str("<aqw:items><rdf:Seq>");
    for item in data["shown_items"].as_array().unwrap() {
        out.push_str(&format!(
            "<rdf:li><aqw:slot>{}</aqw:slot><aqw:name>{}</aqw:name></rdf:li>",
            escape(item["slot"].as_str().unwrap()),
            escape(item["name"].as_str().unwrap())
        ));
    }
    out.push_str("</rdf:Seq></aqw:items><aqw:colors><rdf:Seq>");
    for (label, value) in data["colors"].as_object().unwrap() {
        out.push_str(&format!(
            "<rdf:li><aqw:label>{label}</aqw:label><aqw:value>{}</aqw:value></rdf:li>",
            value.as_str().unwrap()
        ));
    }
    out.push_str("</rdf:Seq></aqw:colors></rdf:Description></rdf:RDF></x:xmpmeta>");
    ensure!(out.len() <= 65536, "render XMP exceeds 64 KiB");
    Ok(out.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn respects_rendered_items_visibility_and_base_selection() {
        let mut fields = json!({"strName":"A & B","strClassName":"Mage","strArmorName":"Base armor","strClassFile":"base.swf",
            "strCustArmorName":"Cosmetic","strCustArmorFile":"custom.swf","strHelmName":"Hat","strHelmFile":"hat.swf",
            "strHairName":"Bob","strHairFile":"hair.swf","strMiscName":"Rune","strMiscFile":"rune.swf","ia1":"2",
            "level":"42","guild":"Guild","intColorHair":"16711680"});
        let data = display(&fields, &json!({})).unwrap();
        assert!(data["shown_items"]
            .as_array()
            .unwrap()
            .contains(&json!({"slot":"Armor","name":"Cosmetic"})));
        assert!(data["shown_items"]
            .as_array()
            .unwrap()
            .contains(&json!({"slot":"Hair","name":"Bob"})));
        assert!(!data["shown_items"].to_string().contains("Hat"));
        let base = display(&fields, &json!({"base_items":true})).unwrap();
        assert!(base["shown_items"].to_string().contains("Base armor"));
        fields["ia1"] = "0".into();
        let shown = display(&fields, &json!({})).unwrap();
        assert!(shown["shown_items"].to_string().contains("Hat"));
        assert!(!shown["shown_items"].to_string().contains("Bob"));
        let xml = String::from_utf8(
            packet(&json!({"fields":fields,"settings":{},"job_id":"job-123","render_hash":"hash"}))
                .unwrap(),
        )
        .unwrap();
        assert!(xml.contains("A &amp; B"));
        assert!(xml.contains("<aqw:jobId>job-123</aqw:jobId>"));
        assert!(xml.contains("#FF0000"));
        assert!(xml.contains("<aqw:slot>Rune</aqw:slot>"));
    }
}
