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

pub fn validated_scripts<'a>(
    source: &[u8],
    swf: &Swf,
    scripts: &'a BTreeMap<String, Class>,
) -> Result<Cow<'a, BTreeMap<String, Class>>> {
    if !scripts.values().any(|c| {
        matches!(
            c.animate_layer,
            Some(AnimateLayer::Properties | AnimateLayer::Controller { .. })
        )
    }) {
        return Ok(Cow::Borrowed(scripts));
    }
    ensure!(
        scripts.values().all(|c| c.animate_layer.is_some()),
        "Animate layers: unrecognized companion script"
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
                .map(|c| (*id, c.animate_layer.as_ref().unwrap()))
        })
        .collect();
    // Validate the stage too: document-level placement can introduce a camera,
    // a duplicate instance, or a property clip outside a recognized controller.
    validate_container(&swf::tags(&body, offset)?, None, &kinds)?;
    for (id, payload) in &swf.sprites {
        ensure!(
            swf::u16_at(payload, 2)? == 1,
            "Animate layers: sprite {id} requires frame synchronization"
        );
        let tags = swf::tags(payload, 4)?;
        if matches!(kinds.get(id), Some(AnimateLayer::Properties)) {
            ensure!(
                tags.iter()
                    .all(|(code, data)| (*code == 0 || *code == 1) && data.is_empty()),
                "Animate layers: property clip {id} contains artwork or actions"
            );
        }
        validate_container(&tags, kinds.get(id).copied(), &kinds)
            .with_context(|| format!("Animate layer sprite {id}"))?;
    }
    Ok(Cow::Owned(
        scripts
            .iter()
            .map(|(name, _)| (name.clone(), Class::default()))
            .collect(),
    ))
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
            (
                CONTROLLER.replace(
                    "this.___applyLayerZdepthAndEffects___();",
                    "stop(); this.___applyLayerZdepthAndEffects___();",
                ),
                RUNTIME.to_owned(),
            ),
        ] {
            let (bytes, swf, scripts) = fixture(&controller, &runtime);
            // A recognized companion forces strict validation of the entire family.
            assert!(validated_scripts(&bytes, &swf, &scripts).is_err());
        }
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
