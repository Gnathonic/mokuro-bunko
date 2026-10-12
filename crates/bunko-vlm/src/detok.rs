//! Token ids → text, reading a Hugging Face `tokenizer.json`, decode only.
//!
//! Reproduces `tokenizers`' `decode(ids, skip_special_tokens=True)` for the two decoder
//! chains these models use (spec §5.10, §6.10): an id maps to its added-token content or
//! its vocab entry; tokens whose content is a `special: true` added token are dropped;
//! the rest go through the decoder chain.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde_json::Value;

use crate::VlmError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Chain {
    /// GPT-2 `ByteLevel` (hayai-nova).
    ByteLevel,
    /// `Replace("▁" → " ")` → `ByteFallback` → `Fuse` (PaddleOCR-VL / ERNIE).
    SentencePiece,
}

/// A decode-only tokenizer.
#[derive(Debug)]
pub struct Detokenizer {
    id2tok: Vec<Option<String>>,
    special: HashSet<String>,
    chain: Chain,
    byte_of: HashMap<char, u8>,
}

impl Detokenizer {
    pub fn from_file(path: &Path) -> Result<Self, VlmError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| VlmError::Io(path.display().to_string(), e))?;
        Self::from_json(&text).map_err(|why| VlmError::Asset(format!("{}: {why}", path.display())))
    }

    pub fn from_json(text: &str) -> Result<Self, String> {
        let t: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
        let chain = detect_chain(&t["decoder"])?;
        let vocab = t["model"]["vocab"]
            .as_object()
            .ok_or("model.vocab is not an object")?;
        let mut id2tok: Vec<Option<String>> = Vec::new();
        let mut put = |id: usize, s: &str| {
            if id2tok.len() <= id {
                id2tok.resize(id + 1, None);
            }
            id2tok[id] = Some(s.to_owned());
        };
        for (tok, id) in vocab {
            put(
                id.as_u64().ok_or("vocab id is not an integer")? as usize,
                tok,
            );
        }
        let mut special = HashSet::new();
        for a in t["added_tokens"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            let id = a["id"].as_u64().ok_or("added token without id")? as usize;
            let content = a["content"].as_str().ok_or("added token without content")?;
            put(id, content);
            if a["special"].as_bool().unwrap_or(false) {
                special.insert(content.to_owned());
            }
        }
        Ok(Self {
            id2tok,
            special,
            chain,
            byte_of: byte_decoder(),
        })
    }

    /// Number of ids that are dropped as special.
    pub fn special_count(&self) -> usize {
        self.special.len()
    }

    /// `decode(ids, skip_special_tokens=True)` (no strip, no normalisation).
    pub fn decode(&self, ids: &[u32]) -> String {
        let toks: Vec<&str> = ids
            .iter()
            .filter_map(|&id| self.id2tok.get(id as usize).and_then(Option::as_deref))
            .filter(|t| !self.special.contains(*t))
            .collect();
        match self.chain {
            Chain::ByteLevel => {
                let mut bytes = Vec::new();
                for t in toks {
                    let mapped: Option<Vec<u8>> =
                        t.chars().map(|c| self.byte_of.get(&c).copied()).collect();
                    match mapped {
                        Some(b) => bytes.extend(b),
                        None => bytes.extend_from_slice(t.as_bytes()),
                    }
                }
                String::from_utf8_lossy(&bytes).into_owned()
            }
            Chain::SentencePiece => {
                let mut out = String::new();
                let mut pending: Vec<u8> = Vec::new();
                let flush = |pending: &mut Vec<u8>, out: &mut String| {
                    if pending.is_empty() {
                        return;
                    }
                    match std::str::from_utf8(pending) {
                        Ok(s) => out.push_str(s),
                        Err(_) => out.extend(std::iter::repeat_n('\u{fffd}', pending.len())),
                    }
                    pending.clear();
                };
                for t in toks {
                    let t = t.replace('\u{2581}', " ");
                    if let Some(b) = byte_token(&t) {
                        pending.push(b);
                    } else {
                        flush(&mut pending, &mut out);
                        out.push_str(&t);
                    }
                }
                flush(&mut pending, &mut out);
                out
            }
        }
    }
}

/// `<0xHH>` → the byte (tokenizers' `ByteFallback`).
fn byte_token(t: &str) -> Option<u8> {
    if t.len() == 6 && t.starts_with("<0x") && t.ends_with('>') {
        u8::from_str_radix(&t[3..5], 16).ok()
    } else {
        None
    }
}

fn detect_chain(d: &Value) -> Result<Chain, String> {
    match d["type"].as_str() {
        Some("ByteLevel") => Ok(Chain::ByteLevel),
        Some("Sequence") => {
            let parts: Vec<&str> = d["decoders"]
                .as_array()
                .map(|a| a.iter().filter_map(|x| x["type"].as_str()).collect())
                .unwrap_or_default();
            let replace_ok = d["decoders"][0]["pattern"]["String"].as_str() == Some("\u{2581}")
                && d["decoders"][0]["content"].as_str() == Some(" ");
            if parts == ["Replace", "ByteFallback", "Fuse"] && replace_ok {
                Ok(Chain::SentencePiece)
            } else {
                Err(format!("unsupported decoder sequence {parts:?}"))
            }
        }
        other => Err(format!("unsupported decoder {other:?}")),
    }
}

/// GPT-2's byte decoder: printable bytes map to themselves, the other 68 to U+0100+n.
fn byte_decoder() -> HashMap<char, u8> {
    let mut bs: Vec<u32> = (u32::from(b'!')..=u32::from(b'~'))
        .chain(0xA1..=0xAC)
        .chain(0xAE..=0xFF)
        .collect();
    let mut cs = bs.clone();
    let mut n = 0;
    for b in 0..256u32 {
        if !bs.contains(&b) {
            bs.push(b);
            cs.push(256 + n);
            n += 1;
        }
    }
    bs.into_iter()
        .zip(cs)
        .filter_map(|(b, c)| char::from_u32(c).map(|ch| (ch, b as u8)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const BL: &str = r#"{"decoder":{"type":"ByteLevel"},"model":{"vocab":{"a":0,"Ġb":1,"ãģĤ":2}},
        "added_tokens":[{"id":3,"content":"<eos>","special":true},{"id":4,"content":"<x>","special":false}]}"#;

    #[test]
    fn byte_level() {
        let d = Detokenizer::from_json(BL).unwrap();
        // "ãģĤ" is the byte-level spelling of あ (E3 81 82)
        assert_eq!(d.decode(&[0, 1, 3, 2, 99]), "a bあ");
        assert_eq!(d.decode(&[4]), "<x>");
    }

    const SP: &str = r#"{"decoder":{"type":"Sequence","decoders":[{"type":"Replace","pattern":{"String":"▁"},"content":" "},{"type":"ByteFallback"},{"type":"Fuse"}]},
        "model":{"vocab":{"<unk>":0,"▁hi":1,"<0xE3>":2,"<0x81>":3,"<0x82>":4,"x":5}},
        "added_tokens":[{"id":0,"content":"<unk>","special":true},{"id":6,"content":"<|IMAGE_START|>","special":true},{"id":7,"content":"<|LOC|>","special":false}]}"#;

    #[test]
    fn sentencepiece_byte_fallback() {
        let d = Detokenizer::from_json(SP).unwrap();
        assert_eq!(d.decode(&[1, 2, 3, 4, 5]), " hiあx");
        // an invalid byte run → one U+FFFD per byte
        assert_eq!(d.decode(&[2, 3, 5]), "\u{fffd}\u{fffd}x");
        // only special added tokens are skipped
        assert_eq!(d.decode(&[0, 6, 7, 5]), "<|LOC|>x");
    }
}
