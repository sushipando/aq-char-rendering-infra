use std::collections::BTreeMap;

pub const DISPLAY_SCALE: f64 = 2.2980957;
pub const ARMOR_PARTS: &[(&str, &str, bool)] = &[
    ("head", "Head", false),
    ("chest", "Chest", true),
    ("hip", "Hip", true),
    ("idle_foot", "FootIdle", true),
    ("back_foot", "Foot", true),
    ("shoulder", "Shoulder", true),
    ("hand", "Hand", true),
    ("thigh", "Thigh", true),
    ("shin", "Shin", true),
    ("robe", "Robe", false),
    ("back_robe", "RobeBack", false),
];

pub fn compose(a: [f64; 6], b: [f64; 6]) -> [f64; 6] {
    [
        a[0] * b[0] + a[2] * b[1],
        a[1] * b[0] + a[3] * b[1],
        a[0] * b[2] + a[2] * b[3],
        a[1] * b[2] + a[3] * b[3],
        a[0] * b[4] + a[2] * b[5] + a[4],
        a[1] * b[4] + a[3] * b[5] + a[5],
    ]
}

// Exact characterB idle registration matrices from the reference renderer.
fn transform(name: &str) -> [f64; 6] {
    match name {
        "weapon" => [
            0.059173885,
            -0.208672928,
            -0.208758253,
            -0.059147657,
            -10.760687280,
            -47.265744330,
        ],
        "weapon_off" => [
            0.038950285,
            -0.211466225,
            -0.211645155,
            -0.038892447,
            25.176562560,
            -50.366785000,
        ],
        "helm" | "hair" => [
            0.237343166,
            0.0,
            0.0,
            0.237079071,
            5.656329360,
            -100.733687495,
        ],
        "cape" => [
            0.287392009,
            0.006609274,
            0.031190777,
            0.342461126,
            -9.209079000,
            -93.481253670,
        ],
        "head" => [0.237245421, 0.0, 0.0, 0.237079071, 5.656329, -100.733687],
        "chest" => [
            0.266389399,
            0.042586311,
            -0.042616193,
            0.266202614,
            1.752283,
            -76.475547,
        ],
        "hip" => [
            0.270101136,
            0.009417834,
            -0.009424442,
            0.269911749,
            2.653217,
            -60.220092,
        ],
        "front_shoulder" => [
            0.213386460,
            0.161263218,
            -0.161376371,
            0.213236840,
            -10.660584,
            -81.077091,
        ],
        "back_shoulder" => [
            -0.235259722,
            0.053927397,
            0.053965236,
            0.235094765,
            8.659442,
            -78.076084,
        ],
        "front_hand" => [
            -0.258248030,
            0.054080039,
            -0.054117985,
            -0.258082209,
            -17.467639,
            -61.770612,
        ],
        "back_hand" => [
            -0.196996792,
            0.135406161,
            -0.135470619,
            -0.196873928,
            16.267328,
            -57.569202,
        ],
        "front_thigh" => [
            0.260783609,
            0.061208285,
            -0.061251232,
            0.260600754,
            -5.204929,
            -49.466483,
        ],
        "back_thigh" => [
            0.255132001,
            -0.123332405,
            0.066780646,
            0.328006195,
            11.212088,
            -50.566852,
        ],
        "front_shin" => [
            0.265595116,
            0.040739380,
            -0.041531696,
            0.270384938,
            -15.165253,
            -30.910256,
        ],
        "back_shin" => [
            0.255773536,
            -0.085126837,
            0.086790400,
            0.260356542,
            10.661517,
            -28.159332,
        ],
        "idle_foot" => [
            0.206314310,
            0.022392159,
            -0.022407870,
            0.206169648,
            -24.875317,
            -5.501729,
        ],
        "back_foot" => [
            0.239781009,
            0.003709130,
            -0.003711733,
            0.239612881,
            16.918002,
            -7.152283,
        ],
        "robe" => [
            1.000426617,
            0.034908565,
            -0.034933059,
            0.999725145,
            1.752283,
            -61.870646,
        ],
        "back_robe" => [
            1.000502986,
            -0.008715694,
            0.008721809,
            0.999801461,
            -3.953632,
            -66.922341,
        ],
        "gauntlet_front" => [
            0.206598424,
            -0.043264031,
            -0.043294388,
            -0.206465767,
            -17.467639200,
            -61.770611980,
        ],
        "gauntlet_back" => [
            0.157597434,
            -0.108324928,
            -0.108376495,
            -0.157499143,
            16.267327920,
            -57.569202040,
        ],
        "ground" => [1.0010376, 0.0, 0.0, 1.0003357, 0.0, 0.0],
        "pet" => [1.0, 0.0, 0.0, 1.0, -40.0, 10.0],
        "backhair" => compose(
            [1.0010376, 0.0, 0.0, 1.0003357, -0.45, -0.35],
            [
                0.89178467,
                0.10942078,
                -0.10942078,
                0.89178467,
                4.0,
                -114.15,
            ],
        ),
        _ => unreachable!("internal layer name"),
    }
}

pub struct Layer {
    pub name: String,
    pub symbol_key: String,
    pub matrix: [f64; 6],
    pub darken: bool,
}
pub fn layers(aliases: &BTreeMap<String, String>, weapon_type: &str, facing: &str) -> Vec<Layer> {
    let weapon = weapon_type.to_lowercase();
    let mut order = vec![
        ("ground", "ground", false),
        ("backhair", "backhair", false),
        ("cape", "cape", false),
    ];
    if weapon == "dagger" {
        order.push(("weapon_off", "weapon", false));
    }
    order.extend([
        ("back_shoulder", "shoulder", true),
        ("back_hand", "hand", true),
    ]);
    if weapon == "gauntlet" {
        order.push(("gauntlet_back", "weapon", false));
    }
    order.extend([
        ("back_robe", "back_robe", false),
        ("back_foot", "back_foot", true),
        ("back_thigh", "thigh", true),
        ("chest", "chest", false),
        ("hip", "hip", false),
        ("back_shin", "shin", true),
        ("head", "head", false),
        ("hair", "hair", false),
        ("helm", "helm", false),
        ("front_thigh", "thigh", false),
        ("front_shin", "shin", false),
        ("idle_foot", "idle_foot", false),
        ("robe", "robe", false),
    ]);
    if weapon != "gauntlet" {
        order.push(("weapon", "weapon", false));
    }
    order.extend([
        ("front_shoulder", "shoulder", false),
        ("front_hand", "hand", false),
    ]);
    if weapon == "gauntlet" {
        order.push(("gauntlet_front", "weapon", false));
    }
    order.push(("pet", "pet", false));
    let outer = [
        if facing == "left" {
            -DISPLAY_SCALE
        } else {
            DISPLAY_SCALE
        },
        0.0,
        0.0,
        DISPLAY_SCALE,
        0.0,
        0.0,
    ];
    order
        .into_iter()
        .filter_map(|(name, alias, darken)| {
            aliases
                .get(name)
                .or_else(|| aliases.get(alias))
                .map(|key| Layer {
                    name: name.into(),
                    symbol_key: key.clone(),
                    matrix: compose(outer, transform(name)),
                    darken,
                })
        })
        .collect()
}

pub fn pattern<T: Ord>(values: &[T]) -> Vec<usize> {
    let mut states = BTreeMap::new();
    values
        .iter()
        .map(|value| {
            let next = states.len();
            *states.entry(value).or_insert(next)
        })
        .collect()
}

pub fn period<T: PartialEq>(values: &[T], max: usize) -> Option<usize> {
    (1..=max.min(values.len().saturating_sub(1))).find(|p| {
        values.len() - p >= 8.min(*p) && (*p..values.len()).all(|i| values[i] == values[i % p])
    })
}

pub fn lcm(a: usize, b: usize) -> Option<usize> {
    let (mut x, mut y) = (a, b);
    while y != 0 {
        (x, y) = (y, x % y);
    }
    a.checked_div(x).and_then(|q| q.checked_mul(b))
}
pub fn pingpong(index: usize, span: usize) -> usize {
    let position = index % (2 * (span - 1));
    if position < span {
        position
    } else {
        2 * span - position - 2
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validation_tail_protects_cap_boundary() {
        assert_eq!(period(&[1, 2, 3, 1, 2, 3], 3), Some(3));
        assert_eq!(period(&[1, 2, 3, 1], 3), None);
    }
    #[test]
    fn pingpong_does_not_cross_mirror() {
        assert_eq!(
            (0..8).map(|i| pingpong(i, 3)).collect::<Vec<_>>(),
            vec![0, 1, 2, 1, 0, 1, 2, 1]
        );
    }
}
