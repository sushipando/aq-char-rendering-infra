//! Stage-based item assets use the same stage wrapper as backgrounds. Their
//! authored registration coordinates are preserved; no filename/link guesses.
use crate::swf::{self, Swf};
use anyhow::Result;
pub const CLASS: &str = "AqwRenderStageAsset";
pub const POLICY: &str = "stage-item-v1";
pub fn prepare(bytes: &[u8]) -> Result<Option<(Vec<u8>, u16)>> {
    let swf = Swf::parse(bytes)?;
    if swf
        .symbols
        .iter()
        .any(|(id, _)| swf.sprites.contains_key(id))
    {
        return Ok(None);
    }
    let body = swf::decompress(bytes)?;
    let offset = (5 + 4 * (body[0] as usize >> 3)).div_ceil(8) + 4;
    if !swf::tags(&body, offset)?
        .iter()
        .any(|(code, _)| matches!(code, 4 | 26 | 70 | 94))
    {
        return Ok(None);
    }
    let (wrapped, root, _) = crate::background::wrap(bytes)?;
    let swf = Swf::parse(&wrapped)?;
    let mut symbols = swf.symbols.clone();
    symbols.push((root, CLASS.into()));
    Ok(Some((
        swf::replace_dictionary(&wrapped, &Default::default(), &symbols)?,
        root,
    )))
}
